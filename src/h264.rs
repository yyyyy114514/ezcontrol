//! H.264 encoding (openh264, built from source) for browsers that can decode with WebCodecs.
//! Inter-frame coding makes typical desktop updates several times smaller than JPEG tiles,
//! which matters most on slow links such as a Cloudflare tunnel.

use std::sync::Arc;

use openh264::encoder::{
    BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp};
use openh264_sys2::{
    SBitrateInfo, SEncParamExt, ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE,
    ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, SM_FIXEDSLCNUM_SLICE, SPATIAL_LAYER_ALL,
};

pub struct H264State {
    enc: Encoder,
    yuv: YUVBuffer,
    pub w: usize,
    pub h: usize,
    bitrate: u32,
    /// slices/threads applied (only possible after the encoder initialised on its first frame)
    threaded: bool,
    /// frame last encoded, to tell a refine pass (same picture) from new content
    pub prev: Option<Arc<Vec<u8>>>,
}

fn threads() -> u16 {
    let n = std::thread::available_parallelism().map_or(2, |n| n.get());
    (n / 2).clamp(1, 4) as u16
}

impl H264State {
    fn new(w: usize, h: usize, bitrate: u32) -> Result<H264State, String> {
        let cfg = EncoderConfig::new()
            // camera mode: ~4x faster than screen-content mode at this size, similar output size
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bitrate))
            .max_frame_rate(FrameRate::from_hz(60.0))
            .complexity(Complexity::Low)
            // never drop frames (that is what stutter looks like): rate control adjusts the
            // quantiser instead, and the pacer keeps the link from queueing
            .skip_frames(false)
            .num_threads(threads());
        let enc = Encoder::with_api_config(OpenH264API::from_source(), cfg).map_err(|e| e.to_string())?;
        Ok(H264State { enc, yuv: YUVBuffer::new(w, h), w, h, bitrate, threaded: false, prev: None })
    }

    fn set_bitrate(&mut self, bps: u32) {
        if bps == self.bitrate {
            return;
        }
        self.bitrate = bps;
        unsafe {
            let raw = self.enc.raw_api();
            let mut info = SBitrateInfo { iLayer: SPATIAL_LAYER_ALL, iBitrate: bps as i32 };
            raw.set_option(ENCODER_OPTION_MAX_BITRATE, &mut info as *mut _ as *mut _);
            raw.set_option(ENCODER_OPTION_BITRATE, &mut info as *mut _ as *mut _);
        }
    }

    /// Split the picture into slices so the encoder can use several threads.
    fn enable_threads(&mut self) {
        let n = threads();
        self.threaded = true;
        if n < 2 {
            return;
        }
        unsafe {
            let raw = self.enc.raw_api();
            let mut p: SEncParamExt = std::mem::zeroed();
            if raw.get_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, &mut p as *mut _ as *mut _) != 0 {
                return;
            }
            p.iMultipleThreadIdc = n;
            p.sSpatialLayers[0].sSliceArgument.uiSliceMode = SM_FIXEDSLCNUM_SLICE;
            p.sSpatialLayers[0].sSliceArgument.uiSliceNum = n as u32;
            raw.set_option(ENCODER_OPTION_SVC_ENCODE_PARAM_EXT, &mut p as *mut _ as *mut _);
        }
    }
}

/// Message: [type=2][seq u32][w u16][h u16][key u8] then an Annex B access unit.
pub fn encode(
    st: Option<H264State>,
    img: Arc<Vec<u8>>,
    w: usize,
    h: usize,
    bitrate: u32,
    force_key: bool,
    ts_ms: u64,
) -> (Option<H264State>, Option<Vec<u8>>) {
    // H.264 needs even dimensions: drop a trailing odd column/row
    let (ew, eh) = (w & !1, h & !1);
    let mut st = match st {
        Some(s) if s.w == ew && s.h == eh => s,
        _ => match H264State::new(ew, eh, bitrate) {
            Ok(s) => s,
            Err(e) => {
                crate::util::log(&format!("H.264 编码器创建失败: {e}"));
                return (None, None);
            }
        },
    };
    st.set_bitrate(bitrate);
    if ew == w {
        st.yuv.read_bgra8(BgraSliceU8::new(&img[..w * eh * 4], (ew, eh)));
    } else {
        let mut cropped = Vec::with_capacity(ew * eh * 4);
        for y in 0..eh {
            cropped.extend_from_slice(&img[y * w * 4..(y * w + ew) * 4]);
        }
        st.yuv.read_bgra8(BgraSliceU8::new(&cropped, (ew, eh)));
    }
    if force_key {
        st.enc.force_intra_frame();
    }
    let (data, key) = match st.enc.encode_at(&st.yuv, Timestamp::from_millis(ts_ms)) {
        Ok(bs) => (bs.to_vec(), matches!(bs.frame_type(), FrameType::IDR | FrameType::I)),
        Err(e) => {
            crate::util::log(&format!("H.264 编码失败: {e}"));
            return (None, None);
        }
    };
    st.prev = Some(img);
    if !st.threaded {
        // first frame initialised the encoder; switch to multi-slice (forces a new keyframe)
        st.enable_threads();
    }
    if data.is_empty() {
        return (Some(st), None); // skipped by rate control
    }
    let mut msg = Vec::with_capacity(10 + data.len());
    msg.push(2u8);
    msg.extend_from_slice(&0u32.to_le_bytes());
    msg.extend_from_slice(&(ew as u16).to_le_bytes());
    msg.extend_from_slice(&(eh as u16).to_le_bytes());
    msg.push(key as u8);
    msg.extend_from_slice(&data);
    (Some(st), Some(msg))
}
