//! 数据通道（控制端）：剪贴板双向同步、拖放上传、取回远端复制的文件。

use crate::pasteboard;
use rc_protocol::data::{self, ClipData, DataMsg};
use rc_protocol::transfer::{self, Event};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct DataLink {
    writer: Arc<Mutex<TcpStream>>,
    /// 窗口标题里显示的传输 / 剪贴板状态
    pub status: Arc<Mutex<Option<String>>>,
    /// 远端剪贴板里的文件（名称、总大小）
    pub remote_files: Arc<Mutex<Option<(Vec<String>, u64)>>>,
    next_transfer: Arc<AtomicU32>,
    pending_drop: Arc<Mutex<Vec<PathBuf>>>,
    alive: Arc<AtomicBool>,
}

/// 收到的文件放在“下载”文件夹（可用环境变量 RC_DOWNLOADS 改到别处）。
fn downloads() -> PathBuf {
    std::env::var_os("RC_DOWNLOADS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads"))
}

fn size_text(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b => format!("{} KB", b.div_ceil(1024)),
    }
}

impl DataLink {
    /// 打开数据通道并启动收发线程。
    pub fn open(addr: SocketAddr, token: [u8; 16]) -> anyhow::Result<Self> {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
        rc_protocol::write_msg(&mut stream, &rc_protocol::ClientMsg::DataHello { token })?;
        let link = Self {
            writer: Arc::new(Mutex::new(stream.try_clone()?)),
            status: Default::default(),
            remote_files: Default::default(),
            next_transfer: Arc::new(AtomicU32::new(1)),
            pending_drop: Default::default(),
            alive: Arc::new(AtomicBool::new(true)),
        };
        // 最近一次由本程序写入剪贴板后的变化计数：本地变化与它相同就不回传
        let ours = Arc::new(AtomicIsize::new(pasteboard::change_count()));
        link.spawn_watcher(ours.clone());
        link.spawn_reader(stream, ours);
        Ok(link)
    }

    fn send(&self, m: &DataMsg) -> std::io::Result<()> {
        data::write(&mut *self.writer.lock().unwrap(), m)
    }

    fn set_status(&self, s: Option<String>) {
        *self.status.lock().unwrap() = s;
    }

    /// 每 300ms 检查一次本机剪贴板，有变化就发给对方。
    fn spawn_watcher(&self, ours: Arc<AtomicIsize>) {
        let link = self.clone();
        std::thread::spawn(move || {
            let mut last = pasteboard::change_count();
            while link.alive.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(300));
                let now = pasteboard::change_count();
                if now == last {
                    continue;
                }
                last = now;
                if now == ours.load(Ordering::Relaxed) {
                    continue;
                }
                if let Some(d) = pasteboard::read() {
                    if link.send(&DataMsg::Clipboard(d)).is_err() {
                        break;
                    }
                }
            }
        });
    }

    fn spawn_reader(&self, mut stream: TcpStream, ours: Arc<AtomicIsize>) {
        let link = self.clone();
        std::thread::spawn(move || {
            let mut receiver = transfer::Receiver::new(downloads());
            while let Ok(msg) = data::read(&mut stream) {
                match msg {
                    DataMsg::Clipboard(ClipData::Files { names, total_bytes }) => {
                        *link.remote_files.lock().unwrap() = Some((names, total_bytes));
                    }
                    DataMsg::Clipboard(d) => {
                        // 远端复制了别的内容，之前提示的文件作废
                        *link.remote_files.lock().unwrap() = None;
                        pasteboard::write(&d);
                        ours.store(pasteboard::change_count(), Ordering::Relaxed);
                    }
                    DataMsg::FetchClipboardFiles => {} // 被控端不会向控制端要文件
                    other => match receiver.handle(other) {
                        Some(Event::Started { files, total_bytes, .. }) => {
                            link.set_status(Some(format!("正在接收 {files} 个文件（{}）", size_text(total_bytes))))
                        }
                        Some(Event::Progress { done, total, .. }) => {
                            link.set_status(Some(format!("正在接收 {}%", done * 100 / total.max(1))))
                        }
                        Some(Event::Done { items, .. }) => {
                            let names: Vec<String> =
                                items.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
                            pasteboard::write_files(&items);
                            ours.store(pasteboard::change_count(), Ordering::Relaxed);
                            link.set_status(Some(format!("已保存到“下载”：{}（可在访达里 ⌘V 粘贴）", names.join("、"))));
                        }
                        Some(Event::Failed { reason, .. }) => link.set_status(Some(format!("接收失败：{reason}"))),
                        None => {}
                    },
                }
            }
            link.alive.store(false, Ordering::Relaxed);
        });
    }

    /// 请求对方把它剪贴板里的文件发过来。
    pub fn fetch_remote_files(&self) {
        if self.remote_files.lock().unwrap().take().is_some() {
            self.set_status(Some("正在请求远端复制的文件…".into()));
            let _ = self.send(&DataMsg::FetchClipboardFiles);
        } else {
            self.set_status(Some("远端剪贴板里没有文件（先在远端资源管理器里复制文件）".into()));
        }
    }

    /// 拖放进窗口的文件：攒一小会儿（一次拖多个文件会逐个到达）再作为一批发送。
    pub fn drop_file(&self, path: PathBuf) {
        let first = {
            let mut p = self.pending_drop.lock().unwrap();
            p.push(path);
            p.len() == 1
        };
        if !first {
            return;
        }
        let link = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let paths = std::mem::take(&mut *link.pending_drop.lock().unwrap());
            link.send_paths(paths);
        });
    }

    pub fn send_paths(&self, paths: Vec<PathBuf>) {
        let id = self.next_transfer.fetch_add(1, Ordering::Relaxed);
        let (files, total, _) = transfer::summarize(&paths);
        self.set_status(Some(format!("正在发送 {files} 个文件（{}）", size_text(total))));
        let mut last_pct = u64::MAX;
        let result = transfer::send_paths(id, &paths, |m| self.send(m), |done, total| {
            let pct = done * 100 / total.max(1);
            if pct != last_pct {
                last_pct = pct;
                self.set_status(Some(format!("正在发送 {pct}%")));
            }
        });
        self.set_status(Some(match result {
            Ok(()) => format!("已发送 {files} 个文件到远端“下载”（在远端 Ctrl+V 可粘贴到任意文件夹）"),
            Err(e) => format!("发送失败：{e}"),
        }));
    }

    pub fn close(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.writer.lock().unwrap().shutdown(std::net::Shutdown::Both);
    }
}
