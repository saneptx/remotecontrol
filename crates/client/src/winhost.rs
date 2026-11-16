//! Windows：被控端是一个手动启动的系统服务（安装一次，需要管理员权限）。
//! 界面打开时启动它，界面运行期间持有全局互斥量；界面关闭后服务发现互斥量消失就自行停止。
//! 安装时服务已允许普通用户启动 / 停止，所以这里不需要管理员权限。

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::process::Command;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const SERVICE: &str = "RemoteControlHost";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 声明“界面正在运行”：互斥量随进程存在，进程退出（包括崩溃）时系统自动释放。
pub fn hold_ui_mutex() {
    // 句柄故意不关闭
    let _ = unsafe { CreateMutexW(None, false, &HSTRING::from(rc_protocol::UI_MUTEX)) };
}

/// 与本程序放在同一目录的 rc-host.exe。
pub fn host_exe() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("rc-host.exe");
    p.exists().then_some(p)
}

fn sc(args: &[&str]) -> Option<std::process::Output> {
    use std::os::windows::process::CommandExt;
    Command::new("sc").args(args).creation_flags(CREATE_NO_WINDOW).output().ok()
}

/// 服务状态码（1 已停止 / 2 正在启动 / 3 正在停止 / 4 运行中）；None 表示未安装。
pub fn service_state() -> Option<u32> {
    let out = sc(&["query", SERVICE])?;
    if !out.status.success() {
        return None;
    }
    // “STATE : 4 RUNNING”——数字与系统语言无关
    let text = String::from_utf8_lossy(&out.stdout);
    let code = text.lines().find(|l| l.contains("STATE") || l.contains("状态")).and_then(|l| l.split(':').nth(1)).and_then(|v| v.split_whitespace().next()).and_then(|n| n.parse::<u32>().ok());
    Some(code.unwrap_or(0))
}

pub fn state_text(state: Option<u32>) -> &'static str {
    match state {
        None => "未安装",
        Some(4) => "运行中",
        Some(1) => "已停止",
        Some(2) => "正在启动",
        Some(3) => "正在停止",
        Some(_) => "未知",
    }
}

/// 启动 / 停止服务；失败时返回 sc 的输出。
pub fn control(start: bool) -> Result<()> {
    let Some(out) = sc(&[if start { "start" } else { "stop" }, SERVICE]) else { bail!("无法运行 sc") };
    if out.status.success() {
        return Ok(());
    }
    // 1056 已在运行 / 1062 未启动：都算成功
    let text = String::from_utf8_lossy(&out.stdout);
    if text.contains("1056") || text.contains("1062") {
        return Ok(());
    }
    if text.contains("失败 5:") || text.contains("FAILED 5:") {
        bail!("没有权限启动被控端服务，请点“重新安装”（旧版本安装的服务需要更新一次）");
    }
    bail!("{} 被控端服务失败：{}", if start { "启动" } else { "停止" }, text.lines().filter(|l| !l.trim().is_empty()).last().unwrap_or("").trim());
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
