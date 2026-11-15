//! 用 D3D11 视频处理器在显卡上缩放 BGRA 画面（RGB → RGB，全范围，不做任何画面“增强”）。

use anyhow::{Context, Result};
use std::mem::ManuallyDrop;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC};

pub struct Scaler {
    context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
    output: ID3D11Texture2D,
}

impl Scaler {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        src_size: (u32, u32),
        dst_size: (u32, u32),
        fps: u32,
    ) -> Result<Self> {
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast().context("ID3D11VideoDevice")?;
            let vctx: ID3D11VideoContext = context.cast().context("ID3D11VideoContext")?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: src_size.0,
                InputHeight: src_size.1,
                OutputFrameRate: rate,
                OutputWidth: dst_size.0,
                OutputHeight: dst_size.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
            };
            let enumerator = vdev.CreateVideoProcessorEnumerator(&desc).context("CreateVideoProcessorEnumerator")?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0).context("CreateVideoProcessor")?;

            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: dst_size.0,
                Height: dst_size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut output = None;
            device.CreateTexture2D(&tex_desc, None, Some(&mut output)).context("CreateTexture2D(scaled)")?;
            let output = output.unwrap();

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
            let mut output_view = None;
            vdev.CreateVideoProcessorOutputView(&output, &enumerator, &out_desc, Some(&mut output_view))
                .context("CreateVideoProcessorOutputView")?;

            // 输入输出都是 RGB 全范围（位域全 0）；关闭驱动的自动画质处理（锐化、降噪等），保证颜色原样。
            let rgb_full = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
            vctx.VideoProcessorSetStreamColorSpace(&processor, 0, &rgb_full);
            vctx.VideoProcessorSetOutputColorSpace(&processor, &rgb_full);
            vctx.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            Ok(Self { context: vctx, processor, input: input.unwrap(), output_view: output_view.unwrap(), output })
        }
    }

    /// 缩放后的纹理（注册给编码器）。
    pub fn output(&self) -> &ID3D11Texture2D {
        &self.output
    }

    /// 把源纹理的当前内容缩放到输出纹理。
    pub fn process(&self) -> Result<()> {
        unsafe {
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input.clone())),
                ..Default::default()
            };
            let streams = [stream];
            let result = self.context.VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams);
            let [stream] = streams;
            drop(ManuallyDrop::into_inner(stream.pInputSurface));
            result.context("VideoProcessorBlt")
        }
    }
}
