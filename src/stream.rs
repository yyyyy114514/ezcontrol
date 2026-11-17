//! Per-viewer WebSocket session: dirty-tile diff + parallel JPEG encode, ACK-based flow control,
//! input events on the same socket.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use jpeg_encoder::{ColorType, Encoder};
use rayon::prelude::*;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;

use crate::capture::{self, Frame, Hub};
use crate::h264::{self, H264State};
use crate::pacer::Pacer;
use crate::{auth, input, util, AppState};

const TILE: usize = 64;
/// Most tiles one high-quality refine message may re-send, so it never bursts on slow links.
const REFINE_TILES: usize = 48;
const MAX_FPS: u32 = 120;
/// Clipboard text larger than this is not synced automatically.
const MAX_CLIP_CHARS: usize = 4 << 20;

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = auth::client_ip(peer, &headers);
    ws.max_message_size(16 << 20)
        .on_upgrade(move |socket| session(socket, st.hub.clone(), ip.to_string()))
}

#[derive(Deserialize, Clone, Copy)]
struct Settings {
    quality: u8,
    scale: f32,
    fps: u32,
    refine: bool,
    /// lower quality/resolution automatically when the link congests
    #[serde(default = "default_true")]
    auto: bool,
    /// H.264 instead of JPEG tiles (client decodes with WebCodecs)
    #[serde(default)]
    h264: bool,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings { quality: 70, scale: 1.0, fps: 60, refine: true, auto: true, h264: false }
    }
}

enum Ctrl {
    Ack(Option<u32>),
    Settings(Settings),
    Keyframe,
    Monitor(usize),
}

#[derive(Deserialize)]
#[serde(tag = "t")]
enum ClientMsg {
    /// `s`: sequence number of the frame the client drew
    #[serde(rename = "ack")]
    Ack {
        #[serde(default)]
        s: Option<u32>,
    },
    #[serde(rename = "cfg")]
    Cfg(Settings),
    #[serde(rename = "key")]
    Keyframe,
    #[serde(rename = "mon")]
    Monitor { i: usize },
    #[serde(rename = "ping")]
    Ping { ts: f64 },
    #[serde(rename = "clip_get")]
    ClipGet,
    #[serde(rename = "clip_set")]
    ClipSet { s: String },
    /// push remote clipboard changes to this client
    #[serde(rename = "clip_watch")]
    ClipWatch { on: bool },
    /// browser details, logged for diagnosis
    #[serde(rename = "info")]
    Info {
        #[serde(default)]
        ua: String,
        #[serde(default)]
        codec: String,
        #[serde(default)]
        h264: serde_json::Value,
    },
    #[serde(untagged)]
    Input(input::Event),
}

async fn session(socket: WebSocket, hub: Arc<Hub>, ip: String) {
    util::log(&format!("远程会话开始 ip={ip}"));
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(16);
    let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<Ctrl>();

    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(m).await.is_err() {
                break;
            }
        }
    });

    let hello = json!({
        "t": "hello",
        "monitors": hub.monitors(),
        "monitor": hub.monitor.load(std::sync::atomic::Ordering::Relaxed),
    });
    let _ = out_tx.send(Message::Text(hello.to_string().into())).await;

    hub.add_viewer();
    let video = tokio::spawn(video_loop(hub.clone(), out_tx.clone(), ctrl_rx, ip.clone()));
    let (watch_tx, watch_rx) = tokio::sync::watch::channel(false);
    // clipboard sequence number of our own last write, so it isn't echoed back
    let own_seq = Arc::new(AtomicU32::new(0));
    let clip = tokio::spawn(clip_watch(out_tx.clone(), watch_rx, own_seq.clone()));

    // reader: input is applied immediately here so it never waits behind encoding
    let mut keys = input::Pressed::default();
    while let Some(Ok(msg)) = stream.next().await {
        let Message::Text(text) = msg else {
            if let Message::Close(_) = msg {
                break;
            }
            continue;
        };
        let Ok(m) = serde_json::from_str::<ClientMsg>(&text) else {
            continue;
        };
        match m {
            ClientMsg::Ack { s } => {
                let _ = ctrl_tx.send(Ctrl::Ack(s));
            }
            ClientMsg::Cfg(s) => {
                let _ = ctrl_tx.send(Ctrl::Settings(s));
            }
            ClientMsg::Keyframe => {
                let _ = ctrl_tx.send(Ctrl::Keyframe);
            }
            ClientMsg::Monitor { i } => {
                let _ = ctrl_tx.send(Ctrl::Monitor(i));
            }
            ClientMsg::Ping { ts } => {
                let _ = out_tx.try_send(Message::Text(json!({"t":"pong","ts":ts}).to_string().into()));
            }
            ClientMsg::ClipGet => {
                let s = tokio::task::spawn_blocking(input::clipboard_get).await.ok().flatten();
                let _ = out_tx
                    .send(Message::Text(json!({"t":"clip","s":s}).to_string().into()))
                    .await;
            }
            ClientMsg::ClipSet { s } => {
                let seq = tokio::task::spawn_blocking(move || input::clipboard_set(&s)).await;
                if let Ok(Some(seq)) = seq {
                    own_seq.store(seq, Ordering::Relaxed);
                }
            }
            ClientMsg::ClipWatch { on } => {
                let _ = watch_tx.send(on);
            }
            ClientMsg::Info { ua, codec, h264 } => {
                let ua: String = ua.chars().take(200).collect();
                util::log(&format!("客户端 ip={ip} 编码={codec} H.264检测={h264} UA={ua}"));
            }
            ClientMsg::Input(ev) => {
                let mon = hub.current_monitor();
                input::apply(&ev, &mon, &mut keys);
            }
        }
    }
    keys.release_all();
    hub.remove_viewer();
    video.abort();
    clip.abort();
    drop(out_tx);
    let _ = writer.await;
    util::log(&format!("远程会话结束 ip={ip}"));
}

/// Sends the remote clipboard text to the client whenever it changes (while enabled).
async fn clip_watch(
    out: mpsc::Sender<Message>,
    mut enabled: tokio::sync::watch::Receiver<bool>,
    own_seq: Arc<AtomicU32>,
) {
    let mut last = input::clipboard_seq();
    let mut tick = tokio::time::interval(Duration::from_millis(300));
    loop {
        if !*enabled.borrow_and_update() {
            if enabled.changed().await.is_err() {
                return;
            }
            // only changes made after sync was switched on
            last = input::clipboard_seq();
            continue;
        }
        tokio::select! {
            r = enabled.changed() => { if r.is_err() { return; } continue; }
            _ = tick.tick() => {}
        }
        let seq = input::clipboard_seq();
        if seq == last {
            continue;
        }
        last = seq;
        if seq == own_seq.load(Ordering::Relaxed) {
            continue;
        }
        let Ok(Some(s)) = tokio::task::spawn_blocking(input::clipboard_get).await else {
            continue; // not text
        };
        if s.is_empty() || s.len() > MAX_CLIP_CHARS {
            continue;
        }
        let msg = json!({"t": "clip", "s": s, "auto": true}).to_string();
        if out.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }
}

struct EncState {
    /// what the client currently shows; shared with the capture pool, never copied
    prev: Arc<Vec<u8>>,
    w: usize,
    h: usize,
    /// tiles sent at normal quality since last high-quality refine
    lowq: Vec<bool>,
}

struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// Adjusts quality, then resolution (JPEG), or the bitrate (H.264), from congestion signals.
/// The user's settings are the ceiling.
struct Adapt {
    q: u8,
    scale: f32,
    last_step: Instant,
    /// when quality reached its floor; resolution only drops if congestion persists after that
    at_floor_since: Option<Instant>,
    /// H.264 target and ceiling, bits per second
    rate: u32,
    rate_max: u32,
}

const MIN_RATE: u32 = 300_000;

/// H.264 bitrate ceiling for a quality setting: ~0.05 bit/pixel at 60 fps for quality 70.
fn rate_for(quality: u8, pixels: f64) -> u32 {
    (pixels * 60.0 * 0.05 * quality as f64 / 70.0).clamp(500_000.0, 40_000_000.0) as u32
}

const SCALE_STEPS: [f32; 3] = [1.0, 0.75, 0.5];
const MIN_AUTO_Q: u8 = 30;

impl Adapt {
    fn new(s: &Settings, screen_pixels: usize) -> Adapt {
        let rate_max = rate_for(s.quality, screen_pixels as f64 * (s.scale * s.scale) as f64);
        Adapt {
            q: s.quality,
            scale: s.scale,
            last_step: Instant::now(),
            at_floor_since: None,
            // start in the middle and let the controller find the link's capacity
            rate: if s.auto { rate_max / 2 } else { rate_max },
            rate_max,
        }
    }

    /// The link is saturated. `bw` is the measured delivery rate in bytes/s.
    /// Returns true if the resolution changed (needs a full frame).
    fn congested(&mut self, s: &Settings, bw: f64) -> bool {
        let now = Instant::now();
        self.last_step = now;
        if s.h264 {
            // straight to just under what the link has been delivering; the target may be far
            // above what the encoder actually produces, so scaling it down would take ages
            let r = if bw > 0.0 { bw * 8.0 * 0.85 } else { self.rate as f64 * 0.7 };
            self.rate = (r.min(self.rate as f64 * 0.9) as u32).max(MIN_RATE);
            return false;
        }
        if self.q > MIN_AUTO_Q {
            self.q = self.q.saturating_sub(10).max(MIN_AUTO_Q);
            if self.q == MIN_AUTO_Q {
                self.at_floor_since = Some(now);
            }
            return false;
        }
        let floor_for = self.at_floor_since.map_or(Duration::ZERO, |t| now - t);
        if floor_for < Duration::from_secs(5) {
            return false;
        }
        if let Some(&next) = SCALE_STEPS.iter().find(|&&x| x < self.scale - 0.01) {
            self.scale = next;
            self.q = s.quality.min(50);
            self.at_floor_since = None;
            return true;
        }
        false
    }

    /// Called while the link is calm. `bw` is the measured delivery rate in bytes/s.
    /// Returns true if the resolution changed.
    fn calm(&mut self, s: &Settings, calm_for: Duration, bw: f64) -> bool {
        if self.last_step.elapsed() < Duration::from_secs(2) || calm_for < Duration::from_secs(2) {
            return false;
        }
        self.last_step = Instant::now();
        if s.h264 {
            // probe upwards gradually, never far beyond what the link has shown it delivers
            let ceiling = if bw > 0.0 { (bw * 8.0 * 1.5).max(MIN_RATE as f64) } else { f64::MAX };
            let r = (self.rate as f64 * 1.15).min(ceiling).max(self.rate as f64);
            self.rate = (r as u32).min(self.rate_max);
            return false;
        }
        if self.q < s.quality {
            self.q = (self.q + 5).min(s.quality);
            self.at_floor_since = None;
            return false;
        }
        if self.scale < s.scale - 0.01 && calm_for > Duration::from_secs(10) {
            self.scale = SCALE_STEPS
                .iter()
                .rev()
                .find(|&&x| x > self.scale + 0.01)
                .copied()
                .unwrap_or(s.scale)
                .min(s.scale);
            return true;
        }
        false
    }
}

async fn video_loop(
    hub: Arc<Hub>,
    out: mpsc::Sender<Message>,
    mut ctrl: mpsc::UnboundedReceiver<Ctrl>,
    ip: String,
) {
    let mut notify = hub.notify.subscribe();
    // per-session numbers for ezcontrol.log, to diagnose remote links
    let mut stats_tick = tokio::time::interval(Duration::from_secs(10));
    stats_tick.tick().await;
    let (mut st_frames, mut st_bytes, mut st_cuts, mut st_skipped) = (0u32, 0usize, 0u32, 0u32);
    let mut st_seq = *notify.borrow();
    // encode time (total, max), largest message, times a ready frame waited for the window
    let (mut st_enc, mut st_enc_max, mut st_big, mut st_waits) = (Duration::ZERO, Duration::ZERO, 0usize, 0u32);
    let started = Instant::now();
    let mut settings = Settings::default();
    let screen_pixels = || {
        let m = hub.current_monitor();
        (m.w.max(1) * m.h.max(1)) as usize
    };
    let mut adapt = Adapt::new(&settings, screen_pixels());
    let mut pacer = Pacer::new();
    let mut state: Option<EncState> = None;
    let mut h264: Option<H264State> = None;
    let mut h264_key = false;
    // re-encodes of an unchanged picture, which sharpen it after motion
    let mut h264_refines = 0u8;
    let mut last_sent = Instant::now() - Duration::from_secs(1);
    let mut last_seq = 0u64;
    let mut force = true;
    let mut seq_out: u32 = 0;
    let mut cursor_tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_cursor: (&str, i32, i32) = ("", -1, -1);
    let mut last_change = Instant::now();
    let mut reported = (0u8, 0f32, 0u32);
    const REFINE_DELAY: Duration = Duration::from_millis(400);
    /// no high-quality refine this soon after congestion: it would only add to the queue
    const REFINE_CALM: Duration = Duration::from_secs(3);

    loop {
        if settings.auto && adapt.calm(&settings, pacer.calm_for(), pacer.bandwidth()) {
            state = None;
            force = true;
        }
        if settings.auto && pacer.saturated() && adapt.last_step.elapsed() > Duration::from_secs(1) {
            st_cuts += 1;
            if adapt.congested(&settings, pacer.bandwidth()) {
                state = None;
                force = true;
            }
        }
        let interval = Duration::from_micros(1_000_000 / settings.fps.clamp(1, MAX_FPS) as u64);
        let latest_seq = *notify.borrow_and_update();
        let have_new = latest_seq != last_seq;
        let refine_due = if settings.h264 {
            settings.refine && h264.is_some() && h264_refines < 3 && pacer.calm_for() > REFINE_CALM
        } else {
            settings.refine
                && adapt.q < 85
                && pacer.calm_for() > REFINE_CALM
                && state.as_ref().is_some_and(|s| s.lowq.iter().any(|b| *b))
        };
        let blocked = !pacer.can_send();
        if blocked && (have_new || force) {
            pacer.mark_limited();
            st_waits += 1;
        }
        let deadline: Option<Instant> = if blocked {
            pacer.stall_deadline()
        } else if force || have_new {
            Some(last_sent + interval)
        } else if refine_due {
            Some((last_change + REFINE_DELAY).max(last_sent + interval))
        } else if settings.auto
            && (adapt.q < settings.quality
                || adapt.scale < settings.scale - 0.01
                || (settings.h264 && adapt.rate < adapt.rate_max))
        {
            // wake up to recover quality even when the screen is idle
            Some(Instant::now() + Duration::from_secs(1))
        } else {
            None
        };

        let rate = if settings.h264 { adapt.rate } else { 0 };
        if (adapt.q, adapt.scale, rate) != reported {
            reported = (adapt.q, adapt.scale, rate);
            let msg = json!({"t": "adapt", "q": adapt.q, "s": adapt.scale, "kbps": rate / 1000,
                             "max_kbps": adapt.rate_max / 1000, "rtt": pacer.srtt().as_millis() as u64});
            let _ = out.try_send(Message::Text(msg.to_string().into()));
        }

        if deadline.is_none_or(|d| d > Instant::now()) {
            let sleep_to = deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
            tokio::select! {
                c = ctrl.recv() => {
                    match c {
                        None => return,
                        Some(Ctrl::Ack(s)) => pacer.on_ack(s),
                        Some(Ctrl::Settings(s)) => {
                            settings = Settings {
                                quality: s.quality.clamp(10, 95),
                                scale: s.scale.clamp(0.2, 1.0),
                                fps: s.fps.clamp(1, MAX_FPS),
                                refine: s.refine,
                                auto: s.auto,
                                h264: s.h264,
                            };
                            let old_scale = adapt.scale;
                            adapt = Adapt::new(&settings, screen_pixels());
                            if (adapt.scale - old_scale).abs() > 0.001 || !settings.h264 { state = None; }
                            if !settings.h264 { h264 = None; }
                            h264_key = true;
                            force = true;
                        }
                        Some(Ctrl::Keyframe) => {
                            // the client lost its decoder state: whatever it had in flight is gone too
                            pacer.reset();
                            state = None;
                            h264_key = true;
                            force = true;
                        }
                        Some(Ctrl::Monitor(i)) => {
                            hub.set_monitor(i);
                            adapt = Adapt::new(&settings, screen_pixels());
                            state = None;
                            h264_key = true;
                            force = true;
                        }
                    }
                }
                r = notify.changed() => { if r.is_err() { return; } }
                _ = stats_tick.tick() => {
                    let p = pacer.stats();
                    let enc = if settings.h264 {
                        format!("H.264 码率 {:.1}/{:.1} Mbps", adapt.rate as f64 / 1e6, adapt.rate_max as f64 / 1e6)
                    } else {
                        format!("JPEG 画质 {} 缩放 {}%", adapt.q, (adapt.scale * 100.0).round())
                    };
                    let seq = *notify.borrow();
                    let encodes = seq.saturating_sub(st_seq).max(st_frames as u64).max(1);
                    let (w, h) = hub.latest.lock().unwrap().as_ref().map_or((0, 0), |f| (f.w, f.h));
                    util::log(&format!(
                        "会话统计 ip={ip} | 画面变化 {:.1} fps 发送 {:.1} fps {:.2} Mbps 编码器丢帧 {} | 编码 平均 {} 最大 {} ms 最大帧 {} KB 分辨率 {}x{} | RTT 平滑 {} 基线 {} ms | 窗口 {} KB 在途 {} 帧 等窗口 {} 次 带宽估计 {:.1} Mbps 降级 {} 次 | {enc}",
                        seq.saturating_sub(st_seq) as f64 / 10.0,
                        st_frames as f64 / 10.0,
                        st_bytes as f64 * 8.0 / 10.0 / 1e6,
                        st_skipped,
                        (st_enc / encodes as u32).as_millis(),
                        st_enc_max.as_millis(),
                        st_big / 1024,
                        (w as f32 * adapt.scale) as usize,
                        (h as f32 * adapt.scale) as usize,
                        p.srtt_ms, p.base_ms, p.window_kb, p.inflight, st_waits, p.bw_mbps, st_cuts,
                    ));
                    (st_frames, st_bytes, st_cuts, st_skipped, st_seq) = (0, 0, 0, 0, seq);
                    (st_enc, st_enc_max, st_big, st_waits) = (Duration::ZERO, Duration::ZERO, 0, 0);
                }
                _ = cursor_tick.tick() => {
                    let mon = hub.current_monitor();
                    let c = capture::cursor_state(&mon);
                    if c != last_cursor {
                        last_cursor = c;
                        let _ = out.try_send(Message::Text(json!({"t":"cur","c":c.0,"x":c.1,"y":c.2}).to_string().into()));
                    }
                }
                _ = tokio::time::sleep_until(sleep_to.into()) => {}
            }
            continue;
        }

        // deadline reached
        if blocked {
            pacer.reset(); // client stopped acknowledging; resync
            continue;
        }
        if !force && !have_new && !refine_due {
            continue; // quality-recovery wakeup only
        }
        let frame: Option<Arc<Frame>> = hub.latest.lock().unwrap().clone();
        let Some(frame) = frame else {
            force = false;
            continue;
        };
        let refine_only = !force && !have_new;
        if have_new {
            last_change = Instant::now();
        }
        force = false;
        last_seq = frame.seq;

        let enc_start = Instant::now();
        let msg = if settings.h264 {
            h264_refines = if refine_only { h264_refines + 1 } else { 0 };
            let (scale, rate, key) = (adapt.scale, adapt.rate, std::mem::take(&mut h264_key));
            // real timestamps: the encoder's rate control budgets bits per elapsed time
            let ts = started.elapsed().as_millis() as u64;
            let st = h264.take();
            let Ok((st, msg)) = tokio::task::spawn_blocking(move || {
                let (w, h, img) = scale_frame(&frame, scale);
                h264::encode(st, img, w, h, rate, key, ts)
            })
            .await
            else {
                return;
            };
            h264 = st;
            if msg.is_none() && !refine_only {
                st_skipped += 1;
            }
            msg
        } else {
            let s = Settings { quality: adapt.q, scale: adapt.scale, ..settings };
            let st = state.take();
            let Ok((st, msg)) =
                tokio::task::spawn_blocking(move || encode(&frame, st, s, refine_only)).await
            else {
                return;
            };
            state = Some(st);
            msg
        };
        let enc_time = enc_start.elapsed();
        st_enc += enc_time;
        st_enc_max = st_enc_max.max(enc_time);
        if let Some(mut buf) = msg {
            st_big = st_big.max(buf.len());
            seq_out = seq_out.wrapping_add(1);
            buf[1..5].copy_from_slice(&seq_out.to_le_bytes());
            pacer.on_send(seq_out, buf.len());
            st_bytes += buf.len();
            if out.send(Message::Binary(buf.into())).await.is_err() {
                return;
            }
            last_sent = Instant::now();
            st_frames += 1;
        }
    }
}

fn scale_frame(f: &Frame, scale: f32) -> (usize, usize, Arc<Vec<u8>>) {
    if scale >= 0.999 {
        return (f.w, f.h, f.data.clone());
    }
    let dw = ((f.w as f32 * scale) as usize).max(16) & !1;
    let dh = ((f.h as f32 * scale) as usize).max(16) & !1;
    let mut out = vec![0u8; dw * dh * 4];
    let sx = (f.w as u64) * 65536 / dw as u64;
    let sy = (f.h as u64) * 65536 / dh as u64;
    let src = &f.data;
    let sw = f.w;
    out.par_chunks_mut(dw * 4).enumerate().for_each(|(y, row)| {
        // box filter over the source footprint (approximated by 2x2 bilinear at its center)
        let fy = ((y as u64 * sy + sy / 2) as i64 - 32768).max(0) as u64;
        let y0 = ((fy >> 16) as usize).min(f.h - 1);
        let y1 = (y0 + 1).min(f.h - 1);
        let wy = (fy & 0xffff) as u32 >> 8;
        for x in 0..dw {
            let fx = ((x as u64 * sx + sx / 2) as i64 - 32768).max(0) as u64;
            let x0 = ((fx >> 16) as usize).min(sw - 1);
            let x1 = (x0 + 1).min(sw - 1);
            let wx = (fx & 0xffff) as u32 >> 8;
            let p00 = (y0 * sw + x0) * 4;
            let p01 = (y0 * sw + x1) * 4;
            let p10 = (y1 * sw + x0) * 4;
            let p11 = (y1 * sw + x1) * 4;
            for c in 0..3 {
                let top = src[p00 + c] as u32 * (256 - wx) + src[p01 + c] as u32 * wx;
                let bot = src[p10 + c] as u32 * (256 - wx) + src[p11 + c] as u32 * wx;
                row[x * 4 + c] = ((top * (256 - wy) + bot * wy) >> 16) as u8;
            }
            row[x * 4 + 3] = 255;
        }
    });
    (dw, dh, Arc::new(out))
}

/// Returns dirty tiles (row-major) between prev and cur.
fn diff(prev: &[u8], cur: &[u8], w: usize, h: usize) -> Vec<bool> {
    let cols = w.div_ceil(TILE);
    let rows = h.div_ceil(TILE);
    let stride = w * 4;
    let mut out = vec![false; cols * rows];
    out.par_chunks_mut(cols).enumerate().for_each(|(ty, band)| {
        let y0 = ty * TILE;
        let y1 = (y0 + TILE).min(h);
        for y in y0..y1 {
            let off = y * stride;
            for (tx, d) in band.iter_mut().enumerate() {
                if *d {
                    continue;
                }
                let a = off + tx * TILE * 4;
                let b = off + ((tx + 1) * TILE).min(w) * 4;
                if prev[a..b] != cur[a..b] {
                    *d = true;
                }
            }
        }
    });
    out
}

/// Merge dirty tiles into horizontal runs per tile row (bridging 1-tile gaps).
fn rects(dirty: &[bool], w: usize, h: usize) -> Vec<Rect> {
    let cols = w.div_ceil(TILE);
    let mut out = Vec::new();
    for (ty, band) in dirty.chunks(cols).enumerate() {
        let mut tx = 0;
        while tx < cols {
            if !band[tx] {
                tx += 1;
                continue;
            }
            let start = tx;
            let mut end = tx + 1;
            while end < cols && (band[end] || (end + 1 < cols && band[end + 1])) {
                end += 1;
            }
            let x = start * TILE;
            let y = ty * TILE;
            out.push(Rect {
                x,
                y,
                w: (end * TILE).min(w) - x,
                h: (y + TILE).min(h) - y,
            });
            tx = end;
        }
    }
    out
}

fn encode_rect(img: &[u8], stride_px: usize, r: &Rect, q: u8) -> Vec<u8> {
    let copied;
    let data: &[u8] = if r.x == 0 && r.w == stride_px {
        // full-width band is already contiguous
        let a = r.y * stride_px * 4;
        &img[a..a + r.w * r.h * 4]
    } else {
        let mut buf = Vec::with_capacity(r.w * r.h * 4);
        for y in r.y..r.y + r.h {
            let a = (y * stride_px + r.x) * 4;
            buf.extend_from_slice(&img[a..a + r.w * 4]);
        }
        copied = buf;
        &copied
    };
    let mut out = Vec::with_capacity(r.w * r.h / 4);
    let enc = Encoder::new(&mut out, q);
    let _ = enc.encode(data, r.w as u16, r.h as u16, ColorType::Bgra);
    out
}

/// Message: [type=1][seq u32][fw u16][fh u16][n u16] then n x {x,y,w,h u16, len u32, jpeg}
fn encode(
    f: &Frame,
    st: Option<EncState>,
    s: Settings,
    refine_only: bool,
) -> (EncState, Option<Vec<u8>>) {
    let (w, h, cur) = scale_frame(f, s.scale);
    let cols = w.div_ceil(TILE);
    let rows = h.div_ceil(TILE);
    let (dirty, quality, mut st) = match st {
        Some(st) if st.w == w && st.h == h => {
            if refine_only {
                let mut d = st.lowq.clone();
                d.iter_mut().filter(|b| **b).skip(REFINE_TILES).for_each(|b| *b = false);
                (d, s.quality.saturating_add(25).min(92), st)
            } else if Arc::ptr_eq(&st.prev, &cur) {
                (vec![false; cols * rows], s.quality, st)
            } else {
                let d = diff(&st.prev, &cur, w, h);
                (d, s.quality, st)
            }
        }
        _ => (
            vec![true; cols * rows],
            s.quality,
            EncState { prev: cur.clone(), w, h, lowq: vec![false; cols * rows] },
        ),
    };
    let rs = rects(&dirty, w, h);
    if rs.is_empty() {
        st.prev = cur;
        return (st, None);
    }
    let encoded: Vec<Vec<u8>> = rs.par_iter().map(|r| encode_rect(&cur, w, r, quality)).collect();

    let total: usize = encoded.iter().map(|e| e.len() + 12).sum();
    let mut msg = Vec::with_capacity(11 + total);
    msg.push(1u8);
    msg.extend_from_slice(&0u32.to_le_bytes());
    msg.extend_from_slice(&(w as u16).to_le_bytes());
    msg.extend_from_slice(&(h as u16).to_le_bytes());
    msg.extend_from_slice(&(rs.len() as u16).to_le_bytes());
    for (r, e) in rs.iter().zip(&encoded) {
        for v in [r.x, r.y, r.w, r.h] {
            msg.extend_from_slice(&(v as u16).to_le_bytes());
        }
        msg.extend_from_slice(&(e.len() as u32).to_le_bytes());
        msg.extend_from_slice(e);
    }

    // track which tiles are at low quality (whole runs, including bridged gaps)
    let hq = refine_only || quality >= 85;
    for r in &rs {
        let ty = r.y / TILE;
        for tx in r.x / TILE..(r.x + r.w).div_ceil(TILE) {
            st.lowq[ty * cols + tx] = !hq;
        }
    }
    st.prev = cur;
    (st, Some(msg))
}

/// `ezcontrol --bench`: time each pipeline stage on the current screen.
pub fn bench(hub: Arc<Hub>) {
    hub.add_viewer();
    let t0 = Instant::now();
    let frame = loop {
        if let Some(f) = hub.latest.lock().unwrap().clone() {
            break f;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    println!("首帧采集: {:?}  {}x{}", t0.elapsed(), frame.w, frame.h);
    let seq0 = frame.seq;
    // run the real viewer pipeline (diff + encode) on every new frame for 3 s
    let settings = Settings { refine: false, ..Settings::default() };
    let (mut st, mut last, mut n, mut busy, mut bytes) = (None, 0u64, 0u32, Duration::ZERO, 0usize);
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        let f = hub.latest.lock().unwrap().clone().unwrap();
        if f.seq == last {
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        last = f.seq;
        let t1 = Instant::now();
        let (s, msg) = encode(&f, st.take(), settings, false);
        busy += t1.elapsed();
        st = Some(s);
        if let Some(m) = msg {
            n += 1;
            bytes += m.len();
        }
    }
    {
        use std::sync::atomic::Ordering::Relaxed;
        let grabs = capture::STAT_GRABS.load(Relaxed);
        if grabs > 0 {
            println!("DXGI: 抓取 {grabs} 次, 桌面实际更新 {} 次", capture::STAT_PRESENTS.load(Relaxed));
        }
    }
    let seq1 = hub.latest.lock().unwrap().as_ref().map(|f| f.seq).unwrap_or(seq0);
    println!("采集速率(3秒内屏幕变化帧): {:.1} fps", (seq1 - seq0) as f64 / 3.0);
    println!(
        "编码管线: 发出 {:.1} fps, 平均每帧 {:.2} ms, {} KB",
        n as f64 / 3.0,
        busy.as_secs_f64() * 1000.0 / n.max(1) as f64,
        bytes / 1024 / n.max(1) as usize
    );

    let time = |name: &str, n: u32, f: &mut dyn FnMut()| {
        let t = Instant::now();
        for _ in 0..n {
            f();
        }
        println!("{name}: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    };
    let (w, h) = (frame.w, frame.h);
    time("复制整帧", 20, &mut || drop(std::hint::black_box(frame.data.to_vec())));
    time("缩放 50%", 20, &mut || drop(std::hint::black_box(scale_frame(&frame, 0.5))));
    let other = frame.data.to_vec();
    time("差分整帧", 20, &mut || drop(std::hint::black_box(diff(&frame.data, &other, w, h))));
    let all = vec![true; w.div_ceil(TILE) * h.div_ceil(TILE)];
    let rs = rects(&all, w, h);
    for q in [50u8, 70] {
        time(&format!("编码整帧 q{q} 并行 ({} 块)", rs.len()), 10, &mut || {
            let v: Vec<Vec<u8>> = rs.par_iter().map(|r| encode_rect(&frame.data, w, r, q)).collect();
            std::hint::black_box(v);
        });
    }
    time("编码整帧 q70 单线程", 3, &mut || {
        let v: Vec<Vec<u8>> = rs.iter().map(|r| encode_rect(&frame.data, w, r, 70)).collect();
        std::hint::black_box(v);
    });
    hub.remove_viewer();
}
