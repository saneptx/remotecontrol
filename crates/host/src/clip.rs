//! 数据通道（被控端）：剪贴板同步与文件收发。

use rc_protocol::data::{self, ClipData, DataMsg};
use rc_protocol::transfer::{self, Event};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// 本机剪贴板的变化。
#[cfg_attr(not(windows), allow(dead_code))]
pub enum ClipEvent {
    Data(ClipData),
    /// 用户复制了文件（资源管理器里 Ctrl+C）
    Files(Vec<PathBuf>),
}

pub trait ClipboardBackend: Send + Sync {
    fn set(&self, data: ClipData);
    /// 把文件列表放进剪贴板，用户可以直接粘贴到任意文件夹。
    fn set_files(&self, paths: Vec<PathBuf>);
    /// 当前剪贴板里的文件。
    fn files(&self) -> Vec<PathBuf>;
}

/// 收到的文件存放位置：当前登录用户的“下载”文件夹。
pub fn downloads_dir() -> PathBuf {
    #[cfg(windows)]
    {
        crate::win::clipboard::user_downloads_dir()
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("RC_DOWNLOADS")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads"))
    }
}

fn backend(on_change: Box<dyn Fn(ClipEvent) + Send>) -> Box<dyn ClipboardBackend> {
    #[cfg(windows)]
    {
        // 服务模式（SYSTEM）下交给以用户身份运行的助手进程，否则读不到资源管理器 / Office 复制的内容
        let on_change: std::sync::Arc<dyn Fn(ClipEvent) + Send + Sync> = std::sync::Arc::new({
            let f = Mutex::new(on_change);
            move |ev| (f.lock().unwrap())(ev)
        });
        if let Some(token) = crate::win::cliphelper::user_token() {
            match crate::win::cliphelper::HelperClipboard::spawn(token, on_change.clone()) {
                Ok(h) => return Box::new(h),
                Err(e) => eprintln!("启动剪贴板助手失败，改用本进程（部分复制内容可能读不到）: {e:#}"),
            }
        }
        Box::new(crate::win::clipboard::Clipboard::spawn(Box::new(move |ev| on_change(ev))))
    }
    #[cfg(not(windows))]
    {
        // 测试图像源没有剪贴板
        let _ = on_change;
        struct Null;
        impl ClipboardBackend for Null {
            fn set(&self, d: ClipData) {
                println!("剪贴板（测试）: {}", describe(&d));
            }
            fn set_files(&self, paths: Vec<PathBuf>) {
                println!("剪贴板（测试）: {} 个文件", paths.len());
            }
            fn files(&self) -> Vec<PathBuf> {
                std::env::var_os("RC_TEST_CLIP_FILES").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default()
            }
        }
        Box::new(Null)
    }
}

pub fn describe(d: &ClipData) -> String {
    match d {
        ClipData::Text(t) => format!("文字 {} 字", t.chars().count()),
        ClipData::Image(p) => format!("图片 {} KB", p.len() / 1024),
        ClipData::Files { names, .. } => format!("{} 个文件", names.len()),
    }
}

/// 运行数据通道，直到连接关闭。
pub fn run(stream: TcpStream) {
    let Ok(write_half) = stream.try_clone() else { return };
    let writer = Arc::new(Mutex::new(write_half));
    let send = {
        let w = writer.clone();
        move |m: &DataMsg| data::write(&mut *w.lock().unwrap(), m)
    };

    let clip: Arc<dyn ClipboardBackend> = Arc::from(backend(Box::new({
        let send = send.clone();
        move |ev| {
            let msg = match ev {
                ClipEvent::Data(d) => {
                    println!("本机剪贴板变化：{}", describe(&d));
                    DataMsg::Clipboard(d)
                }
                ClipEvent::Files(paths) => {
                    println!("本机剪贴板变化：{} 个文件", paths.len());
                    let (_, total_bytes, names) = transfer::summarize(&paths);
                    DataMsg::Clipboard(ClipData::Files { names, total_bytes })
                }
            };
            let _ = send(&msg);
        }
    })));

    // 连接前就已复制好的文件也通知对方（文字和图片只同步之后的变化）
    let existing = clip.files();
    if !existing.is_empty() {
        let (_, total_bytes, names) = transfer::summarize(&existing);
        let _ = send(&DataMsg::Clipboard(ClipData::Files { names, total_bytes }));
    }

    let mut receiver = transfer::Receiver::new(downloads_dir());
    let mut next_transfer = 1u32;
    let mut reader = stream;
    while let Ok(msg) = data::read(&mut reader) {
        match msg {
            DataMsg::Clipboard(ClipData::Files { .. }) => {} // 对方复制文件的通知，本端不需要
            DataMsg::Clipboard(d) => clip.set(d),
            DataMsg::FetchClipboardFiles => {
                let files = clip.files();
                let id = next_transfer;
                next_transfer += 1;
                let send = send.clone();
                std::thread::spawn(move || {
                    if files.is_empty() {
                        let _ = send(&DataMsg::TransferError { transfer: id, reason: "剪贴板里没有文件".into() });
                        return;
                    }
                    match transfer::send_paths(id, &files, &send, |_, _| {}) {
                        Ok(()) => println!("已发送剪贴板中的 {} 项", files.len()),
                        Err(e) => eprintln!("发送文件失败: {e}"),
                    }
                });
            }
            other => {
                if matches!(other, DataMsg::TransferBegin { .. }) {
                    receiver.set_dest(downloads_dir()); // 用户可能已切换
                }
                match receiver.handle(other) {
                    Some(Event::Started { files, total_bytes, .. }) => {
                        println!("开始接收 {files} 个文件（{:.1} MB）", total_bytes as f64 / 1e6)
                    }
                    Some(Event::Done { items, .. }) => {
                        println!("已接收到“下载”：{}", items.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join("、"));
                        clip.set_files(items);
                    }
                    Some(Event::Failed { reason, .. }) => eprintln!("接收文件失败: {reason}"),
                    _ => {}
                }
            }
        }
    }
}
