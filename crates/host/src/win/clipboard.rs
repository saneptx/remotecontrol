//! Windows 剪贴板：读写文字、图片、文件列表，并监视变化。
//!
//! 剪贴板属于窗口站（winsta0），与当前在哪个桌面无关；写入时需要一个窗口作为“所有者”，
//! 所以在专用线程里创建一个不可见的消息窗口，所有剪贴板操作都在这个线程里做。
//! 变化检测用剪贴板序列号轮询（每 250ms），不需要消息循环里的监听器。
//!
//! 资源管理器复制文件、Office 复制内容时，数据放在 OLE 数据对象里，跨用户（本进程是 SYSTEM）
//! 用传统 API 只能看到一个私有格式；这时改用 OleGetClipboard 读取。

use crate::clip::{ClipEvent, ClipboardBackend};
use rc_protocol::data::{ClipData, MAX_CLIPBOARD};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5, CF_HDROP, CF_UNICODETEXT};
use windows::Win32::UI::Shell::{DragQueryFileW, DROPFILES, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

enum Cmd {
    Set(ClipData),
    SetFiles(Vec<PathBuf>),
    GetFiles(mpsc::Sender<Vec<PathBuf>>),
}

pub struct Clipboard {
    tx: mpsc::Sender<Cmd>,
}

impl Clipboard {
    pub fn spawn(on_change: Box<dyn Fn(ClipEvent) + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || unsafe { run(rx, on_change) });
        Self { tx }
    }
}

impl ClipboardBackend for Clipboard {
    fn set(&self, data: ClipData) {
        let _ = self.tx.send(Cmd::Set(data));
    }
    fn set_files(&self, paths: Vec<PathBuf>) {
        let _ = self.tx.send(Cmd::SetFiles(paths));
    }
    fn files(&self) -> Vec<PathBuf> {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Cmd::GetFiles(tx)).is_err() {
            return Vec::new();
        }
        rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
    }
}

/// 剪贴板线程。`Clipboard` 被丢弃后通道断开，线程退出。
unsafe fn run(rx: mpsc::Receiver<Cmd>, on_change: Box<dyn Fn(ClipEvent) + Send>) {
    let _ = windows::Win32::System::Ole::OleInitialize(None);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        w!("RemoteControlClipboard"),
        WINDOW_STYLE(0),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        None,
        None,
    )
    .unwrap_or_default();
    let png_format = RegisterClipboardFormatW(w!("PNG"));
    let mut seq = GetClipboardSequenceNumber();
    let mut busy_retries = 0;
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(Cmd::Set(d)) => {
                write(hwnd, png_format, &d);
                seq = GetClipboardSequenceNumber(); // 自己写入的不回传
            }
            Ok(Cmd::SetFiles(paths)) => {
                write_files(hwnd, &paths);
                seq = GetClipboardSequenceNumber();
            }
            Ok(Cmd::GetFiles(reply)) => {
                let mut files = with_clipboard(hwnd, || read_files()).unwrap_or_default();
                if files.is_empty() {
                    files = ole_bytes(CF_HDROP.0 as u32).map(|b| parse_dropfiles(&b)).unwrap_or_default();
                }
                let _ = reply.send(files);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let now = GetClipboardSequenceNumber();
        if now != seq {
            let ev = match with_clipboard(hwnd, || read(png_format)) {
                Some(None) => Some(ole_read(png_format)), // 传统 API 读不到时走 OLE（不能在打开剪贴板时调用）
                other => other,
            };
            // 剪贴板被别的程序占着（资源管理器刚复制完时常见）：这次变化留到下一轮再读，最多重试约 5 秒
            if ev.is_none() && busy_retries < 20 {
                busy_retries += 1;
                continue;
            }
            busy_retries = 0;
            seq = now;
            match ev {
                Some(Some(ev)) => on_change(ev),
                Some(None) => {
                    let formats = with_clipboard(hwnd, || {
                        let mut v = Vec::new();
                        let mut f = 0;
                        loop {
                            f = EnumClipboardFormats(f);
                            if f == 0 {
                                break;
                            }
                            v.push(f);
                        }
                        v
                    });
                    eprintln!("剪贴板变化但没有可同步的内容，格式：{formats:?}");
                }
                None => eprintln!("剪贴板被其它程序占用，无法读取"),
            }
        }
    }
    if !hwnd.is_invalid() {
        let _ = DestroyWindow(hwnd);
    }
}

/// 打开剪贴板执行 `f`。别的程序正占用时重试几次。
unsafe fn with_clipboard<T>(hwnd: HWND, f: impl FnOnce() -> T) -> Option<T> {
    for _ in 0..10 {
        if OpenClipboard(Some(hwnd)).is_ok() {
            let r = f();
            let _ = CloseClipboard();
            return Some(r);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

unsafe fn global_bytes(h: HANDLE) -> Option<Vec<u8>> {
    let g = HGLOBAL(h.0);
    let p = GlobalLock(g) as *const u8;
    if p.is_null() {
        return None;
    }
    let v = std::slice::from_raw_parts(p, GlobalSize(g)).to_vec();
    let _ = GlobalUnlock(g);
    Some(v)
}

unsafe fn read(png_format: u32) -> Option<ClipEvent> {
    let has = |f: u32| IsClipboardFormatAvailable(f).is_ok();
    if has(CF_HDROP.0 as u32) {
        let files = read_files();
        if !files.is_empty() {
            return Some(ClipEvent::Files(files));
        }
        eprintln!("剪贴板有 CF_HDROP 但读不到文件：{:?}", GetClipboardData(CF_HDROP.0 as u32).map(|h| h.0));
    }
    if png_format != 0 && has(png_format) {
        if let Some(png) = GetClipboardData(png_format).ok().and_then(|h| global_bytes(h)) {
            if png.len() <= MAX_CLIPBOARD && rc_protocol::image::decode_png(&png).is_some() {
                return Some(ClipEvent::Data(ClipData::Image(png)));
            }
        }
    }
    for f in [CF_DIBV5, CF_DIB] {
        if has(f.0 as u32) {
            if let Some(png) = GetClipboardData(f.0 as u32).ok().and_then(|h| global_bytes(h)).and_then(|d| dib_to_png(&d)) {
                if png.len() <= MAX_CLIPBOARD {
                    return Some(ClipEvent::Data(ClipData::Image(png)));
                }
            }
            break;
        }
    }
    if has(CF_UNICODETEXT.0 as u32) {
        let bytes = global_bytes(GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?)?;
        let wide: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
        let text = String::from_utf16_lossy(&wide).replace("\r\n", "\n");
        if !text.is_empty() && text.len() <= MAX_CLIPBOARD {
            return Some(ClipEvent::Data(ClipData::Text(text)));
        }
    }
    None
}

unsafe fn read_files() -> Vec<PathBuf> {
    let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) else { return Vec::new() };
    let hdrop = HDROP(h.0);
    let count = DragQueryFileW(hdrop, u32::MAX, None);
    (0..count)
        .filter_map(|i| {
            let len = DragQueryFileW(hdrop, i, None) as usize;
            let mut buf = vec![0u16; len + 1];
            let n = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            (n > 0).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..n])))
        })
        .collect()
}

/// 通过 OLE 数据对象读取某个格式（HGLOBAL 形式）。
unsafe fn ole_bytes(format: u32) -> Option<Vec<u8>> {
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Ole::{OleGetClipboard, ReleaseStgMedium};
    let obj = match OleGetClipboard() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("OleGetClipboard 失败: {e}");
            return None;
        }
    };
    let fe = FORMATETC {
        cfFormat: format as u16,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = match obj.GetData(&fe) {
        Ok(m) => m,
        Err(e) => {
            if format == CF_HDROP.0 as u32 {
                eprintln!("读取 OLE 文件列表失败: {e}");
            }
            return None;
        }
    };
    let bytes = (medium.tymed == TYMED_HGLOBAL.0 as u32).then(|| global_bytes(HANDLE(medium.u.hGlobal.0))).flatten();
    ReleaseStgMedium(&mut medium);
    bytes
}

/// 解析 DROPFILES 结构（CF_HDROP 的内存格式）。
fn parse_dropfiles(b: &[u8]) -> Vec<PathBuf> {
    if b.len() < 20 {
        return Vec::new();
    }
    let offset = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
    let wide = u32::from_le_bytes(b[16..20].try_into().unwrap()) != 0;
    let Some(rest) = b.get(offset..) else { return Vec::new() };
    let names: Vec<String> = if wide {
        let u: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        u.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    } else {
        rest.split(|&c| c == 0).take_while(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect()
    };
    names.into_iter().map(PathBuf::from).collect()
}

unsafe fn ole_read(png_format: u32) -> Option<ClipEvent> {
    if let Some(files) = ole_bytes(CF_HDROP.0 as u32).map(|b| parse_dropfiles(&b)).filter(|f| !f.is_empty()) {
        return Some(ClipEvent::Files(files));
    }
    if png_format != 0 {
        if let Some(png) = ole_bytes(png_format).filter(|p| p.len() <= MAX_CLIPBOARD && rc_protocol::image::decode_png(p).is_some()) {
            return Some(ClipEvent::Data(ClipData::Image(png)));
        }
    }
    for f in [CF_DIBV5, CF_DIB] {
        if let Some(png) = ole_bytes(f.0 as u32).and_then(|d| dib_to_png(&d)).filter(|p| p.len() <= MAX_CLIPBOARD) {
            return Some(ClipEvent::Data(ClipData::Image(png)));
        }
    }
    let bytes = ole_bytes(CF_UNICODETEXT.0 as u32)?;
    let wide: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0).collect();
    let text = String::from_utf16_lossy(&wide).replace("\r\n", "\n");
    (!text.is_empty() && text.len() <= MAX_CLIPBOARD).then_some(ClipEvent::Data(ClipData::Text(text)))
}

unsafe fn alloc_global(bytes: &[u8]) -> Option<HANDLE> {
    let g = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let p = GlobalLock(g) as *mut u8;
    if p.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
    let _ = GlobalUnlock(g);
    Some(HANDLE(g.0))
}

unsafe fn write(hwnd: HWND, png_format: u32, data: &ClipData) {
    let formats: Vec<(u32, Vec<u8>)> = match data {
        ClipData::Text(t) => {
            let normalized = t.replace("\r\n", "\n").replace('\n', "\r\n");
            let bytes: Vec<u8> = normalized.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
            vec![(CF_UNICODETEXT.0 as u32, bytes)]
        }
        ClipData::Image(png) => {
            let Some(dib) = png_to_dib(png) else { return };
            let mut v = vec![(CF_DIB.0 as u32, dib)];
            if png_format != 0 {
                v.push((png_format, png.clone()));
            }
            v
        }
        ClipData::Files { .. } => return, // 文件需要先传输过来，见 write_files
    };
    with_clipboard(hwnd, || {
        let _ = EmptyClipboard();
        for (fmt, bytes) in &formats {
            if let Some(h) = alloc_global(bytes) {
                let _ = SetClipboardData(*fmt, Some(h));
            }
        }
    });
}

/// 把文件列表放进剪贴板（CF_HDROP），在资源管理器里 Ctrl+V 即可粘贴。
unsafe fn write_files(hwnd: HWND, paths: &[PathBuf]) {
    let header = DROPFILES { pFiles: size_of::<DROPFILES>() as u32, fWide: true.into(), ..Default::default() };
    let mut bytes = std::slice::from_raw_parts((&header as *const DROPFILES).cast::<u8>(), size_of::<DROPFILES>()).to_vec();
    for p in paths {
        let wide: Vec<u16> = p.to_string_lossy().encode_utf16().chain([0]).collect();
        bytes.extend(wide.iter().flat_map(|c| c.to_le_bytes()));
    }
    bytes.extend([0, 0]);
    let effect_format = RegisterClipboardFormatW(w!("Preferred DropEffect"));
    with_clipboard(hwnd, || {
        let _ = EmptyClipboard();
        if let Some(h) = alloc_global(&bytes) {
            let _ = SetClipboardData(CF_HDROP.0 as u32, Some(h));
        }
        if effect_format != 0 {
            if let Some(h) = alloc_global(&1u32.to_le_bytes()) {
                let _ = SetClipboardData(effect_format, Some(h)); // DROPEFFECT_COPY
            }
        }
    });
}

/// CF_DIB / CF_DIBV5（24 或 32 位）→ PNG。
fn dib_to_png(d: &[u8]) -> Option<Vec<u8>> {
    if d.len() < 40 {
        return None;
    }
    let u32_at = |i: usize| u32::from_le_bytes(d[i..i + 4].try_into().unwrap());
    let header_size = u32_at(0) as usize;
    let width = i32::from_le_bytes(d[4..8].try_into().unwrap());
    let height = i32::from_le_bytes(d[8..12].try_into().unwrap());
    let bpp = u16::from_le_bytes([d[14], d[15]]);
    let compression = u32_at(16);
    if width <= 0 || height == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) {
        return None;
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let masks = if compression == 3 && header_size == 40 { 12 } else { 0 };
    let clr_used = u32_at(32) as usize;
    let offset = header_size + masks + clr_used * 4;
    let stride = (w * bpp as usize / 8 + 3) & !3;
    if d.len() < offset + stride * h {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        let src_row = if height > 0 { h - 1 - y } else { y }; // 正高度为自下而上
        let row = &d[offset + src_row * stride..];
        for x in 0..w {
            let px = &row[x * bpp as usize / 8..];
            let a = if bpp == 32 { px[3] } else { 255 };
            any_alpha |= bpp == 32 && a != 0;
            rgba[(y * w + x) * 4..][..4].copy_from_slice(&[px[2], px[1], px[0], a]);
        }
    }
    if bpp == 32 && !any_alpha {
        // 多数程序写 32 位 BI_RGB 时 alpha 全为 0，表示不透明
        rgba.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    rc_protocol::image::encode_png(w as u32, h as u32, &rgba)
}

/// PNG → 32 位自下而上的 CF_DIB。
fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
    let (w, h, rgba) = rc_protocol::image::decode_png(png)?;
    let header = BITMAPINFOHEADER {
        biSize: 40,
        biWidth: w as i32,
        biHeight: h as i32,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: 0,
        biSizeImage: w * h * 4,
        ..Default::default()
    };
    let mut out = unsafe { std::slice::from_raw_parts((&header as *const BITMAPINFOHEADER).cast::<u8>(), 40) }.to_vec();
    for y in (0..h as usize).rev() {
        for px in rgba[y * w as usize * 4..(y + 1) * w as usize * 4].chunks_exact(4) {
            out.extend([px[2], px[1], px[0], px[3]]);
        }
    }
    Some(out)
}

/// 当前登录用户的“下载”文件夹（工作进程是 SYSTEM，需要借用户的令牌查询）。
pub fn user_downloads_dir() -> PathBuf {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
    use windows::Win32::UI::Shell::{SHGetKnownFolderPath, FOLDERID_Downloads, KNOWN_FOLDER_FLAG};
    unsafe {
        let mut token = HANDLE::default();
        let token = WTSQueryUserToken(WTSGetActiveConsoleSessionId(), &mut token).is_ok().then_some(token);
        let path = SHGetKnownFolderPath(&FOLDERID_Downloads, KNOWN_FOLDER_FLAG(0), token);
        if let Some(t) = token {
            let _ = CloseHandle(t);
        }
        match path {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                PathBuf::from(s)
            }
            Err(_) => PathBuf::from(r"C:\Users\Public\Downloads"),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dropfiles_parse() {
        let mut b = vec![20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0];
        for s in ["C:\\a.txt", "D:\\文件夹"] {
            b.extend(s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
        }
        b.extend([0, 0]);
        let v = super::parse_dropfiles(&b);
        assert_eq!(v, vec![std::path::PathBuf::from("C:\\a.txt"), std::path::PathBuf::from("D:\\文件夹")]);
    }

    #[test]
    fn dib_png_roundtrip() {
        let rgba: Vec<u8> = (0..5 * 3).flat_map(|i| [i as u8, 2 * i as u8, 3 * i as u8, 255]).collect();
        let png = rc_protocol::image::encode_png(5, 3, &rgba).unwrap();
        let dib = super::png_to_dib(&png).unwrap();
        let back = super::dib_to_png(&dib).unwrap();
        assert_eq!(rc_protocol::image::decode_png(&back).unwrap(), (5, 3, rgba));
    }
}
