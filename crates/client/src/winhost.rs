//! Windows：“允许其他设备控制这台电脑”= 安装 rc-host 系统服务（需要管理员权限）。

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::Command;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 与本程序放在同一目录的 rc-host.exe。
pub fn host_exe() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("rc-host.exe");
    p.exists().then_some(p)
}

/// 服务状态：None 表示未安装。
pub fn service_state() -> Option<&'static str> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = Command::new("sc").args(["query", "RemoteControlHost"]).creation_flags(CREATE_NO_WINDOW).output().ok()?;
    if !out.status.success() {
        return None;
    }
    // “STATE : 4 RUNNING”——数字与系统语言无关
    let text = String::from_utf8_lossy(&out.stdout);
    let code = text.lines().find(|l| l.contains("STATE") || l.contains("状态")).and_then(|l| l.split(':').nth(1)).and_then(|v| v.split_whitespace().next()).and_then(|n| n.parse::<u32>().ok());
    Some(match code {
        Some(4) => "运行中",
        Some(1) => "已停止",
        Some(2) => "正在启动",
        Some(3) => "正在停止",
        _ => "未知",
    })
}

/// 以管理员身份运行 rc-host.exe（会弹出 UAC 确认）。
pub fn run_elevated(args: &str) -> Result<()> {
    let Some(exe) = host_exe() else { bail!("找不到 rc-host.exe（应与 rc-client.exe 放在同一文件夹）") };
    let r = unsafe {
        ShellExecuteW(None, &HSTRING::from("runas"), &HSTRING::from(exe.as_os_str()), &HSTRING::from(args), PCWSTR::null(), SW_SHOWNORMAL)
    };
    if r.0 as isize <= 32 {
        bail!("没有完成（可能取消了管理员授权）");
    }
    Ok(())
}
