//! 数据通道：剪贴板与文件传输。
//!
//! 与控制通道分开的第二条 TCP 连接（连接后先发 [`crate::ClientMsg::DataHello`]），
//! 大块数据在这里传，不会拖慢键鼠输入。两个方向使用同一种消息。

use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};

/// 文件分块大小。
pub const CHUNK: usize = 256 * 1024;
/// 单条消息上限（剪贴板图片可能较大）。
pub const MAX_DATA_MSG: u32 = 64 << 20;
/// 剪贴板同步的大小上限（超过的不同步，避免复制一张超大图片拖慢网络）。
pub const MAX_CLIPBOARD: usize = 32 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ClipData {
    Text(String),
    /// PNG 编码的图片
    Image(Vec<u8>),
    /// 对方剪贴板里有文件（只是通知，需要时用 [`DataMsg::FetchClipboardFiles`] 取回）。
    Files { names: Vec<String>, total_bytes: u64 },
}

/// 本机剪贴板的变化（平台剪贴板模块产生）。
#[derive(Debug, Clone, PartialEq)]
pub enum ClipEvent {
    Data(ClipData),
    /// 用户复制了文件（资源管理器 Ctrl+C / 访达 ⌘C）
    Files(Vec<std::path::PathBuf>),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DataMsg {
    Clipboard(ClipData),
    /// 请求对方把它剪贴板里的文件发过来。
    FetchClipboardFiles,
    /// 开始一批传输。`top_level` 是顶层的文件/文件夹名（接收方据此放进剪贴板）。
    TransferBegin { transfer: u32, files: u32, total_bytes: u64, top_level: Vec<String> },
    /// 一个文件开始。`path` 为以 `/` 分隔的相对路径；`size` 为字节数。
    FileBegin { transfer: u32, path: String, size: u64 },
    /// 空文件夹（含有文件的文件夹由文件路径隐式创建）。
    Dir { transfer: u32, path: String },
    FileChunk { transfer: u32, data: Vec<u8> },
    TransferEnd { transfer: u32 },
    /// 发送或接收出错，整批作废。
    TransferError { transfer: u32, reason: String },
}

pub fn write(w: &mut impl Write, msg: &DataMsg) -> io::Result<()> {
    let body = bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut buf = Vec::with_capacity(4 + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&body);
    w.write_all(&buf)
}

pub fn read(r: &mut impl Read) -> io::Result<DataMsg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len);
    if len > MAX_DATA_MSG {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "data message too large"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    bincode::deserialize(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// 校验对方给的相对路径：只允许普通的路径分量，拒绝绝对路径、`..`、盘符等，防止写到目标目录以外。
pub fn safe_relative_path(path: &str) -> Option<Vec<String>> {
    let parts: Vec<String> = path.split(['/', '\\']).filter(|p| !p.is_empty() && *p != ".").map(str::to_string).collect();
    if parts.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        return None;
    }
    let bad = |p: &String| p == ".." || p.contains(':') || p.chars().any(|c| c.is_control() || "<>\"|?*".contains(c)) || p.len() > 255;
    if parts.iter().any(bad) {
        return None;
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        let m = DataMsg::FileChunk { transfer: 3, data: vec![7; 1000] };
        write(&mut buf, &m).unwrap();
        write(&mut buf, &DataMsg::Clipboard(ClipData::Text("你好".into()))).unwrap();
        let mut r = &buf[..];
        assert_eq!(read(&mut r).unwrap(), m);
        assert_eq!(read(&mut r).unwrap(), DataMsg::Clipboard(ClipData::Text("你好".into())));
    }

    #[test]
    fn path_safety() {
        assert_eq!(safe_relative_path("a/b.txt"), Some(vec!["a".into(), "b.txt".into()]));
        assert_eq!(safe_relative_path("照片/2026/x.jpg").unwrap().len(), 3);
        assert_eq!(safe_relative_path("./a"), Some(vec!["a".into()]));
        for bad in ["../x", "a/../../x", "/etc/passwd", "\\\\server\\x", "C:\\x", "a/b:c", "", "a/\u{1}"] {
            assert_eq!(safe_relative_path(bad), None, "{bad}");
        }
    }
}
