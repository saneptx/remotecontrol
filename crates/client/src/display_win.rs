//! Windows 解码与显示：Media Foundation 硬件解码 HEVC（解码结果留在显存里的 D3D11 纹理），
//! 再用 D3D11 视频处理器把 YUV 转成 RGB、等比缩放并加黑边，直接画到窗口的交换链上。全程不经过 CPU 拷贝。
//!
//! HEVC 解码依赖系统的“HEVC 视频扩展”（Microsoft Store，很多电脑已预装）。
//!
//! 所有 COM / D3D 对象都在第一次 `push` 时、在视频接收线程里创建，之后也只在这个线程使用。

use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;

struct Pipeline {
    context: ID3D11DeviceContext,
    vdevice: ID3D11VideoDevice,
    vcontext: ID3D11VideoContext,
    swap: IDXGISwapChain1,
    decoder: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    /// 解码输出的可见区域
    visible: (u32, u32),
    /// 视频处理器（随输入纹理尺寸、窗口尺寸重建）
    vp: Option<(ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor, (u32, u32), (u32, u32))>,
    swap_size: (u32, u32),
    frame: i64,
    /// 调试：RC_VIDEO_DEBUG=1 时每秒打印实际显示的帧数
    debug: Option<(std::time::Instant, u32)>,
}

pub struct VideoSink {
    hwnd: isize,
    pipe: Option<Pipeline>,
    failed: bool,
}

unsafe impl Send for VideoSink {}

impl VideoSink {
    /// # Safety
    /// `hwnd` 必须是有效的窗口句柄。
    pub unsafe fn attach(hwnd: *mut c_void) -> Result<Self> {
        Ok(Self { hwnd: hwnd as isize, pipe: None, failed: false })
    }

    /// 提交一个访问单元。返回 `false` 表示无法解码（调用方会请求关键帧）。
    pub fn push(&mut self, au: &[u8]) -> bool {
        if self.pipe.is_none() && !self.failed {
            match unsafe { Pipeline::new(HWND(self.hwnd as *mut c_void)) } {
                Ok(p) => self.pipe = Some(p),
                Err(e) => {
                    eprintln!("无法初始化视频解码: {e:#}");
                    self.failed = true;
                }
            }
        }
        let Some(p) = self.pipe.as_mut() else { return false };
        match unsafe { p.decode(au) } {
            Ok(ok) => ok,
            Err(e) => {
                eprintln!("解码出错: {e:#}");
                false
            }
        }
    }
}

/// 找一个同步的 HEVC 解码 MFT（系统的 HEVC 视频扩展）。
unsafe fn find_decoder() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_HEVC };
    for flags in [MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_ALL] {
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(MFT_CATEGORY_VIDEO_DECODER, flags, Some(&input), None, &mut list, &mut count)?;
        if count == 0 || list.is_null() {
            continue;
        }
        let items = std::slice::from_raw_parts_mut(list, count as usize);
        let mut found = None;
        for a in items.iter_mut() {
            if let Some(act) = a.take() {
                if found.is_none() {
                    if let Ok(t) = act.ActivateObject::<IMFTransform>() {
                        // 只用同步 MFT（异步 MFT 需要事件驱动，这里不支持）
                        let is_async = t.GetAttributes().ok().and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok()).unwrap_or(0) != 0;
                        if !is_async {
                            found = Some(t);
                        }
                    }
                }
            }
        }
        CoTaskMemFree(Some(list as *const c_void));
        if let Some(t) = found {
            return Ok(t);
        }
    }
    bail!("没有可用的 HEVC 解码器，请在 Microsoft Store 安装“HEVC 视频扩展”")
}

fn frame_size(t: &IMFMediaType) -> Option<(u32, u32)> {
    let v = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.ok()?;
    Some(((v >> 32) as u32, v as u32))
}

/// 可见区域（HEVC 的裁剪窗口）；没有时用整帧尺寸。
fn visible_size(t: &IMFMediaType) -> Option<(u32, u32)> {
    let mut area = MFVideoArea::default();
    let bytes = unsafe { std::slice::from_raw_parts_mut((&mut area as *mut MFVideoArea).cast::<u8>(), size_of::<MFVideoArea>()) };
    if unsafe { t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None) }.is_ok() && area.Area.cx > 0 {
        return Some((area.Area.cx as u32, area.Area.cy as u32));
    }
    frame_size(t)
}

impl Pipeline {
    unsafe fn new(hwnd: HWND) -> Result<Self> {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;

        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
        // 解码器在内部也会用这个设备，需要打开多线程保护
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }

        let mut token = 0u32;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.unwrap();
        manager.ResetDevice(&device, token)?;

        let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 0,
            Height: 0,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let swap = factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None).context("CreateSwapChainForHwnd")?;

        let decoder = find_decoder()?;
        if let Ok(attrs) = decoder.GetAttributes() {
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1); // 解出一帧马上输出，不攒帧
        }
        decoder.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).context("设置 D3D 管理器")?;

        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_HEVC)?;
        // 实际尺寸以码流里的 SPS 为准（解码器会发出格式变化）
        input.SetUINT64(&MF_MT_FRAME_SIZE, (1920u64 << 32) | 1080)?;
        input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        decoder.SetInputType(0, &input, 0).context("设置解码输入格式")?;

        let mut p = Self {
            context,
            vdevice: device.cast()?,
            vcontext: context_cast(&device)?,
            swap,
            decoder,
            _manager: manager,
            visible: (1920, 1080),
            vp: None,
            swap_size: (0, 0),
            frame: 0,
            debug: std::env::var("RC_VIDEO_DEBUG").is_ok_and(|v| v == "1").then(|| (std::time::Instant::now(), 0)),
        };
        p.set_output_type()?;
        let _ = p.decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
        let _ = p.decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        Ok(p)
    }

    /// 选 NV12 作为解码输出格式。
    unsafe fn set_output_type(&mut self) -> Result<()> {
        for i in 0.. {
            let Ok(t) = self.decoder.GetOutputAvailableType(0, i) else { break };
            if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                self.decoder.SetOutputType(0, &t, 0).context("设置解码输出格式")?;
                if let Some(v) = visible_size(&t) {
                    self.visible = v;
                }
                return Ok(());
            }
        }
        bail!("解码器不支持 NV12 输出")
    }

    unsafe fn decode(&mut self, au: &[u8]) -> Result<bool> {
        let buffer = MFCreateMemoryBuffer(au.len() as u32)?;
        let mut ptr = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(au.as_ptr(), ptr, au.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(au.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(self.frame * 10_000)?;
        self.frame += 1;

        let mut rendered = false;
        for _ in 0..2 {
            match self.decoder.ProcessInput(0, &sample, 0) {
                Ok(()) => break,
                // 还有输出没取走：先取再重试
                Err(e) if e.code() == MF_E_NOTACCEPTING => rendered |= self.drain()?,
                Err(e) => return Err(e).context("ProcessInput"),
            }
        }
        rendered |= self.drain()?;
        Ok(rendered || self.frame > 0)
    }

    /// 取出所有已解码的帧并显示。
    unsafe fn drain(&mut self) -> Result<bool> {
        let mut shown = false;
        loop {
            let info = self.decoder.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(if provides { None } else { Some(MFCreateSample()?) }),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let r = self.decoder.ProcessOutput(0, &mut out, &mut status);
            let sample = ManuallyDrop::take(&mut out[0].pSample);
            drop(ManuallyDrop::take(&mut out[0].pEvents));
            match r {
                Ok(()) => {
                    if let Some(s) = sample {
                        self.render(&s)?;
                        shown = true;
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(shown),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 分辨率变化（例如被控端按窗口尺寸调整了编码分辨率）
                    self.set_output_type()?;
                    self.vp = None;
                }
                Err(e) => return Err(e).context("ProcessOutput"),
            }
        }
    }

    unsafe fn render(&mut self, sample: &IMFSample) -> Result<()> {
        let buffer = sample.GetBufferByIndex(0)?;
        let dxgi: IMFDXGIBuffer = buffer.cast().context("解码输出不在显存里")?;
        let mut tex_ptr = std::ptr::null_mut();
        dxgi.GetResource(&ID3D11Texture2D::IID, &mut tex_ptr)?;
        let texture = ID3D11Texture2D::from_raw(tex_ptr);
        let slice = dxgi.GetSubresourceIndex()?;
        let mut tdesc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut tdesc);

        // 跟随窗口大小调整交换链
        let mut rc = RECT::default();
        let _ = GetClientRect(self.swap_hwnd()?, &mut rc);
        let (cw, ch) = ((rc.right - rc.left).max(1) as u32, (rc.bottom - rc.top).max(1) as u32);
        if (cw, ch) != self.swap_size {
            self.context.ClearState();
            self.swap.ResizeBuffers(0, cw, ch, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))?;
            self.swap_size = (cw, ch);
            self.vp = None;
        }

        let input_size = (tdesc.Width, tdesc.Height);
        if !matches!(&self.vp, Some((_, _, i, o)) if *i == input_size && *o == (cw, ch)) {
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input_size.0,
                InputHeight: input_size.1,
                OutputFrameRate: rate,
                OutputWidth: cw,
                OutputHeight: ch,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let en = self.vdevice.CreateVideoProcessorEnumerator(&desc)?;
            let vp = self.vdevice.CreateVideoProcessor(&en, 0)?;
            // 输入：BT.709 有限范围 YCbCr；输出：全范围 RGB
            let yuv709 = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: (1 << 2) | (1 << 4) };
            let rgb = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            self.vcontext.VideoProcessorSetStreamColorSpace(&vp, 0, &yuv709);
            self.vcontext.VideoProcessorSetOutputColorSpace(&vp, &rgb);
            self.vcontext.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            self.vp = Some((en, vp, input_size, (cw, ch)));
        }
        let (en, vp, _, _) = self.vp.as_ref().unwrap();

        // 等比缩放、居中，其余区域填黑
        let (vw, vh) = (self.visible.0.min(input_size.0).max(1), self.visible.1.min(input_size.1).max(1));
        let scale = (cw as f64 / vw as f64).min(ch as f64 / vh as f64);
        let (dw, dh) = ((vw as f64 * scale) as i32, (vh as f64 * scale) as i32);
        let (dx, dy) = ((cw as i32 - dw) / 2, (ch as i32 - dh) / 2);
        let src = RECT { left: 0, top: 0, right: vw as i32, bottom: vh as i32 };
        let dst = RECT { left: dx, top: dy, right: dx + dw, bottom: dy + dh };
        self.vcontext.VideoProcessorSetStreamSourceRect(vp, 0, true, Some(&src));
        self.vcontext.VideoProcessorSetStreamDestRect(vp, 0, true, Some(&dst));
        self.vcontext.VideoProcessorSetOutputTargetRect(vp, true, Some(&RECT { left: 0, top: 0, right: cw as i32, bottom: ch as i32 }));
        let black = D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { RGBA: D3D11_VIDEO_COLOR_RGBA { R: 0.0, G: 0.0, B: 0.0, A: 1.0 } } };
        self.vcontext.VideoProcessorSetOutputBackgroundColor(vp, false, &black);

        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice } },
        };
        let mut input_view = None;
        self.vdevice.CreateVideoProcessorInputView(&texture, en, &in_desc, Some(&mut input_view))?;
        let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut output_view = None;
        self.vdevice.CreateVideoProcessorOutputView(&back, en, &out_desc, Some(&mut output_view))?;

        let streams = [D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: ManuallyDrop::new(input_view), ..Default::default() }];
        let r = self.vcontext.VideoProcessorBlt(vp, output_view.as_ref().unwrap(), 0, &streams);
        let [s] = streams;
        drop(ManuallyDrop::into_inner(s.pInputSurface));
        r.context("VideoProcessorBlt")?;
        // 同步间隔 0：不等垂直同步，新帧立即交给桌面合成；显示队列满时（例如锁屏、窗口被遮挡）
        // 直接丢掉这一帧，绝不阻塞接收线程
        let _ = self.swap.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
        if let Some((t, n)) = self.debug.as_mut() {
            *n += 1;
            if t.elapsed().as_secs() >= 1 {
                eprintln!("显示：{n} 帧/秒，画面 {}x{}，解码纹理 {}x{}，窗口 {}x{}", vw, vh, input_size.0, input_size.1, cw, ch);
                *t = std::time::Instant::now();
                *n = 0;
            }
        }
        Ok(())
    }

    unsafe fn swap_hwnd(&self) -> Result<HWND> {
        Ok(self.swap.GetHwnd()?)
    }
}

fn context_cast(device: &ID3D11Device) -> Result<ID3D11VideoContext> {
    Ok(unsafe { device.GetImmediateContext() }?.cast()?)
}
