//! DXGI Desktop Duplication 屏幕采集。
//!
//! 每次取到新画面就 GPU 内拷贝到自有纹理 `target`（已注册给 NVENC），随后立即释放
//! DXGI 帧，这样桌面合成器不会被我们阻塞。

use anyhow::{bail, Context, Result};
use std::time::Duration;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

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
}

impl Capturer {
    pub fn new() -> Result<Self> {
        unsafe {
            // 不声明 DPI 感知的话，坐标和尺寸会被系统缩放。
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

            let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
            // 取第一块接有显示器的显卡的第一个输出（通常就是主显示器）。
            let (adapter, output) = (0..)
                .map_while(|i| factory.EnumAdapters1(i).ok())
                .find_map(|a| a.EnumOutputs(0).ok().map(|o| (a, o)))
                .context("没有找到已连接显示器的显卡（无头机器需要接显示器或 HDMI 诱骗器）")?;
            let output: IDXGIOutput1 = output.cast()?;

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

            let dup = output.DuplicateOutput(&device).context("DuplicateOutput（是否已有其它程序独占，或处于安全桌面？）")?;
            let dup_desc = dup.GetDesc();
            let (width, height) = (dup_desc.ModeDesc.Width, dup_desc.ModeDesc.Height);
            let rect = output.GetDesc()?.DesktopCoordinates;

            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut target = None;
            device.CreateTexture2D(&tex_desc, None, Some(&mut target)).context("CreateTexture2D")?;

            Ok(Self { device, context, output, dup: Some(dup), target: target.unwrap(), width, height, left: rect.left, top: rect.top })
        }
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn target(&self) -> &ID3D11Texture2D {
        &self.target
    }

    /// 等待至多 `timeout_ms`。有新画面时拷贝进 `target` 并返回 true。
    pub fn acquire(&mut self, timeout_ms: u32) -> Result<bool> {
        unsafe {
            let Some(dup) = self.dup.clone() else {
                // 上次丢失了访问权（UAC/锁屏/切换分辨率），尝试重建。
                std::thread::sleep(Duration::from_millis(timeout_ms as u64));
                if let Ok(dup) = self.output.DuplicateOutput(&self.device) {
                    let desc = dup.GetDesc();
                    if (desc.ModeDesc.Width, desc.ModeDesc.Height) != (self.width, self.height) {
                        bail!("屏幕分辨率已变化，请重新连接");
                    }
                    self.dup = Some(dup);
                }
                return Ok(false);
            };

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                    self.dup = None;
                    return Ok(false);
                }
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }

            // LastPresentTime 为 0 表示只有鼠标指针变化，桌面图像没有更新。
            let mut updated = false;
            if info.LastPresentTime != 0 {
                if let Some(resource) = resource {
                    let frame: ID3D11Texture2D = resource.cast()?;
                    self.context.CopyResource(&self.target, &frame);
                    updated = true;
                }
            }
            let _ = dup.ReleaseFrame();
            Ok(updated)
        }
    }
}
