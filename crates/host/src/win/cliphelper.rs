//! 以当前登录用户身份运行的剪贴板助手。
//!
//! 被控端工作进程是 SYSTEM 身份。资源管理器、Office 等程序复制的内容是“延迟提供”的 OLE 数据，
//! 只有同一用户的进程才能取到，SYSTEM 只能看到一个空壳格式。所以剪贴板读写交给一个
//! 以登录用户身份运行的助手进程（`rc-host clip-helper`），两者通过标准输入输出管道通信。

use super::clipboard::Clipboard;
use crate::clip::{ClipEvent, ClipboardBackend};
use rc_protocol::data::ClipData;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::FromRawHandle;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, SetHandleInformation, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT};
use windows::Win32::Security::{DuplicateTokenEx, SecurityImpersonation, TokenPrimary, SECURITY_ATTRIBUTES, TOKEN_ALL_ACCESS};
use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, TerminateProcess, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW,
};

#[derive(Serialize, Deserialize)]
enum ToHelper {
    Set(ClipData),
    SetFiles(Vec<String>),
    GetFiles,
}

#[derive(Serialize, Deserialize)]
enum FromHelper {
    /// 用户复制了文字 / 图片
    Data(ClipData),
    /// 用户复制了文件
    Copied(Vec<String>),
    /// GetFiles 的回复
    Files(Vec<String>),
}

fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = bincode::serialize(msg).map_err(io::Error::other)?;
    let mut buf = (body.len() as u32).to_le_bytes().to_vec();
    buf.extend(body);
    w.write_all(&buf)?;
    w.flush()
}

fn read_frame<T: DeserializeOwned>(r: &mut impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    r.read_exact(&mut body)?;
    bincode::deserialize(&body).map_err(io::Error::other)
}

fn strings(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.to_string_lossy().to_string()).collect()
}

/// `rc-host clip-helper`：在用户会话里以用户身份运行。
pub fn helper_main() -> anyhow::Result<()> {
    let out = Arc::new(Mutex::new(io::stdout()));
    let clip = Clipboard::spawn(Box::new({
        let out = out.clone();
        move |ev| {
            let msg = match ev {
                ClipEvent::Data(d) => FromHelper::Data(d),
                ClipEvent::Files(p) => FromHelper::Copied(strings(&p)),
            };
            let _ = write_frame(&mut *out.lock().unwrap(), &msg);
        }
    }));
    let mut input = io::stdin().lock();
    // 工作进程关闭管道（会话结束）时退出
    while let Ok(cmd) = read_frame::<ToHelper>(&mut input) {
        match cmd {
            ToHelper::Set(d) => clip.set(d),
            ToHelper::SetFiles(p) => clip.set_files(p.into_iter().map(PathBuf::from).collect()),
            ToHelper::GetFiles => {
                let files = clip.files();
                write_frame(&mut *out.lock().unwrap(), &FromHelper::Files(strings(&files)))?;
            }
        }
    }
    Ok(())
}

/// 工作进程一侧：通过助手进程操作用户的剪贴板。
pub struct HelperClipboard {
    input: Mutex<File>,
    files: Mutex<mpsc::Receiver<Vec<PathBuf>>>,
    process: HANDLE,
}

unsafe impl Send for HelperClipboard {}
unsafe impl Sync for HelperClipboard {}

/// 当前是否以 SYSTEM 身份运行且有用户登录（只有 SYSTEM 能取到用户令牌）。
pub fn user_token() -> Option<HANDLE> {
    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(WTSGetActiveConsoleSessionId(), &mut token).ok()? };
    Some(token)
}

impl HelperClipboard {
    pub fn spawn(user_token: HANDLE, on_change: Arc<dyn Fn(ClipEvent) + Send + Sync>) -> anyhow::Result<Self> {
        unsafe {
            let mut primary = HANDLE::default();
            let dup = DuplicateTokenEx(user_token, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut primary);
            let _ = CloseHandle(user_token);
            dup?;

            let sa = SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, bInheritHandle: true.into(), ..Default::default() };
            let (mut in_r, mut in_w, mut out_r, mut out_w) = (HANDLE::default(), HANDLE::default(), HANDLE::default(), HANDLE::default());
            CreatePipe(&mut in_r, &mut in_w, Some(&sa), 0)?;
            CreatePipe(&mut out_r, &mut out_w, Some(&sa), 0)?;
            // 我们这一端不能被子进程继承
            SetHandleInformation(in_w, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;
            SetHandleInformation(out_r, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))?;

            let mut env = std::ptr::null_mut();
            let _ = CreateEnvironmentBlock(&mut env, Some(primary), false);

            let mut desktop: Vec<u16> = "winsta0\\default\0".encode_utf16().collect();
            let si = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: in_r,
                hStdOutput: out_w,
                hStdError: GetStdHandle(STD_ERROR_HANDLE).unwrap_or_default(), // 输出到工作进程的日志
                ..Default::default()
            };
            let exe = std::env::current_exe()?;
            let mut cmdline: Vec<u16> = format!("\"{}\" clip-helper", exe.display()).encode_utf16().chain([0]).collect();
            let mut pi = PROCESS_INFORMATION::default();
            let created = CreateProcessAsUserW(
                Some(primary),
                PCWSTR::null(),
                Some(PWSTR(cmdline.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
                (!env.is_null()).then_some(env as *const _),
                PCWSTR::null(),
                &si,
                &mut pi,
            );
            if !env.is_null() {
                let _ = DestroyEnvironmentBlock(env);
            }
            let _ = CloseHandle(primary);
            let _ = CloseHandle(in_r);
            let _ = CloseHandle(out_w);
            if let Err(e) = created {
                let _ = CloseHandle(in_w);
                let _ = CloseHandle(out_r);
                return Err(e.into());
            }
            let _ = CloseHandle(pi.hThread);

            let input = File::from_raw_handle(in_w.0);
            let mut output = File::from_raw_handle(out_r.0);
            let (files_tx, files_rx) = mpsc::channel();
            std::thread::spawn(move || {
                while let Ok(msg) = read_frame::<FromHelper>(&mut output) {
                    match msg {
                        FromHelper::Data(d) => on_change(ClipEvent::Data(d)),
                        FromHelper::Copied(p) => on_change(ClipEvent::Files(p.into_iter().map(PathBuf::from).collect())),
                        FromHelper::Files(p) => {
                            let _ = files_tx.send(p.into_iter().map(PathBuf::from).collect());
                        }
                    }
                }
            });
            Ok(Self { input: Mutex::new(input), files: Mutex::new(files_rx), process: pi.hProcess })
        }
    }

    fn send(&self, msg: &ToHelper) {
        let _ = write_frame(&mut *self.input.lock().unwrap(), msg);
    }
}

impl ClipboardBackend for HelperClipboard {
    fn set(&self, data: ClipData) {
        self.send(&ToHelper::Set(data));
    }
    fn set_files(&self, paths: Vec<PathBuf>) {
        self.send(&ToHelper::SetFiles(strings(&paths)));
    }
    fn files(&self) -> Vec<PathBuf> {
        let rx = self.files.lock().unwrap();
        while rx.try_recv().is_ok() {} // 丢掉过期的回复
        self.send(&ToHelper::GetFiles);
        rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default()
    }
}

impl Drop for HelperClipboard {
    fn drop(&mut self) {
        unsafe {
            // 关闭输入管道助手会自行退出；保险起见再结束进程
            let _ = TerminateProcess(self.process, 0);
            let _ = CloseHandle(self.process);
        }
    }
}
