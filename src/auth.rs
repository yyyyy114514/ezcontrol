use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::{util, AppState};

const ITERATIONS: u32 = 120_000;
const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const COOKIE: &str = "ezc_session";

// ---------- password hashing: PBKDF2-HMAC-SHA256 ----------

fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(Sha256::digest(key).as_slice());
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut h = Sha256::new();
    h.update(ipad);
    h.update(msg);
    let inner = h.finalize();
    let mut h = Sha256::new();
    h.update(opad);
    h.update(inner.as_slice());
    let mut out = [0u8; 32];
    out.copy_from_slice(h.finalize().as_slice());
    out
}

fn pbkdf2(pw: &[u8], salt: &[u8], iters: u32) -> [u8; 32] {
    let mut msg = salt.to_vec();
    msg.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(pw, &msg);
    let mut t = u;
    for _ in 1..iters {
        u = hmac_sha256(pw, &u);
        for (a, b) in t.iter_mut().zip(u.iter()) {
            *a ^= b;
        }
    }
    t
}

pub fn hash_password(pw: &str) -> String {
    let salt = util::random_bytes::<16>();
    let dk = pbkdf2(pw.as_bytes(), &salt, ITERATIONS);
    format!("pbkdf2-sha256${ITERATIONS}${}${}", hex::encode(salt), hex::encode(dk))
}

fn verify_password(pw: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2-sha256" {
        return false;
    }
    let (Ok(iters), Ok(salt), Ok(expect)) =
        (parts[1].parse::<u32>(), hex::decode(parts[2]), hex::decode(parts[3]))
    else {
        return false;
    };
    util::ct_eq(&pbkdf2(pw.as_bytes(), &salt, iters), &expect)
}

// ---------- sessions & brute-force protection ----------

struct Failures {
    count: u32,
    locked_until: Option<Instant>,
}

pub struct Auth {
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<HashMap<IpAddr, Failures>>,
}

impl Auth {
    pub fn new() -> Auth {
        Auth {
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(HashMap::new()),
        }
    }

    fn valid(&self, token: &str) -> bool {
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.retain(|_, exp| *exp > now);
        s.contains_key(token)
    }

    fn create(&self) -> String {
        let token = hex::encode(util::random_bytes::<32>());
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Instant::now() + SESSION_TTL);
        token
    }

    fn remove(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }

    fn locked(&self, ip: IpAddr) -> Option<u64> {
        let f = self.failures.lock().unwrap();
        let until = f.get(&ip)?.locked_until?;
        let now = Instant::now();
        (until > now).then(|| (until - now).as_secs() + 1)
    }

    fn fail(&self, ip: IpAddr) {
        let mut f = self.failures.lock().unwrap();
        let e = f.entry(ip).or_insert(Failures { count: 0, locked_until: None });
        e.count += 1;
        if e.count >= 5 {
            // 5 failures -> 1 min, doubles every further 5, capped at 1 h
            let mult = 1u64 << ((e.count / 5 - 1).min(6));
            e.locked_until = Some(Instant::now() + Duration::from_secs((60 * mult).min(3600)));
        }
    }

    fn succeed(&self, ip: IpAddr) {
        self.failures.lock().unwrap().remove(&ip);
    }
}

/// Real client IP. Forwarded headers are trusted only when the TCP peer is loopback
/// (i.e. the request came through a local cloudflared).
pub fn client_ip(peer: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if peer.ip().is_loopback() {
        if let Some(ip) = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
        {
            return ip;
        }
    }
    peer.ip()
}

fn is_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
        || headers
            .get("cf-visitor")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("https"))
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == COOKIE).then(|| v.to_string())
    })
}

#[derive(Deserialize)]
pub struct LoginReq {
    username: String,
    password: String,
}

pub async fn login(
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> Response {
    let ip = client_ip(peer, &headers);
    if let Some(secs) = st.auth.locked(ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            format!("尝试次数过多，请 {secs} 秒后再试"),
        )
            .into_response();
    }
    let stored = st.cfg.password_hash.clone();
    let pw = req.password;
    let pw_ok = tokio::task::spawn_blocking(move || verify_password(&pw, &stored))
        .await
        .unwrap_or(false);
    let ok = pw_ok && util::ct_eq(req.username.as_bytes(), st.cfg.username.as_bytes());
    if !ok {
        st.auth.fail(ip);
        util::log(&format!("登录失败 ip={ip} user={:?}", req.username));
        tokio::time::sleep(Duration::from_millis(600)).await;
        return (StatusCode::UNAUTHORIZED, "用户名或密码错误").into_response();
    }
    st.auth.succeed(ip);
    let token = st.auth.create();
    util::log(&format!("登录成功 ip={ip}"));
    let secure = if is_https(&headers) { "; Secure" } else { "" };
    let cookie = format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        SESSION_TTL.as_secs()
    );
    let mut resp = (StatusCode::OK, "ok").into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    resp
}

pub async fn logout(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(t) = session_token(&headers) {
        st.auth.remove(&t);
    }
    let mut resp = (StatusCode::OK, "ok").into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("ezc_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    resp
}

fn is_public(path: &str) -> bool {
    path == "/login" || path == "/api/login" || path.starts_with("/assets/")
}

/// Same-origin check for WebSocket upgrades and state-changing requests.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true; // non-browser client; cookie still required
    };
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let o = origin
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    o.eq_ignore_ascii_case(host)
}

pub async fn guard(State(st): State<Arc<AppState>>, req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let headers = req.headers();
    let mutating = req.method() != Method::GET && req.method() != Method::HEAD;

    if (mutating || path == "/ws") && !same_origin(headers) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    // CSRF: every mutating API call must carry a custom header (forces CORS preflight)
    if mutating && path.starts_with("/api/") && headers.get("x-ezc").is_none() {
        return (StatusCode::FORBIDDEN, "missing x-ezc").into_response();
    }

    let mut resp = if is_public(&path) {
        next.run(req).await
    } else {
        let authed = session_token(headers).is_some_and(|t| st.auth.valid(&t));
        if !authed {
            if path == "/" {
                Redirect::to("/login").into_response()
            } else {
                (StatusCode::UNAUTHORIZED, "未登录").into_response()
            }
        } else {
            next.run(req).await
        }
    };
    let h = resp.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' blob: data:; connect-src 'self' ws: wss:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    resp
}
