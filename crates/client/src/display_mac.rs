//! 解码与显示：把 HEVC 访问单元包装成 CMSampleBuffer，交给 AVSampleBufferDisplayLayer。
//!
//! 该图层内部用 VideoToolbox 硬件解码并直接上屏（含色彩管理），每个样本都标记
//! DisplayImmediately，解完即显示，不做任何缓冲/时间戳同步。

use anyhow::{bail, Result};
use objc2::encode::{Encoding, RefEncode};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_foundation::NSRect;
use rc_protocol::hevc;
use std::ffi::{c_int, c_void};
use std::ptr::{null, null_mut};

macro_rules! opaque {
    ($name:ident, $objc_name:literal) => {
        #[repr(C)]
        pub struct $name([u8; 0]);
        unsafe impl RefEncode for $name {
            const ENCODING_REF: Encoding = Encoding::Pointer(&Encoding::Struct($objc_name, &[]));
        }
    };
}
opaque!(CMSampleBuffer, "opaqueCMSampleBuffer");
opaque!(CGColor, "CGColor");

type CFTypeRef = *const c_void;
type OSStatus = i32;

#[repr(C)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

#[repr(C)]
struct CMSampleTimingInfo {
    duration: CMTime,
    pts: CMTime,
    dts: CMTime,
}

const K_CM_BLOCK_BUFFER_ASSURE_MEMORY_NOW: u32 = 1;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFBooleanTrue: CFTypeRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: isize) -> CFTypeRef;
    fn CFDictionarySetValue(dict: CFTypeRef, key: CFTypeRef, value: CFTypeRef);
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    static kCGColorBlack: CFTypeRef;
    fn CGColorGetConstantColor(name: CFTypeRef) -> *const CGColor;
}

#[link(name = "CoreMedia", kind = "framework")]
extern "C" {
    static kCMSampleAttachmentKey_DisplayImmediately: CFTypeRef;
    fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
        allocator: CFTypeRef,
        count: usize,
        pointers: *const *const u8,
        sizes: *const usize,
        nal_unit_header_length: c_int,
        extensions: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: CFTypeRef,
        memory_block: *mut c_void,
        block_length: usize,
        block_allocator: CFTypeRef,
        custom_block_source: *const c_void,
        offset_to_data: usize,
        data_length: usize,
        flags: u32,
        out: *mut CFTypeRef,
    ) -> OSStatus;
    fn CMBlockBufferReplaceDataBytes(src: *const c_void, dest: CFTypeRef, offset: usize, len: usize) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: CFTypeRef,
        data_buffer: CFTypeRef,
        format: CFTypeRef,
        num_samples: isize,
        num_timing_entries: isize,
        timing: *const CMSampleTimingInfo,
        num_size_entries: isize,
        sizes: *const usize,
        out: *mut *mut CMSampleBuffer,
    ) -> OSStatus;
    fn CMSampleBufferGetSampleAttachmentsArray(sbuf: *mut CMSampleBuffer, create_if_necessary: u8) -> CFTypeRef;
}

// 让 AVSampleBufferDisplayLayer 这个类在运行时可用。
#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}
#[link(name = "QuartzCore", kind = "framework")]
extern "C" {}

const AV_STATUS_FAILED: isize = 2;
const CA_LAYER_WIDTH_SIZABLE: u32 = 1 << 1;
const CA_LAYER_HEIGHT_SIZABLE: u32 = 1 << 4;

pub struct VideoSink {
    layer: Retained<AnyObject>,
    format: CFTypeRef,
    /// 当前格式描述对应的 VPS/SPS/PPS，变化时（例如分辨率变了）重建格式描述。
    params: [Vec<u8>; 3],
    scratch: Vec<u8>,
}

// 图层的 enqueue/flush/status 是线程安全的；其余字段只在接收线程使用。
unsafe impl Send for VideoSink {}

impl VideoSink {
    /// 在 `ns_view` 上挂一个铺满的视频图层。必须在主线程调用。
    ///
    /// # Safety
    /// `ns_view` 必须是有效的 NSView 指针。
    pub unsafe fn attach(ns_view: *mut c_void) -> Result<Self> {
        let view = ns_view as *mut AnyObject;
        let Some(cls) = AnyClass::get(c"AVSampleBufferDisplayLayer") else {
            bail!("找不到 AVSampleBufferDisplayLayer");
        };
        let layer: Retained<AnyObject> = msg_send![cls, new];

        let _: () = msg_send![view, setWantsLayer: true];
        let root: *mut AnyObject = msg_send![view, layer];
        if root.is_null() {
            bail!("NSView 没有 layer");
        }
        let black = CGColorGetConstantColor(kCGColorBlack);
        let _: () = msg_send![root, setBackgroundColor: black];
        let bounds: NSRect = msg_send![root, bounds];
        let _: () = msg_send![&*layer, setFrame: bounds];
        let _: () = msg_send![&*layer, setAutoresizingMask: CA_LAYER_WIDTH_SIZABLE | CA_LAYER_HEIGHT_SIZABLE];
        // videoGravity 默认即 ResizeAspect：等比缩放、留黑边。
        let _: () = msg_send![root, addSublayer: &*layer];

        Ok(Self { layer, format: null(), params: Default::default(), scratch: Vec::new() })
    }

    /// 提交一个访问单元。返回 `false` 表示无法解码（缺参数集或解码器出错），调用方应请求关键帧。
    pub fn push(&mut self, au: &[u8]) -> bool {
        // 1. 收集参数集；2. 其余需要送解码器的 NAL 转成 4 字节长度前缀（hvcC 格式）。
        let mut new_params: [Option<&[u8]>; 3] = [None; 3];
        self.scratch.clear();
        for nal in hevc::nals(au) {
            let t = hevc::nal_type(nal);
            match t {
                hevc::NAL_VPS => new_params[0] = Some(nal),
                hevc::NAL_SPS => new_params[1] = Some(nal),
                hevc::NAL_PPS => new_params[2] = Some(nal),
                // 切片数据，以及前/后缀 SEI
                _ if hevc::is_vcl(t) || t == 39 || t == 40 => {
                    self.scratch.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    self.scratch.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if let [Some(vps), Some(sps), Some(pps)] = new_params {
            if self.format.is_null() || [vps, sps, pps] != [&self.params[0][..], &self.params[1][..], &self.params[2][..]] {
                if !self.set_format(vps, sps, pps) {
                    return false;
                }
            }
        }
        if self.format.is_null() || self.scratch.is_empty() {
            return false;
        }

        unsafe {
            let failed: isize = msg_send![&*self.layer, status];
            let needs_flush: bool = msg_send![&*self.layer, requiresFlushToResumeDecoding];
            if failed == AV_STATUS_FAILED || needs_flush {
                let _: () = msg_send![&*self.layer, flush];
                // flush 之后必须从关键帧重新开始
                if !hevc::contains_irap(au) {
                    return false;
                }
            }

            let len = self.scratch.len();
            let mut block: CFTypeRef = null();
            if CMBlockBufferCreateWithMemoryBlock(null(), null_mut(), len, null(), null(), 0, len, K_CM_BLOCK_BUFFER_ASSURE_MEMORY_NOW, &mut block) != 0 {
                return false;
            }
            let mut sample: *mut CMSampleBuffer = null_mut();
            let ok = CMBlockBufferReplaceDataBytes(self.scratch.as_ptr().cast(), block, 0, len) == 0
                && CMSampleBufferCreateReady(null(), block, self.format, 1, 0, null(), 1, &len, &mut sample) == 0;
            CFRelease(block);
            if !ok {
                return false;
            }

            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 1);
            if !attachments.is_null() {
                let dict = CFArrayGetValueAtIndex(attachments, 0);
                CFDictionarySetValue(dict, kCMSampleAttachmentKey_DisplayImmediately, kCFBooleanTrue);
            }
            let _: () = msg_send![&*self.layer, enqueueSampleBuffer: sample];
            CFRelease(sample as CFTypeRef);
        }
        true
    }

    fn set_format(&mut self, vps: &[u8], sps: &[u8], pps: &[u8]) -> bool {
        let pointers = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
        let sizes = [vps.len(), sps.len(), pps.len()];
        let mut format: CFTypeRef = null();
        let status = unsafe { CMVideoFormatDescriptionCreateFromHEVCParameterSets(null(), 3, pointers.as_ptr(), sizes.as_ptr(), 4, null(), &mut format) };
        if status != 0 || format.is_null() {
            eprintln!("创建视频格式描述失败: {status}");
            return false;
        }
        if !self.format.is_null() {
            unsafe { CFRelease(self.format) };
        }
        self.format = format;
        self.params = [vps.to_vec(), sps.to_vec(), pps.to_vec()];
        true
    }
}

impl Drop for VideoSink {
    fn drop(&mut self) {
        if !self.format.is_null() {
            unsafe { CFRelease(self.format) };
        }
    }
}
