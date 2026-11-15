//! NVENC HEVC 4:4:4 超低延迟编码。输入是已注册的 D3D11 BGRA 纹理，输出 Annex-B 码流。
//!
//! 配置要点：P1 预设 + ULTRA_LOW_LATENCY 调优、无 B 帧、无限 GOP（只在请求时出 IDR）、
//! CBR 且 VBV 只有一帧大小——每帧大小稳定，网络侧不会出现突发排队。

use crate::nvenc_sys::*;
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::c_void;
use std::ptr::null_mut;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

type CreateInstance = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;
type GetMaxVersion = unsafe extern "C" fn(*mut u32) -> NVENCSTATUS;

pub struct Encoder {
    api: NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    bitstream: *mut c_void,
    registered: *mut c_void,
    width: u32,
    height: u32,
    frame_idx: u64,
    // 保持纹理、设备与 DLL 存活到编码器销毁之后（字段按声明顺序析构，Drop::drop 先于它们执行）。
    _texture: ID3D11Texture2D,
    _device: ID3D11Device,
    _lib: libloading::Library,
}

fn check(status: NVENCSTATUS, what: &str) -> Result<()> {
    if status == _NVENCSTATUS::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(anyhow!("NVENC {what} 失败: {status:?}"))
    }
}

impl Encoder {
    pub fn new(device: &ID3D11Device, texture: &ID3D11Texture2D, width: u32, height: u32, fps: u32, bitrate_mbps: u32) -> Result<Self> {
        unsafe {
            let lib = libloading::Library::new("nvEncodeAPI64.dll").context("加载 nvEncodeAPI64.dll 失败（需要 NVIDIA 显卡及驱动）")?;

            let mut max_ver = 0u32;
            let get_max: libloading::Symbol<GetMaxVersion> = lib.get(b"NvEncodeAPIGetMaxSupportedVersion\0")?;
            check(get_max(&mut max_ver), "GetMaxSupportedVersion")?;
            let need = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
            if max_ver < need {
                bail!("显卡驱动过旧：需要 NVENC API {NVENCAPI_MAJOR_VERSION}.{NVENCAPI_MINOR_VERSION}，驱动仅支持 {}.{}，请升级驱动", max_ver >> 4, max_ver & 0xf);
            }

            let mut api = NV_ENCODE_API_FUNCTION_LIST { version: NV_ENCODE_API_FUNCTION_LIST_VER, ..Default::default() };
            let create: libloading::Symbol<CreateInstance> = lib.get(b"NvEncodeAPICreateInstance\0")?;
            check(create(&mut api), "CreateInstance")?;

            let mut open = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: _NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
                device: device.as_raw(),
                apiVersion: NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut enc = null_mut();
            check(
                api.nvEncOpenEncodeSessionEx.context("nvEncOpenEncodeSessionEx")?(&mut open, &mut enc),
                "OpenEncodeSession（显示器是否接在 NVIDIA 显卡上？混合显卡笔记本暂不支持）",
            )?;

            // 之后任何一步失败都由 Drop 负责清理。
            let mut this = Self {
                api,
                enc,
                bitstream: null_mut(),
                registered: null_mut(),
                width,
                height,
                frame_idx: 0,
                _texture: texture.clone(),
                _device: device.clone(),
                _lib: lib,
            };
            this.init(texture, fps, bitrate_mbps)?;
            Ok(this)
        }
    }

    unsafe fn init(&mut self, texture: &ID3D11Texture2D, fps: u32, bitrate_mbps: u32) -> Result<()> {
        let api = &self.api;
        let preset = NV_ENC_PRESET_P1_GUID;
        let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;

        // 以预设为基础再改。结构体很大（几 KB），放堆上。
        let mut preset_cfg: Box<NV_ENC_PRESET_CONFIG> = Box::default();
        preset_cfg.version = NV_ENC_PRESET_CONFIG_VER;
        preset_cfg.presetCfg.version = NV_ENC_CONFIG_VER;
        check(
            api.nvEncGetEncodePresetConfigEx.context("nvEncGetEncodePresetConfigEx")?(self.enc, NV_ENC_CODEC_HEVC_GUID, preset, tuning, &mut *preset_cfg),
            "GetEncodePresetConfigEx",
        )?;

        let mut config: Box<NV_ENC_CONFIG> = Box::new(preset_cfg.presetCfg);
        config.version = NV_ENC_CONFIG_VER;
        config.profileGUID = NV_ENC_HEVC_PROFILE_FREXT_GUID; // 4:4:4 需要 Range Extensions
        config.gopLength = NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1; // 无 B 帧

        let bitrate = bitrate_mbps.saturating_mul(1_000_000);
        let rc = &mut config.rcParams;
        rc.rateControlMode = _NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        rc.averageBitRate = bitrate;
        rc.maxBitRate = bitrate;
        rc.vbvBufferSize = bitrate / fps; // 一帧
        rc.vbvInitialDelay = rc.vbvBufferSize;

        let hevc = &mut config.encodeCodecConfig.hevcConfig;
        hevc.set_chromaFormatIDC(3); // YUV 4:4:4
        hevc.set_repeatSPSPPS(1); // 每个 IDR 都带参数集，控制端可随时加入
        hevc.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        // NVENC 内部把 BGRA 按 BT.601 limited 转成 YUV；在 VUI 里如实声明，解码端才能精确还原。
        let vui = &mut hevc.hevcVUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = _NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 0;
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = _NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vui.transferCharacteristics = _NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SRGB;
        vui.colourMatrix = _NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT470BG;

        let mut init: Box<NV_ENC_INITIALIZE_PARAMS> = Box::default();
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = NV_ENC_CODEC_HEVC_GUID;
        init.presetGUID = preset;
        init.tuningInfo = tuning;
        init.encodeWidth = self.width;
        init.encodeHeight = self.height;
        init.darWidth = self.width;
        init.darHeight = self.height;
        init.maxEncodeWidth = self.width;
        init.maxEncodeHeight = self.height;
        init.frameRateNum = fps;
        init.frameRateDen = 1;
        init.enableEncodeAsync = 0; // 同步模式：EncodePicture 之后直接 LockBitstream 取结果
        init.enablePTD = 1;
        init.encodeConfig = &mut *config;
        check(
            api.nvEncInitializeEncoder.context("nvEncInitializeEncoder")?(self.enc, &mut *init),
            "InitializeEncoder（该显卡可能不支持 HEVC 4:4:4 编码，需要 GTX 10 系及以上）",
        )?;

        let mut bs = NV_ENC_CREATE_BITSTREAM_BUFFER { version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER, ..Default::default() };
        check(api.nvEncCreateBitstreamBuffer.context("nvEncCreateBitstreamBuffer")?(self.enc, &mut bs), "CreateBitstreamBuffer")?;
        self.bitstream = bs.bitstreamBuffer;

        let mut reg = NV_ENC_REGISTER_RESOURCE {
            version: NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: _NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width: self.width,
            height: self.height,
            pitch: 0,
            subResourceIndex: 0,
            resourceToRegister: texture.as_raw(),
            bufferFormat: _NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB, // 即 D3D 的 B8G8R8A8
            bufferUsage: _NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        check(api.nvEncRegisterResource.context("nvEncRegisterResource")?(self.enc, &mut reg), "RegisterResource")?;
        self.registered = reg.registeredResource;
        Ok(())
    }

    /// 编码已注册纹理的当前内容。返回 (Annex-B 数据, 是否关键帧)。
    pub fn encode(&mut self, force_idr: bool) -> Result<(Vec<u8>, bool)> {
        unsafe {
            let api = &self.api;
            let mut map = NV_ENC_MAP_INPUT_RESOURCE {
                version: NV_ENC_MAP_INPUT_RESOURCE_VER,
                registeredResource: self.registered,
                ..Default::default()
            };
            check(api.nvEncMapInputResource.context("nvEncMapInputResource")?(self.enc, &mut map), "MapInputResource")?;

            let mut pic: Box<NV_ENC_PIC_PARAMS> = Box::default();
            pic.version = NV_ENC_PIC_PARAMS_VER;
            pic.inputWidth = self.width;
            pic.inputHeight = self.height;
            pic.inputPitch = 0;
            pic.inputBuffer = map.mappedResource;
            pic.bufferFmt = map.mappedBufferFmt;
            pic.outputBitstream = self.bitstream;
            pic.pictureStruct = _NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
            pic.inputTimeStamp = self.frame_idx;
            if force_idr {
                pic.encodePicFlags =
                    _NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32 | _NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32;
            }
            self.frame_idx += 1;

            let encoded = check(api.nvEncEncodePicture.context("nvEncEncodePicture")?(self.enc, &mut *pic), "EncodePicture").and_then(|()| {
                let mut lock: Box<NV_ENC_LOCK_BITSTREAM> = Box::default();
                lock.version = NV_ENC_LOCK_BITSTREAM_VER;
                lock.outputBitstream = self.bitstream;
                check(api.nvEncLockBitstream.context("nvEncLockBitstream")?(self.enc, &mut *lock), "LockBitstream")?;
                let data = std::slice::from_raw_parts(lock.bitstreamBufferPtr as *const u8, lock.bitstreamSizeInBytes as usize).to_vec();
                let keyframe = lock.pictureType == _NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
                if let Some(unlock) = api.nvEncUnlockBitstream {
                    unlock(self.enc, self.bitstream);
                }
                Ok((data, keyframe))
            });

            if let Some(unmap) = api.nvEncUnmapInputResource {
                unmap(self.enc, map.mappedResource);
            }
            encoded
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if !self.registered.is_null() {
                if let Some(f) = self.api.nvEncUnregisterResource {
                    f(self.enc, self.registered);
                }
            }
            if !self.bitstream.is_null() {
                if let Some(f) = self.api.nvEncDestroyBitstreamBuffer {
                    f(self.enc, self.bitstream);
                }
            }
            if let Some(f) = self.api.nvEncDestroyEncoder {
                f(self.enc);
            }
        }
    }
}
