//! Screen capture: DXGI Desktop Duplication (GPU, only wakes on change) with a GDI fallback.
//! One capture thread runs while at least one viewer is connected and publishes the latest frame.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::watch;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, POINT, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::util;

/// DXGI frames grabbed / desktop presents seen (presents > grabs means updates were merged)
pub static STAT_GRABS: AtomicU64 = AtomicU64::new(0);
pub static STAT_PRESENTS: AtomicU64 = AtomicU64::new(0);

pub struct Frame {
    pub seq: u64,
    pub w: usize,
    pub h: usize,
    /// BGRA, stride = w * 4. Shared and immutable once published.
    pub data: Arc<Vec<u8>>,
}

#[derive(Clone, Serialize)]
pub struct Monitor {
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub w: i32,
    pub h: i32,
    pub primary: bool,
    #[serde(skip)]
    adapter: u32,
    #[serde(skip)]
    output: u32,
    #[serde(skip)]
    hmon: isize,
    /// desktop rendered by a software adapter (no GPU)
    #[serde(skip)]
    pub software: bool,
}

pub struct Hub {
    pub latest: Mutex<Option<Arc<Frame>>>,
    pub notify: watch::Sender<u64>,
    pub viewers: AtomicUsize,
    pub monitor: AtomicUsize,
    running: AtomicBool,
    seq: AtomicU64,
    monitors: Mutex<Vec<Monitor>>,
}

impl Hub {
    pub fn new() -> Arc<Hub> {
        let (tx, _) = watch::channel(0);
        Arc::new(Hub {
            latest: Mutex::new(None),
            notify: tx,
            viewers: AtomicUsize::new(0),
            monitor: AtomicUsize::new(0),
            running: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            monitors: Mutex::new(enumerate_monitors()),
        })
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        let mut m = self.monitors.lock().unwrap();
        let fresh = enumerate_monitors();
        if !fresh.is_empty() {
            *m = fresh;
        }
        m.clone()
    }

    pub fn current_monitor(&self) -> Monitor {
        let ms = self.monitors.lock().unwrap();
        let i = self.monitor.load(Ordering::Relaxed);
        ms.get(i).or(ms.first()).cloned().unwrap_or_else(primary_fallback)
    }

    pub fn set_monitor(&self, i: usize) {
        if self.monitor.swap(i, Ordering::Relaxed) != i {
            *self.latest.lock().unwrap() = None;
        }
    }

    pub fn add_viewer(self: &Arc<Self>) {
        self.viewers.fetch_add(1, Ordering::SeqCst);
        if !self.running.swap(true, Ordering::SeqCst) {
            let hub = self.clone();
            std::thread::Builder::new()
                .name("capture".into())
                .spawn(move || capture_thread(hub))
                .expect("spawn capture thread");
        }
    }

    pub fn remove_viewer(&self) {
        self.viewers.fetch_sub(1, Ordering::SeqCst);
    }

    fn publish(&self, f: Frame) {
        let seq = f.seq;
        *self.latest.lock().unwrap() = Some(Arc::new(f));
        self.notify.send_replace(seq);
    }
}

fn primary_fallback() -> Monitor {
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY).0 as isize };
    Monitor { name: "主屏幕".into(), left: 0, top: 0, w, h, primary: true, adapter: u32::MAX, output: 0, hmon, software: true }
}

fn enumerate_monitors() -> Vec<Monitor> {
    let mut out = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return vec![primary_fallback()];
        };
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            let software = adapter.GetDesc1().is_ok_and(|d| {
                // Microsoft Basic Render/Display Driver, WARP
                d.VendorId == 0x1414 || d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0
            });
            let mut o = 0;
            while let Ok(output) = adapter.EnumOutputs(o) {
                if let Ok(d) = output.GetDesc() {
                    if d.AttachedToDesktop.as_bool() {
                        let r = d.DesktopCoordinates;
                        let name = String::from_utf16_lossy(&d.DeviceName)
                            .trim_end_matches('\0')
                            .trim_start_matches("\\\\.\\")
                            .to_string();
                        out.push(Monitor {
                            name,
                            left: r.left,
                            top: r.top,
                            w: r.right - r.left,
                            h: r.bottom - r.top,
                            primary: r.left == 0 && r.top == 0,
                            adapter: a,
                            output: o,
                            hmon: d.Monitor.0 as isize,
                            software,
                        });
                    }
                }
                o += 1;
            }
            a += 1;
        }
    }
    out.sort_by_key(|m| (!m.primary, m.left, m.top));
    if out.is_empty() {
        out.push(primary_fallback());
    }
    out
}

/// Changed region in frame pixels.
#[derive(Clone, Copy)]
pub struct Region {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub trait Capturer {
    /// Waits for the next screen update and applies it to the internal frame buffer.
    /// Ok(None) = nothing changed within the timeout; Ok(Some(regions)) = what changed.
    fn next(&mut self, timeout_ms: u32) -> Result<Option<Vec<Region>>, String>;
    /// (width, height, BGRA pixels with stride = width * 4)
    fn frame(&self) -> (usize, usize, &[u8]);
}

// ---------------- DXGI ----------------

struct Dxgi {
    ctx: ID3D11DeviceContext,
    device: ID3D11Device,
    dup: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    w: usize,
    h: usize,
    buf: Vec<u8>,
    meta: Vec<u8>,
    first: bool,
    holding: bool,
}

impl Dxgi {
    fn new(m: &Monitor) -> Result<Dxgi, String> {
        if m.adapter == u32::MAX {
            return Err("no dxgi output".into());
        }
        unsafe {
            let factory = CreateDXGIFactory1::<IDXGIFactory1>().map_err(|e| e.to_string())?;
            let adapter = factory.EnumAdapters1(m.adapter).map_err(|e| e.to_string())?;
            let output = adapter.EnumOutputs(m.output).map_err(|e| e.to_string())?;
            let output1: IDXGIOutput1 = output.cast().map_err(|e| e.to_string())?;
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
            let device = device.ok_or("no device")?;
            let ctx = ctx.ok_or("no context")?;
            let dup = output1
                .DuplicateOutput(&device)
                .map_err(|e| format!("DuplicateOutput: {e}"))?;
            let desc = dup.GetDesc();
            Ok(Dxgi {
                ctx,
                device,
                dup,
                staging: None,
                w: desc.ModeDesc.Width as usize,
                h: desc.ModeDesc.Height as usize,
                buf: Vec::new(),
                meta: Vec::new(),
                first: true,
                holding: false,
            })
        }
    }

    /// Changed regions from the frame metadata (move destinations + dirty rects).
    unsafe fn regions(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Option<Vec<Region>> {
        let size = info.TotalMetadataBufferSize as usize;
        if size == 0 {
            return None;
        }
        if self.meta.len() < size {
            self.meta.resize(size, 0);
        }
        let (w, h) = (self.w, self.h);
        let clamp = |r: &RECT| -> Option<Region> {
            let (l, t) = (r.left.max(0) as usize, r.top.max(0) as usize);
            let (rr, b) = ((r.right.max(0) as usize).min(w), (r.bottom.max(0) as usize).min(h));
            (rr > l && b > t).then(|| Region { x: l, y: t, w: rr - l, h: b - t })
        };
        let mut out = Vec::new();
        let mut used = 0u32;
        unsafe {
            self.dup
                .GetFrameMoveRects(
                    self.meta.len() as u32,
                    self.meta.as_mut_ptr() as *mut DXGI_OUTDUPL_MOVE_RECT,
                    &mut used,
                )
                .ok()?;
            let moves = std::slice::from_raw_parts(
                self.meta.as_ptr() as *const DXGI_OUTDUPL_MOVE_RECT,
                used as usize / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>(),
            );
            out.extend(moves.iter().filter_map(|m| clamp(&m.DestinationRect)));
            self.dup
                .GetFrameDirtyRects(self.meta.len() as u32, self.meta.as_mut_ptr() as *mut RECT, &mut used)
                .ok()?;
            let dirty = std::slice::from_raw_parts(
                self.meta.as_ptr() as *const RECT,
                used as usize / std::mem::size_of::<RECT>(),
            );
            out.extend(dirty.iter().filter_map(clamp));
        }
        Some(out)
    }
}

impl Capturer for Dxgi {
    fn next(&mut self, timeout_ms: u32) -> Result<Option<Vec<Region>>, String> {
        unsafe {
            if self.holding {
                let _ = self.dup.ReleaseFrame();
                self.holding = false;
            }
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut res: Option<IDXGIResource> = None;
            match self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut res) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) => return Err(format!("AcquireNextFrame: {e}")),
            }
            self.holding = true;
            STAT_GRABS.fetch_add(1, Ordering::Relaxed);
            STAT_PRESENTS.fetch_add(info.AccumulatedFrames as u64, Ordering::Relaxed);
            if info.LastPresentTime == 0 {
                return Ok(None); // pointer-only update
            }
            let tex: ID3D11Texture2D = res.ok_or("no resource")?.cast().map_err(|e| e.to_string())?;
            if self.staging.is_none() {
                let mut d = D3D11_TEXTURE2D_DESC::default();
                tex.GetDesc(&mut d);
                d.Usage = D3D11_USAGE_STAGING;
                d.BindFlags = 0;
                d.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                d.MiscFlags = 0;
                d.MipLevels = 1;
                d.ArraySize = 1;
                d.SampleDesc = DXGI_SAMPLE_DESC { Count: 1, Quality: 0 };
                if d.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                    return Err(format!("unsupported format {:?}", d.Format));
                }
                self.w = d.Width as usize;
                self.h = d.Height as usize;
                let mut st = None;
                self.device
                    .CreateTexture2D(&d, None, Some(&mut st))
                    .map_err(|e| e.to_string())?;
                self.staging = st;
                self.buf = vec![0u8; self.w * self.h * 4];
                self.first = true;
            }
            let full = Region { x: 0, y: 0, w: self.w, h: self.h };
            let mut regions = if self.first { None } else { self.regions(&info) }
                .unwrap_or_else(|| vec![full]);
            if regions.len() > 64 {
                // many scattered rects: copy their bounding box instead
                let x0 = regions.iter().map(|r| r.x).min().unwrap();
                let y0 = regions.iter().map(|r| r.y).min().unwrap();
                let x1 = regions.iter().map(|r| r.x + r.w).max().unwrap();
                let y1 = regions.iter().map(|r| r.y + r.h).max().unwrap();
                regions = vec![Region { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }];
            }
            self.first = false;
            if regions.is_empty() {
                return Ok(None);
            }
            let staging = self.staging.as_ref().unwrap();
            for r in &regions {
                let bx = D3D11_BOX {
                    left: r.x as u32,
                    top: r.y as u32,
                    front: 0,
                    right: (r.x + r.w) as u32,
                    bottom: (r.y + r.h) as u32,
                    back: 1,
                };
                self.ctx
                    .CopySubresourceRegion(staging, 0, r.x as u32, r.y as u32, 0, &tex, 0, Some(&bx));
            }
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                .map_err(|e| e.to_string())?;
            let row = self.w * 4;
            let pitch = map.RowPitch as usize;
            let src = std::slice::from_raw_parts(map.pData as *const u8, pitch * self.h);
            for r in &regions {
                for y in r.y..r.y + r.h {
                    let s = y * pitch + r.x * 4;
                    let d = y * row + r.x * 4;
                    self.buf[d..d + r.w * 4].copy_from_slice(&src[s..s + r.w * 4]);
                }
            }
            self.ctx.Unmap(staging, 0);
            let _ = self.dup.ReleaseFrame();
            self.holding = false;
            Ok(Some(regions))
        }
    }

    fn frame(&self) -> (usize, usize, &[u8]) {
        (self.w, self.h, &self.buf)
    }
}

// ---------------- GDI fallback ----------------

struct Gdi {
    left: i32,
    top: i32,
    w: usize,
    h: usize,
    buf: Vec<u8>,
    interval: Duration,
    last_at: Instant,
}

impl Gdi {
    fn new(m: &Monitor) -> Gdi {
        Gdi {
            left: m.left,
            top: m.top,
            w: m.w as usize,
            h: m.h as usize,
            buf: Vec::new(),
            interval: Duration::from_millis(33),
            last_at: Instant::now() - Duration::from_secs(1),
        }
    }

    fn grab(&self) -> Result<Vec<u8>, String> {
        unsafe {
            let screen = GetDC(None);
            if screen.is_invalid() {
                return Err("GetDC failed".into());
            }
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, self.w as i32, self.h as i32);
            let old = SelectObject(mem, bmp.into());
            let ok = BitBlt(
                mem, 0, 0, self.w as i32, self.h as i32,
                Some(screen), self.left, self.top, SRCCOPY | CAPTUREBLT,
            );
            let mut data = vec![0u8; self.w * self.h * 4];
            let mut bi = BITMAPINFO::default();
            bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = self.w as i32;
            bi.bmiHeader.biHeight = -(self.h as i32);
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB.0;
            let lines = GetDIBits(
                mem, bmp, 0, self.h as u32,
                Some(data.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            if ok.is_err() || lines == 0 {
                return Err("BitBlt failed".into());
            }
            Ok(data)
        }
    }
}

impl Capturer for Gdi {
    fn next(&mut self, timeout_ms: u32) -> Result<Option<Vec<Region>>, String> {
        let wait = self.interval.saturating_sub(self.last_at.elapsed());
        std::thread::sleep(wait.min(Duration::from_millis(timeout_ms as u64)));
        self.last_at = Instant::now();
        let data = self.grab()?;
        if data == self.buf {
            return Ok(None);
        }
        self.buf = data;
        Ok(Some(vec![Region { x: 0, y: 0, w: self.w, h: self.h }]))
    }

    fn frame(&self) -> (usize, usize, &[u8]) {
        (self.w, self.h, &self.buf)
    }
}

// ---------------- capture thread ----------------

/// Reusable frame buffers. A buffer is free once no viewer holds a Frame pointing at it;
/// it is brought up to date by copying only the regions changed since it was last filled.
struct Pool {
    bufs: Vec<(Arc<Vec<u8>>, u64)>,
    /// (seq, regions changed in that frame), oldest first
    history: VecDeque<(u64, Vec<Region>)>,
}

impl Pool {
    const HISTORY: usize = 32;

    fn new() -> Pool {
        Pool { bufs: Vec::new(), history: VecDeque::new() }
    }

    fn take(&mut self, seq: u64, regions: Vec<Region>, w: usize, src: &[u8]) -> Arc<Vec<u8>> {
        self.history.push_back((seq, regions));
        while self.history.len() > Self::HISTORY {
            self.history.pop_front();
        }
        let oldest = self.history.front().map(|(s, _)| *s).unwrap_or(seq);
        // freshest free buffer of the right size
        let pick = self
            .bufs
            .iter()
            .enumerate()
            .filter(|(_, (b, _))| Arc::strong_count(b) == 1 && b.len() == src.len())
            .max_by_key(|(_, (_, s))| *s)
            .map(|(i, _)| i);
        let Some(i) = pick else {
            // free buffers here have the wrong size (resolution changed): drop them
            self.bufs.retain(|(b, _)| Arc::strong_count(b) > 1);
            let b = Arc::new(src.to_vec());
            self.bufs.push((b.clone(), seq));
            return b;
        };
        let (buf, bseq) = &mut self.bufs[i];
        let data = Arc::get_mut(buf).unwrap();
        if *bseq + 1 >= oldest {
            for (s, rs) in &self.history {
                if *s <= *bseq {
                    continue;
                }
                for r in rs {
                    for y in r.y..r.y + r.h {
                        let a = (y * w + r.x) * 4;
                        data[a..a + r.w * 4].copy_from_slice(&src[a..a + r.w * 4]);
                    }
                }
            }
        } else {
            data.copy_from_slice(src);
        }
        *bseq = seq;
        buf.clone()
    }
}

/// Capture backend chosen with --capture (auto | wgc | dxgi | gdi).
pub static BACKEND: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn open_capturer(mon: &Monitor) -> Box<dyn Capturer> {
    let mode = BACKEND.get().map(String::as_str).unwrap_or("auto");
    if mode == "wgc" || (mode == "auto" && mon.software) {
        match crate::wgc::Wgc::new(mon.hmon) {
            Ok(c) => {
                util::log("采集方式: Windows.Graphics.Capture");
                return Box::new(c);
            }
            Err(e) => util::log(&format!("Windows.Graphics.Capture 不可用 ({e})")),
        }
    }
    if mode != "gdi" {
        match Dxgi::new(mon) {
            Ok(c) => {
                util::log("采集方式: DXGI 桌面复制");
                return Box::new(c);
            }
            Err(e) => util::log(&format!("DXGI 不可用 ({e})")),
        }
    }
    util::log("使用 GDI 采集");
    Box::new(Gdi::new(mon))
}

fn capture_thread(hub: Arc<Hub>) {
    let min_interval = Duration::from_micros(1_000_000 / 120);
    let mut idle_since: Option<Instant> = None;
    'outer: loop {
        if hub.viewers.load(Ordering::SeqCst) == 0 {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() > Duration::from_secs(3) {
                hub.running.store(false, Ordering::SeqCst);
                // a viewer may have arrived between the check and the store
                if hub.viewers.load(Ordering::SeqCst) == 0
                    || hub.running.swap(true, Ordering::SeqCst)
                {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        idle_since = None;

        let mon_idx = hub.monitor.load(Ordering::Relaxed);
        let mon = hub.current_monitor();
        let mut cap = open_capturer(&mon);
        let mut pool = Pool::new();
        let mut last = Instant::now() - min_interval;
        loop {
            if hub.viewers.load(Ordering::SeqCst) == 0
                || hub.monitor.load(Ordering::Relaxed) != mon_idx
            {
                continue 'outer;
            }
            let wait = min_interval.saturating_sub(last.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
            match cap.next(100) {
                Ok(Some(regions)) => {
                    last = Instant::now();
                    let (w, h, src) = cap.frame();
                    let seq = hub.seq.fetch_add(1, Ordering::Relaxed) + 1;
                    let data = pool.take(seq, regions, w, src);
                    hub.publish(Frame { seq, w, h, data });
                }
                Ok(None) => {}
                Err(e) => {
                    util::log(&format!("采集中断: {e}，重新初始化"));
                    std::thread::sleep(Duration::from_millis(300));
                    continue 'outer;
                }
            }
        }
    }
}

// ---------------- cursor ----------------

/// Current cursor as a CSS cursor name, and position relative to the given monitor.
pub fn cursor_state(m: &Monitor) -> (&'static str, i32, i32) {
    unsafe {
        let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut ci).is_err() {
            return ("default", -1, -1);
        }
        let pos = (ci.ptScreenPos.x - m.left, ci.ptScreenPos.y - m.top);
        if ci.flags.0 & CURSOR_SHOWING.0 == 0 {
            return ("none", pos.0, pos.1);
        }
        let table: [(PCWSTR, &'static str); 13] = [
            (IDC_ARROW, "default"),
            (IDC_IBEAM, "text"),
            (IDC_HAND, "pointer"),
            (IDC_WAIT, "wait"),
            (IDC_APPSTARTING, "progress"),
            (IDC_CROSS, "crosshair"),
            (IDC_SIZEALL, "move"),
            (IDC_SIZENS, "ns-resize"),
            (IDC_SIZEWE, "ew-resize"),
            (IDC_SIZENWSE, "nwse-resize"),
            (IDC_SIZENESW, "nesw-resize"),
            (IDC_NO, "not-allowed"),
            (IDC_HELP, "help"),
        ];
        for (id, name) in table {
            if let Ok(h) = LoadCursorW(None, id) {
                if h == ci.hCursor {
                    return (name, pos.0, pos.1);
                }
            }
        }
        ("default", pos.0, pos.1)
    }
}

use windows::core::PCWSTR;
