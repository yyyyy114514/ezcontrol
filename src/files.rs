//! File browsing and transfer. Uploads are chunked (each request < 100 MB) so they pass
//! through Cloudflare's request-body limit.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::Query;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use windows::Win32::Storage::FileSystem::GetLogicalDrives;

use crate::util;

type ApiResult<T> = Result<T, (StatusCode, String)>;

fn err(code: StatusCode, msg: impl ToString) -> (StatusCode, String) {
    (code, msg.to_string())
}

fn io_err(e: std::io::Error) -> (StatusCode, String) {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        std::io::ErrorKind::AlreadyExists => StatusCode::CONFLICT,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, e.to_string())
}

fn abs_path(p: &str) -> ApiResult<PathBuf> {
    let pb = PathBuf::from(p);
    if !pb.is_absolute() {
        return Err(err(StatusCode::BAD_REQUEST, "需要绝对路径"));
    }
    Ok(pb)
}

#[derive(Deserialize)]
pub struct PathQ {
    #[serde(default)]
    path: String,
}

#[derive(Serialize)]
struct Entry {
    name: String,
    dir: bool,
    size: u64,
    mtime: u64,
}

#[derive(Serialize)]
pub struct Listing {
    path: String,
    parent: Option<String>,
    entries: Vec<Entry>,
}

pub async fn list(Query(q): Query<PathQ>) -> ApiResult<Json<Listing>> {
    if q.path.is_empty() {
        let mask = unsafe { GetLogicalDrives() };
        let entries = (0..26)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| Entry {
                name: format!("{}:\\", (b'A' + i as u8) as char),
                dir: true,
                size: 0,
                mtime: 0,
            })
            .collect();
        return Ok(Json(Listing { path: String::new(), parent: None, entries }));
    }
    let path = abs_path(&q.path)?;
    let mut rd = tokio::fs::read_dir(&path).await.map_err(io_err)?;
    let mut entries = Vec::new();
    while let Some(e) = rd.next_entry().await.map_err(io_err)? {
        let Ok(md) = e.metadata().await else { continue };
        entries.push(Entry {
            name: e.file_name().to_string_lossy().into_owned(),
            dir: md.is_dir(),
            size: if md.is_dir() { 0 } else { md.len() },
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
    }
    entries.sort_by(|a, b| {
        b.dir
            .cmp(&a.dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let parent = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .or(Some(String::new()));
    Ok(Json(Listing { path: path.to_string_lossy().into_owned(), parent, entries }))
}

fn content_disposition(name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
        .collect();
    let enc = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
    HeaderValue::from_str(&format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{enc}"))
        .unwrap_or(HeaderValue::from_static("attachment"))
}

pub async fn download(Query(q): Query<PathQ>) -> ApiResult<Response> {
    let path = abs_path(&q.path)?;
    let file = tokio::fs::File::open(&path).await.map_err(io_err)?;
    let md = file.metadata().await.map_err(io_err)?;
    if md.is_dir() {
        return Err(err(StatusCode::BAD_REQUEST, "不能直接下载文件夹"));
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    util::log(&format!("下载文件 {}", path.display()));
    let body = Body::from_stream(ReaderStream::with_capacity(file, 256 * 1024));
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(md.len()));
    h.insert(header::CONTENT_DISPOSITION, content_disposition(&name));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

#[derive(Deserialize)]
pub struct UploadQ {
    path: String,
    #[serde(default)]
    offset: u64,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    overwrite: bool,
}

fn part_path(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".ezcpart");
    PathBuf::from(s)
}

pub async fn upload(Query(q): Query<UploadQ>, body: Body) -> ApiResult<&'static str> {
    let path = abs_path(&q.path)?;
    let part = part_path(&path);
    if q.offset == 0 {
        if !q.overwrite && tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Err(err(StatusCode::CONFLICT, "文件已存在"));
        }
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(q.offset == 0)
        .open(&part)
        .await
        .map_err(io_err)?;
    let len = f.metadata().await.map_err(io_err)?.len();
    if len != q.offset {
        return Err(err(StatusCode::CONFLICT, format!("偏移不匹配: 服务端已有 {len} 字节")));
    }
    f.seek(std::io::SeekFrom::Start(q.offset)).await.map_err(io_err)?;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        f.write_all(&chunk).await.map_err(io_err)?;
    }
    f.flush().await.map_err(io_err)?;
    drop(f);
    if q.done {
        if q.overwrite {
            let _ = tokio::fs::remove_file(&path).await;
        }
        tokio::fs::rename(&part, &path).await.map_err(io_err)?;
        util::log(&format!("上传完成 {}", path.display()));
    }
    Ok("ok")
}

#[derive(Deserialize)]
pub struct PathBody {
    path: String,
}

pub async fn mkdir(Json(b): Json<PathBody>) -> ApiResult<&'static str> {
    let p = abs_path(&b.path)?;
    tokio::fs::create_dir_all(&p).await.map_err(io_err)?;
    Ok("ok")
}

pub async fn delete(Json(b): Json<PathBody>) -> ApiResult<&'static str> {
    let p = abs_path(&b.path)?;
    if p.parent().is_none() {
        return Err(err(StatusCode::BAD_REQUEST, "不能删除磁盘根目录"));
    }
    let md = tokio::fs::symlink_metadata(&p).await.map_err(io_err)?;
    if md.is_dir() {
        tokio::fs::remove_dir_all(&p).await.map_err(io_err)?;
    } else {
        tokio::fs::remove_file(&p).await.map_err(io_err)?;
    }
    util::log(&format!("删除 {}", p.display()));
    Ok("ok")
}

#[derive(Deserialize)]
pub struct RenameBody {
    from: String,
    to: String,
}

pub async fn rename(Json(b): Json<RenameBody>) -> ApiResult<&'static str> {
    let from = abs_path(&b.from)?;
    let to = abs_path(&b.to)?;
    if tokio::fs::try_exists(&to).await.unwrap_or(false) {
        return Err(err(StatusCode::CONFLICT, "目标已存在"));
    }
    tokio::fs::rename(&from, &to).await.map_err(io_err)?;
    Ok("ok")
}
