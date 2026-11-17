//! Windows.Graphics.Capture backend (Windows 10 1903+). Frames land in our own buffer pool,
//! so reading them back never waits on the compositor's shared surface. This matters most on
//! machines without a GPU, where DXGI duplication readback stalls for a whole compose cycle.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::capture::{Capturer, Region};

/// Rows compared/copied as one unit when detecting changes.
const BAND: usize = 16;
/// Frame pool depth: lets the compositor fill one buffer while we read another.
const BUFFERS: i32 = 2;

pub struct Wgc {
    device: ID3D11Device,
    ctx: ID3D11DeviceContext,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_arrived: i64,
    signal: Arc<(Mutex<bool>, Condvar)>,
    staging: Option<ID3D11Texture2D>,
    size: SizeInt32,
    w: usize,
    h: usize,
    buf: Vec<u8>,
    first: bool,
}

fn err(e: windows::core::Error) -> String {
    e.to_string()
}

impl Wgc {
    pub fn new(hmon: isize) -> Result<Wgc, String> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
            if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
                return Err("Windows.Graphics.Capture 不受支持".into());
            }
            let mut device = None;
            let mut ctx = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
            .map_err(err)?;
            let device: ID3D11Device = device.ok_or("no device")?;
            let ctx = ctx.ok_or("no context")?;
            let dxgi: IDXGIDevice = device.cast().map_err(err)?;
            let winrt_device: IDirect3DDevice =
                CreateDirect3D11DeviceFromDXGIDevice(&dxgi).map_err(err)?.cast().map_err(err)?;

            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
                .map_err(err)?;
            let item: GraphicsCaptureItem =
                interop.CreateForMonitor(HMONITOR(hmon as *mut _)).map_err(err)?;
            let size = item.Size().map_err(err)?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                BUFFERS,
                size,
            )
            .map_err(err)?;
            let signal = Arc::new((Mutex::new(false), Condvar::new()));
            let s2 = signal.clone();
            let frame_arrived = pool.FrameArrived(&TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
                move |_, _| {
                    *s2.0.lock().unwrap() = true;
                    s2.1.notify_one();
                    Ok(())
                },
            ))
            .map_err(err)?;
            let session = pool.CreateCaptureSession(&item).map_err(err)?;
            // the browser shows its own pointer; border control only exists on Windows 11
            let _ = session.SetIsCursorCaptureEnabled(false);
            let _ = session.SetIsBorderRequired(false);
            session.StartCapture().map_err(err)?;
            Ok(Wgc {
                device,
                ctx,
                winrt_device,
                pool,
                session,
                frame_arrived,
                signal,
                staging: None,
                size,
                w: size.Width as usize,
                h: size.Height as usize,
                buf: Vec::new(),
                first: true,
            })
        }
    }
}

impl Drop for Wgc {
    /// Must be torn down explicitly: dropping a running session and pool lets the
    /// compositor keep delivering into them, and the next capture created in this
    /// process then crashes.
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_arrived);
        let _ = self.session.Close();
        let _ = self.pool.Close();
        unsafe {
            self.ctx.ClearState();
            self.ctx.Flush();
        }
    }
}

impl Capturer for Wgc {
    fn next(&mut self, timeout_ms: u32) -> Result<Option<Vec<Region>>, String> {
        {
            let (m, cv) = &*self.signal;
            let mut ready = m.lock().unwrap();
            if !*ready {
                ready = cv
                    .wait_timeout(ready, Duration::from_millis(timeout_ms as u64))
                    .unwrap()
                    .0;
            }
            if !*ready {
                return Ok(None);
            }
            *ready = false;
        }
        // drain to the newest frame
        let mut frame = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            if let Some(old) = frame.replace(f) {
                let _ = old.Close();
            }
        }
        let Some(frame) = frame else { return Ok(None) };

        let size = frame.ContentSize().map_err(err)?;
        if size.Width != self.size.Width || size.Height != self.size.Height {
            // resolution changed
            self.size = size;
            self.pool
                .Recreate(&self.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, BUFFERS, size)
                .map_err(err)?;
            self.staging = None;
            let _ = frame.Close();
            return Ok(None);
        }
        unsafe {
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface().map_err(err)?.cast().map_err(err)?;
            let tex: ID3D11Texture2D = access.GetInterface().map_err(err)?;
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
                let mut st = None;
                self.device.CreateTexture2D(&d, None, Some(&mut st)).map_err(err)?;
                self.staging = st;
                self.w = (size.Width as usize).min(d.Width as usize);
                self.h = (size.Height as usize).min(d.Height as usize);
                self.buf = vec![0u8; self.w * self.h * 4];
                self.first = true;
            }
            let staging = self.staging.as_ref().unwrap();
            self.ctx.CopyResource(staging, &tex);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            let mapped = self.ctx.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut map));
            let _ = frame.Close();
            mapped.map_err(err)?;
            let row = self.w * 4;
            let pitch = map.RowPitch as usize;
            let src = std::slice::from_raw_parts(map.pData as *const u8, pitch * self.h);

            // copy only row bands that changed, and report them as regions
            let mut regions: Vec<Region> = Vec::new();
            let mut y = 0;
            while y < self.h {
                let bh = BAND.min(self.h - y);
                let changed = self.first
                    || (y..y + bh).any(|yy| src[yy * pitch..yy * pitch + row] != self.buf[yy * row..(yy + 1) * row]);
                if changed {
                    for yy in y..y + bh {
                        self.buf[yy * row..(yy + 1) * row].copy_from_slice(&src[yy * pitch..yy * pitch + row]);
                    }
                    match regions.last_mut() {
                        Some(r) if r.y + r.h == y => r.h += bh,
                        _ => regions.push(Region { x: 0, y, w: self.w, h: bh }),
                    }
                }
                y += bh;
            }
            self.ctx.Unmap(staging, 0);
            self.first = false;
            Ok((!regions.is_empty()).then_some(regions))
        }
    }

    fn frame(&self) -> (usize, usize, &[u8]) {
        (self.w, self.h, &self.buf)
    }
}
