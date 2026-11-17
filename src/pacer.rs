//! Flow control for the video stream, modelled on BBR.
//!
//! Every frame is acknowledged by the client after it is drawn. From the acks we measure the
//! delivery rate (bytes acknowledged per second) and the round-trip time. The bytes allowed
//! in flight are a multiple of rate × RTT (the bandwidth-delay product): enough to keep the
//! link busy, little enough that we never build a long queue of our own.
//!
//! Delay alone is not used as a congestion signal: tunnels across continents change their
//! RTT by hundreds of milliseconds on their own, which a delay-based scheme mistakes for
//! congestion and answers by throttling a link it barely uses. Real saturation shows up as
//! frames waiting for the window, which the video loop sees via `saturated()`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_WINDOW: usize = 16 << 20;
const INITIAL_WINDOW: usize = 256 * 1024;
const MIN_WINDOW: usize = 64 * 1024;
const MAX_FRAMES: usize = 120;
/// window = GAIN × bandwidth × min RTT; >1 so the rate estimate can grow when the link allows
const GAIN: f64 = 2.0;
/// rate and RTT estimates are kept over two windows of this length, so they follow the path
const WINDOW: Duration = Duration::from_secs(10);
/// frames have to keep waiting for the window this long before it counts as saturation
const SATURATED_AFTER: Duration = Duration::from_millis(500);

struct Sent {
    seq: u32,
    bytes: usize,
    at: Instant,
    /// total bytes acknowledged, and when, at the moment this frame was sent
    delivered: u64,
    delivered_at: Instant,
}

pub struct Pacer {
    inflight: VecDeque<Sent>,
    inflight_bytes: usize,
    srtt: Duration,
    /// minimum RTT over the current and previous window. The minimum, not a typical value:
    /// any RTT above it may include our own queue, and sizing the window from that would let
    /// the queue feed itself.
    min_cur: Duration,
    min_prev: Duration,
    base: Duration,
    /// delivery rate, bytes per second: max over the current and previous window
    bw_cur: f64,
    bw_prev: f64,
    bw_window: Instant,
    /// whether frames waited for the window during the current window. Only such windows
    /// measure the link; quiet ones only measure the screen, and must not lower the estimate.
    pushed_in_window: bool,
    delivered: u64,
    delivered_at: Instant,
    /// continuous stretch of frames waiting for the window
    limited_since: Option<Instant>,
    last_limited: Option<Instant>,
}

/// Snapshot for the session log.
pub struct PacerStats {
    pub srtt_ms: u128,
    pub base_ms: u128,
    pub window_kb: usize,
    pub inflight: usize,
    pub bw_mbps: f64,
}

impl Pacer {
    pub fn new() -> Pacer {
        let now = Instant::now();
        Pacer {
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            srtt: Duration::from_millis(100),
            min_cur: Duration::MAX,
            min_prev: Duration::MAX,
            base: Duration::ZERO,
            bw_cur: 0.0,
            bw_prev: 0.0,
            bw_window: now,
            pushed_in_window: false,
            delivered: 0,
            delivered_at: now,
            limited_since: None,
            last_limited: None,
        }
    }

    pub fn bandwidth(&self) -> f64 {
        self.bw_cur.max(self.bw_prev)
    }

    fn window(&self) -> usize {
        let bw = self.bandwidth();
        if bw <= 0.0 || self.base.is_zero() {
            return INITIAL_WINDOW;
        }
        ((GAIN * bw * self.base.as_secs_f64()) as usize).clamp(MIN_WINDOW, MAX_WINDOW)
    }

    pub fn can_send(&self) -> bool {
        self.inflight.is_empty()
            || (self.inflight_bytes < self.window() && self.inflight.len() < MAX_FRAMES)
    }

    pub fn on_send(&mut self, seq: u32, bytes: usize) {
        let now = Instant::now();
        // after an idle period, measure delivery from this send instead of the last ack
        let delivered_at = if self.inflight.is_empty() { now } else { self.delivered_at };
        self.inflight.push_back(Sent { seq, bytes, at: now, delivered: self.delivered, delivered_at });
        self.inflight_bytes += bytes;
    }

    fn update_base(&mut self, rtt: Duration, roll: bool) {
        if roll {
            self.min_prev = self.min_cur;
            self.min_cur = Duration::MAX;
        }
        self.min_cur = self.min_cur.min(rtt);
        self.base = self.min_cur.min(self.min_prev);
    }

    /// `seq` is the frame the client drew. Acks for frames no longer tracked (sent before a
    /// reset) are ignored: matching them to newer frames would yield impossibly short RTTs.
    pub fn on_ack(&mut self, seq: Option<u32>) {
        if let Some(s) = seq {
            if !self.inflight.iter().any(|f| f.seq == s) {
                return;
            }
            // frames before it were drawn too (acks are in order); they carry no RTT sample
            while self.inflight.front().is_some_and(|f| f.seq != s) {
                let f = self.inflight.pop_front().unwrap();
                self.inflight_bytes -= f.bytes;
                self.delivered += f.bytes as u64;
            }
        }
        let Some(sent) = self.inflight.pop_front() else {
            return;
        };
        self.inflight_bytes -= sent.bytes;
        let now = Instant::now();
        self.delivered += sent.bytes as u64;
        self.delivered_at = now;

        let rtt = now - sent.at;
        self.srtt = (self.srtt * 7 + rtt) / 8;
        let roll = now - self.bw_window > WINDOW;
        self.update_base(rtt, roll);
        let interval = (now - sent.delivered_at).as_secs_f64().max(1e-3);
        let rate = (self.delivered - sent.delivered) as f64 / interval;
        if roll {
            self.bw_prev = if self.pushed_in_window {
                self.bw_cur
            } else {
                self.bw_cur.max(self.bw_prev)
            };
            self.bw_cur = 0.0;
            self.bw_window = now;
            self.pushed_in_window = false;
        }
        self.bw_cur = self.bw_cur.max(rate);
    }

    /// When the oldest unacknowledged frame should be given up on.
    pub fn stall_deadline(&self) -> Option<Instant> {
        let s = self.inflight.front()?;
        Some(s.at + (self.srtt * 4).max(Duration::from_secs(3)))
    }

    /// Client stopped acknowledging (tab hidden, lost acks): forget what is in flight.
    pub fn reset(&mut self) {
        self.inflight.clear();
        self.inflight_bytes = 0;
    }

    /// A frame is ready but must wait for the window.
    pub fn mark_limited(&mut self) {
        let now = Instant::now();
        // marks less than 200 ms apart belong to the same stretch
        if self.last_limited.is_none_or(|t| now - t > Duration::from_millis(200)) {
            self.limited_since = Some(now);
        }
        self.last_limited = Some(now);
        self.pushed_in_window = true;
    }

    /// The screen has been producing more than the link carries for a while: lowering
    /// quality or bitrate will help.
    pub fn saturated(&self) -> bool {
        let recent = self.last_limited.is_some_and(|t| t.elapsed() < Duration::from_millis(200));
        recent && self.limited_since.is_some_and(|t| t.elapsed() > SATURATED_AFTER)
    }

    /// Time since frames last had to wait for the window.
    pub fn calm_for(&self) -> Duration {
        self.last_limited.map_or(Duration::MAX, |t| t.elapsed())
    }

    pub fn srtt(&self) -> Duration {
        self.srtt
    }

    pub fn stats(&self) -> PacerStats {
        PacerStats {
            srtt_ms: self.srtt.as_millis(),
            base_ms: self.base.as_millis(),
            window_kb: self.window() / 1024,
            inflight: self.inflight.len(),
            bw_mbps: self.bandwidth() * 8.0 / 1e6,
        }
    }
}
