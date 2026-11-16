//! Media Foundation 硬件 HEVC 编码：没有 NVIDIA 显卡（或显示器接在 Intel/AMD 显卡上）时的备选。
//!
//! 显卡驱动（Intel Quick Sync、AMD VCN、NVIDIA）都会注册一个硬件 HEVC 编码 MFT。流程：
//! 采集到的 BGRA 纹理 → D3D11 视频处理器（缩放 + 转 NV12，BT.709 有限范围）→ 硬件 MFT → Annex-B。
//! 硬件 MFT 都是异步的：等 `METransformNeedInput` 再送一帧，等 `METransformHaveOutput` 再取码流。
//! 每次只送一帧、取到输出再返回，和 NVENC 的同步模式一样，不排队、不攒帧。

use anyhow::{bail, Context, Result};
use rc_protocol::hevc;
use std::mem::ManuallyDrop;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::VARIANT_BOOL;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_BOOL, VT_UI4};

pub struct Encoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    vctx: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
    nv12: ID3D11Texture2D,
    /// 编码器还能接收几帧（收到 NeedInput 加一，送一帧减一）
    credits: u32,
    /// VPS/SPS/PPS：有的编码器只放在输出格式里，关键帧缺参数集时补上
    headers: Vec<u8>,
    fps: u32,
    frame_idx: u64,
    /// 编码器名称（日志用）
    pub name: String,
    _manager: IMFDXGIDeviceManager,
}

// 只在采集线程里使用
unsafe impl Send for Encoder {}

fn var_u32(v: u32) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 { vt: VT_UI4, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { ulVal: v } }),
        },
    }
}

fn var_bool(v: bool) -> VARIANT {
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BOOL,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 { boolVal: VARIANT_BOOL(if v { -1 } else { 0 }) },
            }),
        },
    }
}

/// 采集设备所在显卡的 LUID，用来挑同一块显卡上的编码器。
unsafe fn adapter_luid(device: &ID3D11Device) -> Option<u64> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let desc = dxgi.GetAdapter().ok()?.GetDesc().ok()?;
    let l = desc.AdapterLuid;
    Some(((l.HighPart as u32 as u64) << 32) | l.LowPart as u64)
}

/// 枚举硬件 HEVC 编码器，优先和采集同一块显卡的。
unsafe fn find_encoders(device: &ID3D11Device) -> Result<Vec<(IMFActivate, String)>> {
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_HEVC };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        None,
        Some(&output),
        &mut list,
        &mut count,
    )
    .context("枚举 HEVC 编码器")?;
    let luid = adapter_luid(device);
    let mut same = Vec::new();
    let mut other = Vec::new();
    if !list.is_null() {
        for i in 0..count as usize {
            let Some(act) = (*list.add(i)).take() else { continue };
            let name = friendly_name(&act);
            // MFT_ENUM_ADAPTER_LUID 是 UINT64；取不到时不做筛选
            match (luid, act.GetUINT64(&MFT_ENUM_ADAPTER_LUID).ok()) {
                (Some(a), Some(b)) if a != b => other.push((act, name)),
                _ => same.push((act, name)),
            }
        }
        CoTaskMemFree(Some(list as *const _));
    }
    // 其它显卡上的编码器用不了采集设备的纹理，只作为最后的尝试
    same.extend(other);
    Ok(same)
}

unsafe fn friendly_name(act: &IMFActivate) -> String {
    let mut buf = [0u16; 128];
    let mut len = 0u32;
    if act.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, Some(&mut len)).is_ok() {
        return String::from_utf16_lossy(&buf[..len as usize]);
    }
    "硬件 HEVC 编码器".into()
}

impl Encoder {
    /// `src` 为 `src_size` 的 BGRA 纹理；编码分辨率为 `size`（不同时由视频处理器缩放）。
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        src_size: (u32, u32),
        size: (u32, u32),
        fps: u32,
        bitrate_mbps: u32,
    ) -> Result<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
            // 编码器在它自己的线程里也会用这个设备
            if let Ok(mt) = device.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(device, token).context("IMFDXGIDeviceManager::ResetDevice")?;

            let (vctx, processor, input_view, output_view, nv12) = make_converter(device, context, src, src_size, size, fps)?;

            let encoders = find_encoders(device)?;
            if encoders.is_empty() {
                bail!("没有找到硬件 HEVC 编码器（显卡太旧或驱动未安装）");
            }
            let mut last_err = None;
            for (act, name) in encoders {
                match open_mft(&act, &manager, size, fps, bitrate_mbps) {
                    Ok((mft, codec, headers)) => {
                        let events: IMFMediaEventGenerator = mft.cast().context("编码器不是异步 MFT")?;
                        return Ok(Self {
                            mft,
                            events,
                            codec,
                            vctx,
                            processor,
                            input_view,
                            output_view,
                            nv12,
                            credits: 0,
                            headers,
                            fps,
                            frame_idx: 0,
                            name,
                            _manager: manager,
                        });
                    }
                    Err(e) => {
                        eprintln!("编码器「{name}」不可用: {e:#}");
                        let _ = act.ShutdownObject();
                        last_err = Some(e);
                    }
                }
            }
            Err(last_err.unwrap())
        }
    }

    /// 编码源纹理的当前内容。返回 (Annex-B 数据, 是否关键帧)。
    pub fn encode(&mut self, force_idr: bool) -> Result<(Vec<u8>, bool)> {
        unsafe {
            // BGRA → NV12（顺带缩放）
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
                ..Default::default()
            };
            let streams = [stream];
            let r = self.vctx.VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams);
            let [stream] = streams;
            drop(ManuallyDrop::into_inner(stream.pInputSurface));
            r.context("VideoProcessorBlt(NV12)")?;

            while self.credits == 0 {
                self.wait_event()?;
            }

            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &self.nv12, 0, false)?;
            if let Ok(b2) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(b2.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let dur = 10_000_000 / self.fps as i64;
            sample.SetSampleTime(self.frame_idx as i64 * dur)?;
            sample.SetSampleDuration(dur)?;
            self.frame_idx += 1;
            if force_idr {
                if let Some(c) = &self.codec {
                    let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &var_u32(1));
                }
            }
            self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")?;
            self.credits -= 1;

            // 等这一帧的码流
            loop {
                if let Some(out) = self.wait_event()? {
                    return Ok(out);
                }
            }
        }
    }

    /// 处理一个编码器事件；有码流时返回它。
    unsafe fn wait_event(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        // 阻塞等待：硬件编码器一帧只要几毫秒
        let ev = self.events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)).context("GetEvent")?;
        let kind = ev.GetType()?;
        if kind == METransformNeedInput.0 as u32 {
            self.credits += 1;
            Ok(None)
        } else if kind == METransformHaveOutput.0 as u32 {
            self.take_output()
        } else if kind == MEError.0 as u32 {
            bail!("编码器出错: {:?}", ev.GetStatus())
        } else {
            Ok(None)
        }
    }

    unsafe fn take_output(&mut self) -> Result<Option<(Vec<u8>, bool)>> {
        let info = self.mft.GetOutputStreamInfo(0)?;
        let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let own = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
            Some(s)
        };
        let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(own), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0u32;
        let r = self.mft.ProcessOutput(0, &mut out, &mut status);
        let sample = ManuallyDrop::take(&mut out[0].pSample);
        drop(ManuallyDrop::take(&mut out[0].pEvents));
        match r {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                // 编码器调整了输出格式：照它给的重新设置
                let t = self.mft.GetOutputAvailableType(0, 0)?;
                self.mft.SetOutputType(0, &t, 0)?;
                if let Some(h) = sequence_header(&t) {
                    self.headers = h;
                }
                return Ok(None);
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
            Err(e) => return Err(e).context("ProcessOutput"),
        }
        let Some(sample) = sample else { return Ok(None) };
        let buf = sample.ConvertToContiguousBuffer()?;
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buf.Lock(&mut ptr, None, Some(&mut len))?;
        let mut data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buf.Unlock();
        let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0
            || hevc::nals(&data).any(|n| !n.is_empty() && hevc::is_irap(hevc::nal_type(n)));
        if key && !self.headers.is_empty() && !hevc::nals(&data).any(|n| !n.is_empty() && hevc::nal_type(n) == hevc::NAL_VPS) {
            let mut v = self.headers.clone();
            v.extend_from_slice(&data);
            data = v;
        }
        Ok(Some((data, key)))
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if let Ok(s) = self.mft.cast::<IMFShutdown>() {
                let _ = s.Shutdown();
            }
        }
    }
}

unsafe fn sequence_header(t: &IMFMediaType) -> Option<Vec<u8>> {
    let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
    let mut v = vec![0u8; size as usize];
    t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut v, None).ok()?;
    Some(v)
}

/// 激活并配置一个编码器：返回 (MFT, ICodecAPI, 参数集)。
unsafe fn open_mft(
    act: &IMFActivate,
    manager: &IMFDXGIDeviceManager,
    size: (u32, u32),
    fps: u32,
    bitrate_mbps: u32,
) -> Result<(IMFTransform, Option<ICodecAPI>, Vec<u8>)> {
    let mft: IMFTransform = act.ActivateObject().context("ActivateObject")?;
    let attrs = mft.GetAttributes()?;
    attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("解锁异步 MFT")?;
    let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
    mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).context("设置 D3D 设备")?;

    let bitrate = bitrate_mbps.saturating_mul(1_000_000);
    let codec: Option<ICodecAPI> = mft.cast().ok();
    if let Some(c) = &codec {
        // 编码参数要在设置格式之前给
        let _ = c.SetValue(&CODECAPI_AVEncCommonRateControlMode, &var_u32(eAVEncCommonRateControlMode_CBR.0 as u32));
        let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &var_u32(bitrate));
        let _ = c.SetValue(&CODECAPI_AVEncCommonBufferSize, &var_u32(bitrate / fps)); // 一帧
        let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &var_bool(true));
        let _ = c.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &var_u32(0));
        // 关键帧只在请求时出；GOP 尽量长（各家上限不同，取一个都能接受的值）
        let _ = c.SetValue(&CODECAPI_AVEncMPVGOPSize, &var_u32(fps.saturating_mul(600).min(0xFFFF)));
        let _ = c.SetValue(&CODECAPI_AVEncCommonQualityVsSpeed, &var_u32(100)); // 速度优先
    }

    let frame_size = ((size.0 as u64) << 32) | size.1 as u64;
    let rate = ((fps as u64) << 32) | 1;
    let out = MFCreateMediaType()?;
    out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
    out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
    out.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
    out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
    out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?;
    mft.SetOutputType(0, &out, 0).context("设置输出格式（HEVC Main）")?;

    let input = find_input_type(&mft, &NV12)?;
    input.SetUINT64(&MF_MT_FRAME_SIZE, frame_size)?;
    input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
    input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    // 和视频处理器的输出一致：BT.709 有限范围。编码器据此写 VUI，控制端按它还原颜色
    input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
    input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    input.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
    input.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_sRGB.0 as u32)?;
    mft.SetInputType(0, &input, 0).context("设置输入格式（NV12）")?;

    let headers = mft.GetOutputCurrentType(0).ok().and_then(|t| sequence_header(&t)).unwrap_or_default();
    mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
    mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
    Ok((mft, codec, headers))
}

const NV12: GUID = MFVideoFormat_NV12;

/// 从编码器给出的输入格式里找 NV12（直接用它给的对象，带着驱动需要的私有属性）。
unsafe fn find_input_type(mft: &IMFTransform, subtype: &GUID) -> Result<IMFMediaType> {
    for i in 0.. {
        let Ok(t) = mft.GetInputAvailableType(0, i) else { break };
        if t.GetGUID(&MF_MT_SUBTYPE).ok().as_ref() == Some(subtype) {
            return Ok(t);
        }
    }
    // 有的编码器在设置输出格式前不列出输入格式：自己建一个
    let t = MFCreateMediaType()?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
    Ok(t)
}

type Converter = (ID3D11VideoContext, ID3D11VideoProcessor, ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, ID3D11Texture2D);

/// BGRA（全范围 RGB）→ NV12（BT.709 有限范围），可同时缩放。
unsafe fn make_converter(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    src_size: (u32, u32),
    dst: (u32, u32),
    fps: u32,
) -> Result<Converter> {
    let vdev: ID3D11VideoDevice = device.cast().context("ID3D11VideoDevice")?;
    let vctx: ID3D11VideoContext = context.cast().context("ID3D11VideoContext")?;
    let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
    let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: rate,
        InputWidth: src_size.0,
        InputHeight: src_size.1,
        OutputFrameRate: rate,
        OutputWidth: dst.0,
        OutputHeight: dst.1,
        Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
    };
    let enumerator = vdev.CreateVideoProcessorEnumerator(&desc).context("CreateVideoProcessorEnumerator")?;
    let processor = vdev.CreateVideoProcessor(&enumerator, 0).context("CreateVideoProcessor")?;

    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: dst.0,
        Height: dst.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut nv12 = None;
    device.CreateTexture2D(&tex_desc, None, Some(&mut nv12)).context("CreateTexture2D(NV12)")?;
    let nv12 = nv12.unwrap();

    let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
        FourCC: 0,
        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
    };
    let mut input = None;
    vdev.CreateVideoProcessorInputView(src, &enumerator, &in_desc, Some(&mut input)).context("CreateVideoProcessorInputView")?;
    let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
    };
    let mut output = None;
    vdev.CreateVideoProcessorOutputView(&nv12, &enumerator, &out_desc, Some(&mut output)).context("CreateVideoProcessorOutputView(NV12)")?;

    // 位域：bit2 = BT.709 矩阵，bit4-5 = 1 表示 16-235 有限范围；输入为全范围 RGB（全 0）
    let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
    let yuv709_limited = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
    vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb_full);
    vctx.VideoProcessorSetOutputColorSpace(&processor, &yuv709_limited);
    vctx.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
    vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
    Ok((vctx, processor, input.unwrap(), output.unwrap(), nv12))
}
