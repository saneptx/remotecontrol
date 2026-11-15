//! 文件传输的发送与接收（两端共用，只依赖标准库）。

use crate::data::{safe_relative_path, DataMsg, CHUNK};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// 待发送的一项：文件（含大小）或空文件夹。路径为以 `/` 分隔的相对路径。
enum Entry {
    File { src: PathBuf, rel: String, size: u64 },
    Dir { rel: String },
}

fn collect(src: &Path, rel: String, out: &mut Vec<Entry>) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        let mut children: Vec<_> = std::fs::read_dir(src)?.filter_map(|e| e.ok()).collect();
        children.sort_by_key(|e| e.file_name());
        if children.is_empty() {
            out.push(Entry::Dir { rel });
            return Ok(());
        }
        for c in children {
            let name = c.file_name().to_string_lossy().to_string();
            collect(&c.path(), format!("{rel}/{name}"), out)?;
        }
    } else if meta.is_file() {
        out.push(Entry::File { src: src.to_path_buf(), rel, size: meta.len() });
    }
    // 符号链接等其它类型忽略
    Ok(())
}

/// 统计要发送的文件。返回 (文件数, 总字节数, 顶层名称)。
pub fn summarize(paths: &[PathBuf]) -> (u32, u64, Vec<String>) {
    let mut entries = Vec::new();
    let mut top = Vec::new();
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if collect(p, name.clone(), &mut entries).is_ok() {
            top.push(name);
        }
    }
    let files = entries.iter().filter(|e| matches!(e, Entry::File { .. })).count() as u32;
    let bytes = entries.iter().map(|e| if let Entry::File { size, .. } = e { *size } else { 0 }).sum();
    (files, bytes, top)
}

/// 发送一批文件 / 文件夹。`send` 写出一条消息；`progress(已发送, 总数)`。
pub fn send_paths(
    transfer: u32,
    paths: &[PathBuf],
    mut send: impl FnMut(&DataMsg) -> io::Result<()>,
    mut progress: impl FnMut(u64, u64),
) -> io::Result<()> {
    let mut entries = Vec::new();
    let mut top_level = Vec::new();
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).ok_or_else(|| io::Error::other("无效路径"))?;
        collect(p, name.clone(), &mut entries)?;
        top_level.push(name);
    }
    let files = entries.iter().filter(|e| matches!(e, Entry::File { .. })).count() as u32;
    let total: u64 = entries.iter().map(|e| if let Entry::File { size, .. } = e { *size } else { 0 }).sum();
    send(&DataMsg::TransferBegin { transfer, files, total_bytes: total, top_level })?;

    let result = (|| -> io::Result<()> {
        let mut sent = 0u64;
        let mut buf = vec![0u8; CHUNK];
        for e in &entries {
            match e {
                Entry::Dir { rel } => send(&DataMsg::Dir { transfer, path: rel.clone() })?,
                Entry::File { src, rel, size } => {
                    let mut f = File::open(src)?;
                    send(&DataMsg::FileBegin { transfer, path: rel.clone(), size: *size })?;
                    let mut left = *size;
                    while left > 0 {
                        let n = f.read(&mut buf[..CHUNK.min(left as usize)])?;
                        if n == 0 {
                            return Err(io::Error::other(format!("{rel} 在发送过程中被修改")));
                        }
                        send(&DataMsg::FileChunk { transfer, data: buf[..n].to_vec() })?;
                        left -= n as u64;
                        sent += n as u64;
                        progress(sent, total);
                    }
                }
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => send(&DataMsg::TransferEnd { transfer }),
        Err(e) => {
            let _ = send(&DataMsg::TransferError { transfer, reason: e.to_string() });
            Err(e)
        }
    }
}

/// 接收进度事件。
#[derive(Debug, PartialEq)]
pub enum Event {
    Started { transfer: u32, files: u32, total_bytes: u64 },
    Progress { transfer: u32, done: u64, total: u64 },
    /// 完成；`items` 为落盘后的顶层文件 / 文件夹路径。
    Done { transfer: u32, items: Vec<PathBuf> },
    Failed { transfer: u32, reason: String },
}

struct Incoming {
    total: u64,
    done: u64,
    /// 顶层名称 → 实际落盘路径（重名时改为 “名称 (1)”）
    tops: HashMap<String, PathBuf>,
    order: Vec<PathBuf>,
    file: Option<(File, u64)>,
}

/// 接收端：把收到的文件写进 `dest` 目录。
pub struct Receiver {
    dest: PathBuf,
    active: HashMap<u32, Incoming>,
}

/// 在 `dir` 下为 `name` 找一个不重名的路径：`a.txt` → `a (1).txt`。
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (1..).map(|i| dir.join(format!("{stem} ({i}){ext}"))).find(|p| !p.exists()).unwrap()
}

impl Receiver {
    pub fn new(dest: PathBuf) -> Self {
        Self { dest, active: HashMap::new() }
    }

    pub fn set_dest(&mut self, dest: PathBuf) {
        self.dest = dest;
    }

    /// 处理一条传输消息；非传输消息返回 `None`。
    pub fn handle(&mut self, msg: DataMsg) -> Option<Event> {
        match msg {
            DataMsg::TransferBegin { transfer, files, total_bytes, .. } => {
                if let Err(e) = std::fs::create_dir_all(&self.dest) {
                    return Some(Event::Failed { transfer, reason: format!("无法创建 {}: {e}", self.dest.display()) });
                }
                self.active.insert(transfer, Incoming { total: total_bytes, done: 0, tops: HashMap::new(), order: Vec::new(), file: None });
                Some(Event::Started { transfer, files, total_bytes })
            }
            DataMsg::Dir { transfer, path } => self.with(transfer, |rx, inc| {
                let p = rx.resolve(inc, &path)?;
                std::fs::create_dir_all(p)?;
                Ok(None)
            }),
            DataMsg::FileBegin { transfer, path, size } => self.with(transfer, |rx, inc| {
                if inc.file.as_ref().is_some_and(|(_, left)| *left > 0) {
                    return Err(io::Error::other("上一个文件未接收完整"));
                }
                let p = rx.resolve(inc, &path)?;
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                inc.file = Some((File::create(&p)?, size));
                Ok(None)
            }),
            DataMsg::FileChunk { transfer, data } => self.with(transfer, |_, inc| {
                let Some((f, left)) = inc.file.as_mut() else { return Err(io::Error::other("收到数据但没有正在接收的文件")) };
                if data.len() as u64 > *left {
                    return Err(io::Error::other("文件数据超出声明的大小"));
                }
                f.write_all(&data)?;
                *left -= data.len() as u64;
                inc.done += data.len() as u64;
                Ok(Some(Event::Progress { transfer, done: inc.done, total: inc.total }))
            }),
            DataMsg::TransferEnd { transfer } => {
                let inc = self.active.remove(&transfer)?;
                if inc.file.as_ref().is_some_and(|(_, left)| *left > 0) {
                    cleanup(&inc);
                    return Some(Event::Failed { transfer, reason: "文件不完整".into() });
                }
                Some(Event::Done { transfer, items: inc.order })
            }
            DataMsg::TransferError { transfer, reason } => {
                let inc = self.active.remove(&transfer)?;
                cleanup(&inc);
                Some(Event::Failed { transfer, reason })
            }
            _ => None,
        }
    }

    fn with(&mut self, transfer: u32, f: impl FnOnce(&Self, &mut Incoming) -> io::Result<Option<Event>>) -> Option<Event> {
        let mut inc = self.active.remove(&transfer)?;
        match f(self, &mut inc) {
            Ok(ev) => {
                self.active.insert(transfer, inc);
                ev
            }
            Err(e) => {
                cleanup(&inc);
                Some(Event::Failed { transfer, reason: e.to_string() })
            }
        }
    }

    /// 相对路径 → 本地路径（校验安全性，处理顶层重名）。
    fn resolve(&self, inc: &mut Incoming, rel: &str) -> io::Result<PathBuf> {
        let parts = safe_relative_path(rel).ok_or_else(|| io::Error::other(format!("不安全的路径：{rel}")))?;
        let top = match inc.tops.get(&parts[0]) {
            Some(p) => p.clone(),
            None => {
                let p = unique_path(&self.dest, &parts[0]);
                inc.tops.insert(parts[0].clone(), p.clone());
                inc.order.push(p.clone());
                p
            }
        };
        Ok(parts[1..].iter().fold(top, |acc, p| acc.join(p)))
    }
}

/// 出错时删除本批次已写入的内容。
fn cleanup(inc: &Incoming) {
    for p in &inc.order {
        if p.is_dir() {
            let _ = std::fs::remove_dir_all(p);
        } else {
            let _ = std::fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rc-transfer-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn send_and_receive_files_and_folders() {
        let src = tmpdir("src");
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        let big: Vec<u8> = (0..(CHUNK * 2 + 123)).map(|i| (i % 251) as u8).collect();
        std::fs::create_dir_all(src.join("照片/子目录")).unwrap();
        std::fs::create_dir_all(src.join("照片/空")).unwrap();
        std::fs::write(src.join("照片/子目录/big.bin"), &big).unwrap();

        let dest = tmpdir("dest");
        std::fs::write(dest.join("a.txt"), b"existing").unwrap(); // 重名

        let mut msgs = Vec::new();
        let mut last = (0, 0);
        send_paths(7, &[src.join("a.txt"), src.join("照片")], |m| Ok(msgs.push(m.clone())), |d, t| last = (d, t)).unwrap();
        assert_eq!(last, (5 + big.len() as u64, 5 + big.len() as u64));

        let mut rx = Receiver::new(dest.clone());
        let mut done = None;
        for m in msgs {
            if let Some(Event::Done { items, .. }) = rx.handle(m) {
                done = Some(items);
            }
        }
        let items = done.expect("transfer done");
        assert_eq!(items, vec![dest.join("a (1).txt"), dest.join("照片")]);
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"existing");
        assert_eq!(std::fs::read(dest.join("a (1).txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dest.join("照片/子目录/big.bin")).unwrap(), big);
        assert!(dest.join("照片/空").is_dir());
    }

    #[test]
    fn rejects_traversal_and_cleans_up() {
        let dest = tmpdir("evil");
        let mut rx = Receiver::new(dest.clone());
        rx.handle(DataMsg::TransferBegin { transfer: 1, files: 2, total_bytes: 3, top_level: vec![] });
        rx.handle(DataMsg::FileBegin { transfer: 1, path: "ok.txt".into(), size: 1 });
        rx.handle(DataMsg::FileChunk { transfer: 1, data: vec![1] });
        let ev = rx.handle(DataMsg::FileBegin { transfer: 1, path: "../escape.txt".into(), size: 1 });
        assert!(matches!(ev, Some(Event::Failed { .. })));
        assert!(!dest.join("ok.txt").exists(), "partial transfer cleaned up");
        assert!(!dest.parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn oversize_chunk_fails() {
        let dest = tmpdir("over");
        let mut rx = Receiver::new(dest);
        rx.handle(DataMsg::TransferBegin { transfer: 2, files: 1, total_bytes: 1, top_level: vec![] });
        rx.handle(DataMsg::FileBegin { transfer: 2, path: "x".into(), size: 1 });
        assert!(matches!(rx.handle(DataMsg::FileChunk { transfer: 2, data: vec![0; 5] }), Some(Event::Failed { .. })));
    }

    #[test]
    fn unique_names() {
        let d = tmpdir("uniq");
        std::fs::write(d.join("r.pdf"), b"").unwrap();
        std::fs::write(d.join("r (1).pdf"), b"").unwrap();
        assert_eq!(unique_path(&d, "r.pdf"), d.join("r (2).pdf"));
        assert_eq!(unique_path(&d, "new"), d.join("new"));
        std::fs::create_dir(d.join(".hidden")).unwrap();
        assert_eq!(unique_path(&d, ".hidden"), d.join(".hidden (1)"));
    }
}
