mod auth;
mod capture;
mod config;
mod files;
mod h264;
mod input;
mod pacer;
mod stream;
mod tunnel;
mod util;
mod wgc;
mod web;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::serve::ListenerExt;
use axum::Router;

pub struct AppState {
    pub cfg: config::Config,
    pub auth: auth::Auth,
    pub hub: Arc<capture::Hub>,
}

fn print_help() {
    println!(
        "ezcontrol - 浏览器远程控制本机

用法: ezcontrol [选项]
  --port <N>             监听端口 (默认 8090，保存到配置)
  --bind <ADDR>          监听地址 (默认 127.0.0.1；局域网直连用 0.0.0.0)
  --user <NAME>          设置登录用户名
  --set-password [PW]    设置登录密码（不带参数时交互输入）后退出
  --tunnel               同时启动 Cloudflare 快速隧道 (trycloudflare.com)
  --tunnel-token <TOKEN> 使用 Cloudflare 命名隧道的 token 启动
  --cloudflared <PATH>   指定 cloudflared.exe 路径
  --tunnel-protocol <P>  隧道协议: auto(默认) / quic / http2
  --capture <MODE>       采集方式: auto(默认) / wgc / dxgi / gdi
  --bench                测试本机采集与编码性能
  -h, --help             显示帮助"
    );
}

#[tokio::main]
async fn main() {
    util::init_console();
    util::init_dpi();
    util::init_com();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = config::Config::load_or_create();
    let mut tunnel_mode: Option<tunnel::Mode> = None;
    let mut cloudflared: Option<String> = None;
    let mut tunnel_protocol: Option<String> = None;
    let mut dirty = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let next = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_else(|| {
                eprintln!("参数 {a} 缺少值");
                std::process::exit(2)
            })
        };
        match a {
            "-h" | "--help" => {
                print_help();
                return;
            }
            "--port" => {
                cfg.port = next(&mut i).parse().expect("端口无效");
                dirty = true;
            }
            "--bind" => {
                cfg.bind = next(&mut i);
                dirty = true;
            }
            "--user" => {
                cfg.username = next(&mut i);
                dirty = true;
            }
            "--set-password" => {
                let pw = match args.get(i + 1) {
                    Some(p) if !p.starts_with("--") => p.clone(),
                    _ => util::prompt_password(),
                };
                if pw.chars().count() < 6 {
                    eprintln!("密码至少 6 位");
                    std::process::exit(2);
                }
                cfg.password_hash = auth::hash_password(&pw);
                cfg.save();
                println!("密码已更新，已有登录会话将在重启后失效。");
                return;
            }
            "--capture" => {
                let m = next(&mut i);
                if !matches!(m.as_str(), "auto" | "wgc" | "dxgi" | "gdi") {
                    eprintln!("--capture 只能是 auto / wgc / dxgi / gdi");
                    std::process::exit(2);
                }
                let _ = capture::BACKEND.set(m);
            }
            "--bench" => {
                stream::bench(capture::Hub::new());
                return;
            }
            "--tunnel" => tunnel_mode = Some(tunnel::Mode::Quick),
            "--tunnel-token" => tunnel_mode = Some(tunnel::Mode::Token(next(&mut i))),
            "--cloudflared" => cloudflared = Some(next(&mut i)),
            "--tunnel-protocol" => {
                let p = next(&mut i);
                if !matches!(p.as_str(), "auto" | "quic" | "http2") {
                    eprintln!("--tunnel-protocol 只能是 auto / quic / http2");
                    std::process::exit(2);
                }
                tunnel_protocol = Some(p);
            }
            other => {
                eprintln!("未知参数: {other}");
                print_help();
                std::process::exit(2);
            }
        }
        i += 1;
    }
    if dirty {
        cfg.save();
    }

    let addr: SocketAddr = format!("{}:{}", cfg.bind, cfg.port)
        .parse()
        .expect("监听地址无效");
    let state = Arc::new(AppState {
        auth: auth::Auth::new(),
        hub: capture::Hub::new(),
        cfg,
    });

    let app = Router::new()
        .route("/", get(web::index))
        .route("/login", get(web::login_page))
        .route("/assets/{name}", get(web::asset))
        .route("/api/login", post(auth::login))
        .route("/api/logout", post(auth::logout))
        .route("/api/files/list", get(files::list))
        .route("/api/files/download", get(files::download))
        .route(
            "/api/files/upload",
            post(files::upload).layer(DefaultBodyLimit::max(64 * 1024 * 1024)),
        )
        .route("/api/files/mkdir", post(files::mkdir))
        .route("/api/files/delete", post(files::delete))
        .route("/api/files/rename", post(files::rename))
        .route("/ws", get(stream::ws_handler))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::guard,
        ))
        .with_state(state.clone());

    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("无法监听 {addr}: {e}");
            std::process::exit(1);
        }
    };
    util::log(&format!("ezcontrol 已启动: http://{addr}  用户名: {}", state.cfg.username));
    if addr.ip().is_unspecified() {
        util::log("注意：正在监听所有网卡，请确保密码足够强。");
    }

    let _tunnel = tunnel_mode.map(|m| tunnel::start(m, state.cfg.port, cloudflared, tunnel_protocol));

    // Small WebSocket frames must not wait for Nagle + delayed ACK (~200 ms on Windows).
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    if let Err(e) = server.await {
        eprintln!("服务器错误: {e}");
    }
    util::log("已退出");
}
