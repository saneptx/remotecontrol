//! NVIDIA Video Codec SDK 12.1 的 bindgen 绑定。
//!
//! 取自 `nvidia-video-codec-sdk` / `moq-nvenc`（MIT，见同目录 LICENSE），
//! 只保留 nvEncodeAPI 部分，运行时从 `nvEncodeAPI64.dll` 动态加载。
#![allow(warnings, clippy::all)]

mod guid;
#[rustfmt::skip]
mod nvEncodeAPI;
mod version;

pub use nvEncodeAPI::*;
