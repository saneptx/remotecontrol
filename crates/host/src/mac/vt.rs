//! VideoToolbox HEVC 硬件编码：实时、无 B 帧、只在请求时出关键帧，输出 Annex-B（关键帧带 VPS/SPS/PPS）。
//!
//! 两个方向统一用 HEVC 4:2:0（Main）：码率低，各家硬件编解码器都支持。

use anyhow::{bail, Result};
use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::sync::Mutex;

type OSStatus = i32;
type CFTypeRef = *const c_void;
type VTSession = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CMTIME_VALID: u32 = 1;
const INVALID_TIME: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
const CODEC_HEVC: u32 = u32::from_be_bytes(*b"hvc1");
const CF_NUMBER_SINT32: i32 = 3;
const CF_NUMBER_DOUBLE: i32 = 13;

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    static kCFTypeArrayCallBacks: c_void;
    fn CFRelease(cf: CFTypeRef);
    fn CFNumberCreate(alloc: CFTypeRef, ty: i32, value: *const c_void) -> CFTypeRef;
    fn CFDictionaryCreate(alloc: CFTypeRef, keys: *const CFTypeRef, values: *const CFTypeRef, n: isize, kcb: *const c_void, vcb: *const c_void) -> CFTypeRef;
    fn CFArrayCreate(alloc: CFTypeRef, values: *const CFTypeRef, n: isize, cb: *const c_void) -> CFTypeRef;
    fn CFArrayGetCount(a: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(a: CFTypeRef, i: isize) -> CFTypeRef;
    fn CFDictionaryGetValue(d: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFTypeRef;
    fn CMSampleBufferGetDataBuffer(sbuf: *mut c_void) -> *mut c_void;
    fn CMSampleBufferGetFormatDescription(sbuf: *mut c_void) -> CFTypeRef;
    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: *mut c_void, create: u8) -> CFTypeRef;
    fn CMBlockBufferGetDataLength(b: *mut c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(b: *mut c_void, offset: usize, len: usize, dest: *mut c_void) -> OSStatus;
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        desc: CFTypeRef,
        index: usize,
        ptr: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_len: *mut i32,
    ) -> OSStatus;
}

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    static kVTCompressionPropertyKey_RealTime: CFTypeRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFTypeRef;
    static kVTCompressionPropertyKey_MaxKeyFrameInterval: CFTypeRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFTypeRef;
    static kVTCompressionPropertyKey_DataRateLimits: CFTypeRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFTypeRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFTypeRef;
    static kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality: CFTypeRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFTypeRef;
    static kVTProfileLevel_HEVC_Main_AutoLevel: CFTypeRef;
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: CFTypeRef;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: CFTypeRef;
    fn VTCompressionSessionCreate(
        alloc: CFTypeRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_spec: CFTypeRef,
        source_attrs: CFTypeRef,
        compressed_alloc: CFTypeRef,
        callback: OutputCallback,
        refcon: *mut c_void,
        out: *mut VTSession,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: VTSession, key: CFTypeRef, value: CFTypeRef) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: VTSession) -> OSStatus;
    fn VTCompressionSessionEncodeFrame(
        session: VTSession,
        image: *mut c_void,
        pts: CMTime,
        duration: CMTime,
        props: CFTypeRef,
        source_ref: *mut c_void,
        info: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionCompleteFrames(session: VTSession, until: CMTime) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: VTSession);
}

fn cf_i32(v: i32) -> CFTypeRef {
    unsafe { CFNumberCreate(null(), CF_NUMBER_SINT32, (&v as *const i32).cast()) }
}

fn cf_f64(v: f64) -> CFTypeRef {
    unsafe { CFNumberCreate(null(), CF_NUMBER_DOUBLE, (&v as *const f64).cast()) }
}

fn cf_dict(pairs: &[(CFTypeRef, CFTypeRef)]) -> CFTypeRef {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|p| p.0).collect();
    let vals: Vec<CFTypeRef> = pairs.iter().map(|p| p.1).collect();
    unsafe {
        CFDictionaryCreate(null(), keys.as_ptr(), vals.as_ptr(), pairs.len() as isize, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks)
    }
}

/// 编码回调写入的结果。
type Output = Mutex<Option<(Vec<u8>, bool)>>;

pub struct Encoder {
    session: VTSession,
    output: Box<Output>,
    pub width: u32,
    pub height: u32,
}

unsafe impl Send for Encoder {}

extern "C" fn on_output(refcon: *mut c_void, _src: *mut c_void, status: OSStatus, _flags: u32, sample: *mut c_void) {
    if status != 0 || sample.is_null() {
        return;
    }
    let out = unsafe { &*(refcon as *const Output) };
    if let Some(au) = unsafe { to_annex_b(sample) } {
        *out.lock().unwrap() = Some(au);
    }
}

/// 把 VideoToolbox 输出（长度前缀的 NAL）转成 Annex-B。返回 (数据, 是否关键帧)。
unsafe fn to_annex_b(sample: *mut c_void) -> Option<(Vec<u8>, bool)> {
    let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
    let keyframe = attachments.is_null()
        || CFArrayGetCount(attachments) == 0
        || CFDictionaryGetValue(CFArrayGetValueAtIndex(attachments, 0), kCMSampleAttachmentKey_NotSync).is_null();

    let mut out = Vec::new();
    let mut nal_len = 4i32;
    if keyframe {
        let desc = CMSampleBufferGetFormatDescription(sample);
        let mut count = 0usize;
        let (mut p, mut sz) = (null(), 0usize);
        if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(desc, 0, &mut p, &mut sz, &mut count, &mut nal_len) != 0 {
            return None;
        }
        for i in 0..count {
            if CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(desc, i, &mut p, &mut sz, null_mut(), null_mut()) == 0 {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(std::slice::from_raw_parts(p, sz));
            }
        }
    }
    let block = CMSampleBufferGetDataBuffer(sample);
    if block.is_null() {
        return None;
    }
    let len = CMBlockBufferGetDataLength(block);
    let mut data = vec![0u8; len];
    if CMBlockBufferCopyDataBytes(block, 0, len, data.as_mut_ptr().cast()) != 0 {
        return None;
    }
    let n = nal_len.clamp(1, 4) as usize;
    let mut i = 0;
    while i + n <= data.len() {
        let size = data[i..i + n].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        i += n;
        if i + size > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i..i + size]);
        i += size;
    }
    Some((out, keyframe))
}

impl Encoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_mbps: u32) -> Result<Self> {
        let output: Box<Output> = Box::new(Mutex::new(None));
        unsafe {
            let mut session: VTSession = null_mut();
            let create = |spec: CFTypeRef, session: &mut VTSession| {
                VTCompressionSessionCreate(
                    null(),
                    width as i32,
                    height as i32,
                    CODEC_HEVC,
                    spec,
                    null(),
                    null(),
                    on_output,
                    (&*output as *const Output) as *mut c_void,
                    session,
                )
            };
            // 优先用低延迟码控（帧级码率控制，几乎不缓冲）；不支持时退回普通模式
            let low_latency = cf_dict(&[
                (kVTVideoEncoderSpecification_EnableLowLatencyRateControl, kCFBooleanTrue),
                (kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue),
            ]);
            let mut status = create(low_latency, &mut session);
            CFRelease(low_latency);
            if status != 0 {
                let hw = cf_dict(&[(kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, kCFBooleanTrue)]);
                status = create(hw, &mut session);
                CFRelease(hw);
            }
            if status != 0 {
                bail!("创建 HEVC 硬件编码器失败: {status}");
            }

            let set = |key: CFTypeRef, value: CFTypeRef, own: bool| {
                VTSessionSetProperty(session, key, value);
                if own {
                    CFRelease(value);
                }
            };
            set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, false);
            set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, false);
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_HEVC_Main_AutoLevel, false);
            set(kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality, kCFBooleanTrue, false);
            set(kVTCompressionPropertyKey_MaxKeyFrameInterval, cf_i32(i32::MAX), true);
            set(kVTCompressionPropertyKey_ExpectedFrameRate, cf_i32(fps as i32), true);
            let bps = bitrate_mbps.saturating_mul(1_000_000) as i32;
            set(kVTCompressionPropertyKey_AverageBitRate, cf_i32(bps), true);
            // 每秒最多 1.5 倍平均码率，避免突发
            let (bytes, secs) = (cf_f64(bps as f64 / 8.0 * 1.5), cf_f64(1.0));
            let limits = CFArrayCreate(null(), [bytes, secs].as_ptr(), 2, &kCFTypeArrayCallBacks);
            CFRelease(bytes);
            CFRelease(secs);
            set(kVTCompressionPropertyKey_DataRateLimits, limits, true);
            VTCompressionSessionPrepareToEncodeFrames(session);
            Ok(Self { session, output, width, height })
        }
    }

    /// 同步编码一帧（`pixel_buffer` 为 CVPixelBufferRef）。
    pub fn encode(&mut self, pixel_buffer: *mut c_void, pts_us: u64, force_key: bool) -> Result<Option<(Vec<u8>, bool)>> {
        unsafe {
            let props = force_key.then(|| cf_dict(&[(kVTEncodeFrameOptionKey_ForceKeyFrame, kCFBooleanTrue)]));
            let pts = CMTime { value: pts_us as i64, timescale: 1_000_000, flags: CMTIME_VALID, epoch: 0 };
            let status = VTCompressionSessionEncodeFrame(self.session, pixel_buffer, pts, INVALID_TIME, props.unwrap_or(null()), null_mut(), null_mut());
            if let Some(p) = props {
                CFRelease(p);
            }
            if status != 0 {
                bail!("编码失败: {status}");
            }
            VTCompressionSessionCompleteFrames(self.session, INVALID_TIME);
        }
        Ok(self.output.lock().unwrap().take())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session as CFTypeRef);
        }
    }
}
