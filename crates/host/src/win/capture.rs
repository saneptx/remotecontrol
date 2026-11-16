//! DXGI Desktop Duplication 屏幕采集。
//!
//! 每次取到新画面就 GPU 内拷贝到自有纹理 `target`（已注册给 NVENC），随后立即释放
//! DXGI 帧，这样桌面合成器不会被我们阻塞。

use anyhow::{bail, Context, Result};
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Ok,
    /// 锁屏 / UAC 等安全桌面：普通权限的进程无权采集。
    SecureDesktop,
    /// 采集中断且无法恢复（显示器关闭后断开等）。
    Unavailable,
}

/// 一块接在桌面上的显示器（某块显卡的某个输出）。
pub struct OutputInfo {
    adapter: u32,
    output: u32,
    /// 例如 `\\.\DISPLAY1`
    pub device: String,
    /// 在虚拟桌面中的 (left, top, width, height)
    pub rect: (i32, i32, u32, u32),
}

impl OutputInfo {
    pub fn primary(&self) -> bool {
        self.rect.0 == 0 && self.rect.1 == 0
    }
}

/// 所有显示器，顺序为“显卡 → 输出”，与 [`Capturer::new`] 的下标一致。
pub fn list_outputs() -> Vec<OutputInfo> {
    let mut list = Vec::new();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return list };
        for a in (0..).map_while(|i| factory.EnumAdapters1(i).ok().map(|ad| (i, ad))) {
            for (o, out) in (0..).map_while(|j| a.1.EnumOutputs(j).ok().map(|out| (j, out))) {
                let Ok(desc) = out.GetDesc() else { continue };
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                let device = String::from_utf16_lossy(&desc.DeviceName).trim_end_matches('\0').to_string();
                list.push(OutputInfo {
                    adapter: a.0,
                    output: o,
                    device,
                    rect: (r.left, r.top, (r.right - r.left) as u32, (r.bottom - r.top) as u32),
                });
            }
        }
    }
    list
}

pub struct Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    target: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub left: i32,
    pub top: i32,
    /// 显示器刷新率（Hz）。
    pub refresh_hz: u32,
    state: State,
    /// 进入 Unavailable 的时刻。
    lost_since: Option<Instant>,
}

impl Capturer {
    /// 采集 [`list_outputs`] 中的第 `index` 块显示器。
    pub fn new(index: usize) -> Result<Self> {
        unsafe {
            // 不声明 DPI 感知的话，坐标和尺寸会被系统缩放。
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            super::desktop::sync_thread();

            let outputs = list_outputs();
            let info = outputs
                .get(index)
                .or(outputs.first())
                .context("没有找到已连接的显示器（显示器关闭后可能会断开；无头机器需要接 HDMI 诱骗器）")?;
            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            let adapter = factory.EnumAdapters1(info.adapter).context("EnumAdapters1")?;
            let output: IDXGIOutput1 = adapter.EnumOutputs(info.output).context("EnumOutputs")?.cast()?;

            let mut device = None;
            let mut context = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11CreateDevice")?;
            let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
            // 提高 GPU 调度优先级：前台有游戏等重负载时，采集与编码不至于排队。
            if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
                let _ = dxgi.SetGPUThreadPriority(7);
            }

            let dup = match output.DuplicateOutput(&device) {
                Ok(d) => d,
                Err(e) if e.code() == E_ACCESSDENIED => {
                    bail!("远端处于锁屏或 UAC 界面，当前权限无法采集（用 rc-host install 安装为系统服务即可解决）")
                }
                Err(e) => return Err(e).context("DuplicateOutput（是否有其它程序独占了屏幕采集？）"),
            };
            let dup_desc = dup.GetDesc();
            let (width, height) = (dup_desc.ModeDesc.Width, dup_desc.ModeDesc.Height);
            let rate = dup_desc.ModeDesc.RefreshRate;
            let refresh_hz = if rate.Denominator > 0 && rate.Numerator > 0 {
                ((rate.Numerator as f64 / rate.Denominator as f64).round() as u32).clamp(24, 240)
            } else {
                60
            };
            let rect = output.GetDesc()?.DesktopCoordinates;

            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                // SHADER_RESOURCE：作为视频处理器（缩放）的输入
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut target = None;
            device.CreateTexture2D(&tex_desc, None, Some(&mut target)).context("CreateTexture2D")?;

            Ok(Self {
                device,
                context,
                output,
                dup: Some(dup),
                target: target.unwrap(),
                width,
                height,
                left: rect.left,
                top: rect.top,
                refresh_hz,
                state: State::Ok,
                lost_since: None,
            })
        }
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    pub fn target(&self) -> &ID3D11Texture2D {
        &self.target
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// 采集已中断超过 2 秒且无法就地恢复：输出对象可能已失效（例如显示器断开后重连），
    /// 需要重新枚举显卡与显示器、重建整条管线。
    pub fn needs_rebuild(&self) -> bool {
        self.state == State::Unavailable && self.lost_since.is_some_and(|t| t.elapsed() > Duration::from_secs(2))
    }

    fn set_state(&mut self, s: State) {
        if s == State::Unavailable && self.state != State::Unavailable {
            self.lost_since = Some(Instant::now());
        } else if s != State::Unavailable {
            self.lost_since = None;
        }
        self.state = s;
    }

    /// 等待至多 `timeout_ms`。有新画面时拷贝进 `target`，并返回该画面的呈现时刻（QPC 计数）。
    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Option<i64>> {
        unsafe {
            let Some(dup) = self.dup.clone() else {
                // 上次丢失了访问权（锁屏/UAC/显示器关闭/切换分辨率），尝试重建。
                // 桌面切换时先跟过去（服务模式下才有权限进入锁屏所在的安全桌面）。
                let switched = super::desktop::sync_thread();
                if !switched {
                    std::thread::sleep(Duration::from_millis(timeout_ms.clamp(10, 50) as u64));
                }
                match self.output.DuplicateOutput(&self.device) {
                    Ok(dup) => {
                        let desc = dup.GetDesc();
                        if (desc.ModeDesc.Width, desc.ModeDesc.Height) != (self.width, self.height) {
                            bail!("屏幕分辨率已变化，请重新连接");
                        }
                        self.dup = Some(dup);
                        self.set_state(State::Ok);
                    }
                    // 刚切换了桌面却仍然失败：旧设备可能不能跨桌面使用，立即整体重建，别再等。
                    Err(_) if switched => {
                        self.set_state(State::Unavailable);
                        self.lost_since = Some(Instant::now() - Duration::from_secs(10));
                    }
                    Err(e) if e.code() == E_ACCESSDENIED => self.set_state(State::SecureDesktop),
                    Err(_) => self.set_state(State::Unavailable),
                }
                return Ok(None);
            };

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                    self.dup = None;
                    return Ok(None);
                }
                Err(e) => {
                    // 例如 DXGI_ERROR_DEVICE_REMOVED：交给上层重建。
                    eprintln!("AcquireNextFrame 失败: {e}");
                    self.dup = None;
                    self.set_state(State::Unavailable);
                    self.lost_since = Some(Instant::now() - Duration::from_secs(10));
                    return Ok(None);
                }
            }

            // LastPresentTime 为 0 表示只有鼠标指针变化，桌面图像没有更新。
            let mut present = None;
            if info.LastPresentTime != 0 {
                if let Some(resource) = resource {
                    let frame: ID3D11Texture2D = resource.cast()?;
                    self.context.CopyResource(&self.target, &frame);
                    present = Some(info.LastPresentTime);
                }
            }
            let _ = dup.ReleaseFrame();
            Ok(present)
        }
    }
}
