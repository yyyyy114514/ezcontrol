//! Optional Cloudflare Tunnel: spawns cloudflared pointing at the local server, and reports
//! whether the tunnel is actually connected (cloudflared hands out a quick-tunnel URL even
//! when it cannot reach Cloudflare's edge, which then answers with error 1033).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

use crate::util;

pub enum Mode {
    /// Temporary *.trycloudflare.com URL, no account needed.
    Quick,
    /// Named tunnel created in the Cloudflare dashboard (stable custom domain).
    Token(String),
}

/// How long to wait for the first edge connection before explaining what is wrong.
const CONNECT_GRACE: Duration = Duration::from_secs(30);

fn find_cloudflared(explicit: Option<String>) -> PathBuf {
    if let Some(p) = explicit {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        let local = exe.with_file_name("cloudflared.exe");
        if local.exists() {
            return local;
        }
    }
    for p in [
        r"C:\Program Files (x86)\cloudflared\cloudflared.exe",
        r"C:\Program Files\cloudflared\cloudflared.exe",
    ] {
        if std::path::Path::new(p).exists() {
            return PathBuf::from(p);
        }
    }
    PathBuf::from("cloudflared.exe")
}

/// What cloudflared's log says about why it cannot connect.
#[derive(Default)]
struct Symptoms {
    url: Option<String>,
    tls_intercepted: bool,
    quic_blocked: bool,
    tcp_blocked: bool,
    no_edge_addr: bool,
    last_err: Option<String>,
}

impl Symptoms {
    fn note(&mut self, line: &str) {
        if line.contains("x509") || line.contains("TLS handshake with edge") {
            self.tls_intercepted = true;
        }
        if line.contains("UDP Connectivity") && line.contains("FAIL")
            || line.contains("failed to dial to edge with quic")
        {
            self.quic_blocked = true;
        }
        if line.contains("TCP Connectivity") && line.contains("FAIL") {
            self.tcp_blocked = true;
        }
        if line.contains("no free edge addresses") {
            self.no_edge_addr = true;
        }
        if line.contains(" ERR ") {
            self.last_err = Some(line.split(" ERR ").nth(1).unwrap_or(line).trim().to_string());
        }
    }

    fn diagnose(&self) -> Vec<String> {
        let mut v = vec!["Cloudflare 隧道仍未连通，公网地址此时会显示 Error 1033。".to_string()];
        if self.tls_intercepted {
            v.push("原因：连接 Cloudflare 边缘（7844 端口）时收到了无效证书，说明流量被代理软件或防火墙拦截/劫持。".into());
        } else if self.quic_blocked && self.tcp_blocked {
            v.push("原因：出站 7844 端口的 UDP(QUIC) 和 TCP(HTTP2) 都连不上 Cloudflare 边缘，被网络阻断。".into());
        } else if self.quic_blocked {
            v.push("原因：UDP(QUIC) 被阻断，可以试试 --tunnel-protocol http2。".into());
        } else if self.no_edge_addr {
            v.push("原因：解析不到可用的 Cloudflare 边缘地址（DNS 问题）。".into());
        } else if let Some(e) = &self.last_err {
            v.push(format!("cloudflared 最近的错误: {e}"));
        }
        v.push("处理办法：".into());
        v.push("  1) 若本机开着代理/TUN（如 sing-box、Clash、xray 等），给 cloudflared.exe 设为直连/绕过，或放行到 198.41.192.0/24、198.41.200.0/24 的 7844 端口；".into());
        v.push("  2) 或换一个能直连 Cloudflare 7844 端口的网络；".into());
        v.push("  3) 局域网内可先用 --bind 0.0.0.0 直接访问。cloudflared 会继续在后台重试。".into());
        v
    }
}

/// Puts the child in a job that is killed when this process exits for any reason
/// (including a crash or Task Manager), so cloudflared is never left running orphaned.
fn bind_lifetime(child: &Child) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::*;
    let Some(raw) = child.raw_handle() else { return };
    unsafe {
        let Ok(job) = CreateJobObjectW(None, None) else { return };
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let _ = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        let _ = AssignProcessToJobObject(job, HANDLE(raw));
        // the job handle stays open for the rest of the process on purpose
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(20290)
}

/// Number of live edge connections, from cloudflared's metrics `/ready` endpoint.
async fn ready_connections(port: u16) -> Option<u32> {
    let mut s = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?
    .ok()?;
    s.write_all(b"GET /ready HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n").await.ok()?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), s.read_to_end(&mut buf)).await.ok()?.ok()?;
    let text = String::from_utf8_lossy(&buf);
    let i = text.find("\"readyConnections\":")? + "\"readyConnections\":".len();
    text[i..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
}

/// The child is killed when the returned handle is dropped (on server exit).
pub fn start(mode: Mode, port: u16, cloudflared: Option<String>, protocol: Option<String>) -> Option<Child> {
    let exe = find_cloudflared(cloudflared);
    let metrics = free_port();
    let mut cmd = Command::new(&exe);
    cmd.arg("tunnel")
        .arg("--no-autoupdate")
        .arg("--metrics")
        .arg(format!("127.0.0.1:{metrics}"));
    if let Some(p) = &protocol {
        cmd.arg("--protocol").arg(p);
    }
    match &mode {
        Mode::Quick => {
            cmd.arg("--url").arg(format!("http://127.0.0.1:{port}"));
        }
        Mode::Token(t) => {
            cmd.arg("run").arg("--token").arg(t);
        }
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            util::log(&format!(
                "无法启动 cloudflared ({}): {e}。请安装 cloudflared 或用 --cloudflared 指定路径",
                exe.display()
            ));
            return None;
        }
    };
    bind_lifetime(&child);
    util::log("cloudflared 已启动，正在建立隧道...");
    let symptoms = Arc::new(Mutex::new(Symptoms::default()));

    let stderr = child.stderr.take().unwrap();
    let sym = symptoms.clone();
    let quick = matches!(mode, Mode::Quick);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut s = sym.lock().unwrap();
            if quick && s.url.is_none() {
                if let Some(i) = line.find("https://") {
                    let url: String = line[i..]
                        .chars()
                        .take_while(|c| !c.is_whitespace() && *c != '|')
                        .collect();
                    if url.contains("trycloudflare.com") {
                        util::log(&format!("已分配临时地址 {url}，等待隧道连通..."));
                        s.url = Some(url);
                        continue;
                    }
                }
            }
            s.note(&line);
        }
        util::log("cloudflared 已退出，隧道不可用");
    });

    // watch the real connection state
    tokio::spawn(async move {
        let started = Instant::now();
        let mut connected = false;
        let mut explained = false;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let n = ready_connections(metrics).await.unwrap_or(0);
            if n > 0 && !connected {
                connected = true;
                let url = symptoms.lock().unwrap().url.clone();
                util::log("==============================================");
                match url {
                    Some(u) => util::log(&format!(" Cloudflare 隧道已连通，公网地址: {u}")),
                    None => util::log(" Cloudflare 隧道已连通（使用你在 Cloudflare 后台配置的域名访问）"),
                }
                util::log("==============================================");
            } else if n == 0 && connected {
                connected = false;
                util::log("Cloudflare 隧道连接中断，cloudflared 正在重连...");
            } else if n == 0 && !explained && started.elapsed() > CONNECT_GRACE {
                explained = true;
                for line in symptoms.lock().unwrap().diagnose() {
                    util::log(&line);
                }
            }
        }
    });
    Some(child)
}
