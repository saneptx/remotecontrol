//! Mac：界面打开期间在后台运行同一应用包里的 rc-host（子进程），界面关闭后它随之退出。
//! 首次运行时 macOS 会请求“录屏与系统录音”和“辅助功能”权限（记在 SuperControl 应用上）。

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// 旧版本用 LaunchAgent 让被控端登录后常驻，现在改为随界面运行。
const LEGACY_LABEL: &str = "com.remotecontrol.host";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn log_path() -> PathBuf {
    home().join("Library/Logs/RemoteControl/host.log")
}

/// 应用包里的被控端程序（与本程序同目录）。
fn host_binary() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("rc-host");
    p.exists().then_some(p)
}

/// 移除旧版本安装的常驻 LaunchAgent（否则它会占着端口）。
pub fn remove_legacy_agent() {
    let plist = home().join("Library/LaunchAgents").join(format!("{LEGACY_LABEL}.plist"));
    if plist.exists() {
        let uid = Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
        let _ = Command::new("launchctl").args(["bootout", &format!("gui/{uid}/{LEGACY_LABEL}")]).output();
        let _ = std::fs::remove_file(&plist);
    }
}

/// 启动被控端子进程，输出写入日志文件。
pub fn spawn() -> Result<Child> {
    let Some(bin) = host_binary() else {
        bail!("找不到被控端程序 rc-host，请使用打包好的 SuperControl.app");
    };
    let log = log_path();
    std::fs::create_dir_all(log.parent().unwrap())?;
    if std::fs::metadata(&log).is_ok_and(|m| m.len() > 5 << 20) {
        let _ = std::fs::rename(&log, log.with_extension("old.log"));
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
    // Mac 多为笔记本：允许睡眠（--allow-sleep），只在有连接时保持唤醒
    Command::new(bin)
        .args(["--allow-sleep", "--exit-with-parent"])
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .spawn()
        .context("启动被控端失败")
}

/// 被控端日志的最后几行（界面里显示权限提示等）。
pub fn log_tail(n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(log_path()).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}
