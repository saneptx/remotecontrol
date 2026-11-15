//! “允许其他设备控制这台 Mac”：把同一应用包里的 rc-host 注册为登录后自动运行的后台服务（LaunchAgent）。
//! 不需要管理员权限；首次运行时 macOS 会请求“录屏与系统录音”和“辅助功能”权限。

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "com.remotecontrol.host";

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn plist_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

pub fn log_path() -> PathBuf {
    home().join("Library/Logs/RemoteControl/host.log")
}

/// 应用包里的被控端程序（与本程序同目录）。
fn host_binary() -> Option<PathBuf> {
    let p = std::env::current_exe().ok()?.parent()?.join("rc-host");
    p.exists().then_some(p)
}

fn uid() -> String {
    Command::new("id").arg("-u").output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

pub fn enabled() -> bool {
    plist_path().exists()
}

pub fn set_enabled(on: bool) -> Result<()> {
    let domain = format!("gui/{}", uid());
    let plist = plist_path();
    if !on {
        let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
        let _ = std::fs::remove_file(&plist);
        return Ok(());
    }
    let Some(bin) = host_binary() else {
        bail!("找不到被控端程序 rc-host，请使用打包好的 RemoteControl.app");
    };
    let log = log_path();
    std::fs::create_dir_all(log.parent().unwrap())?;
    std::fs::create_dir_all(plist.parent().unwrap())?;
    // Mac 多为笔记本：默认允许睡眠（--allow-sleep），只在有连接时保持唤醒
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array><string>{}</string><string>--allow-sleep</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardOutPath</key><string>{}</string>
    <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        bin.display(),
        log.display(),
        log.display()
    );
    std::fs::write(&plist, xml)?;
    let _ = Command::new("launchctl").args(["bootout", &format!("{domain}/{LABEL}")]).output();
    let out = Command::new("launchctl").args(["bootstrap", &domain]).arg(&plist).output().context("launchctl")?;
    if !out.status.success() {
        bail!("启动被控端失败：{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// 被控端日志的最后几行（界面里显示权限提示等）。
pub fn log_tail(n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(log_path()).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}
