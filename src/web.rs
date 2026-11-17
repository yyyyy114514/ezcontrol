use std::sync::OnceLock;

use axum::extract::Path;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

const ASSETS: [(&str, &str, &str); 4] = [
    ("app.js", include_str!("../web/app.js"), "text/javascript; charset=utf-8"),
    ("login.js", include_str!("../web/login.js"), "text/javascript; charset=utf-8"),
    ("style.css", include_str!("../web/style.css"), "text/css; charset=utf-8"),
    ("icon.svg", include_str!("../web/icon.svg"), "image/svg+xml"),
];

/// Content hash of all assets. Asset URLs carry it (`app.js?v=…`), so after an update no
/// browser or CDN cache can keep serving an old script to a new page.
fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut h = Sha256::new();
        for (_, body, _) in ASSETS {
            h.update(body.as_bytes());
        }
        hex::encode(&h.finalize()[..6])
    })
}

fn render(page: &'static str) -> String {
    let v = version();
    ASSETS.iter().fold(page.to_string(), |p, (name, _, _)| {
        p.replace(&format!("/assets/{name}\""), &format!("/assets/{name}?v={v}\""))
    })
}

/// Pages must never be cached, by the browser or by Cloudflare in front of a tunnel.
fn no_store(mut resp: Response) -> Response {
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("cdn-cache-control", HeaderValue::from_static("no-store"));
    h.insert("cloudflare-cdn-cache-control", HeaderValue::from_static("no-store"));
    resp
}

pub async fn index() -> Response {
    static PAGE: OnceLock<String> = OnceLock::new();
    let page = PAGE.get_or_init(|| render(include_str!("../web/app.html")));
    no_store(axum::response::Html(page.as_str()).into_response())
}

pub async fn login_page() -> Response {
    static PAGE: OnceLock<String> = OnceLock::new();
    let page = PAGE.get_or_init(|| render(include_str!("../web/login.html")));
    no_store(axum::response::Html(page.as_str()).into_response())
}

pub async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Response {
    let Some((_, body, ct)) = ASSETS.iter().find(|(n, _, _)| *n == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let etag = format!("\"{}\"", version());
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == etag);
    let mut resp = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, *ct)], *body).into_response()
    };
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    h.insert("cdn-cache-control", HeaderValue::from_static("no-store"));
    h.insert("cloudflare-cdn-cache-control", HeaderValue::from_static("no-store"));
    h.insert(header::ETAG, HeaderValue::from_str(&etag).unwrap());
    resp
}
