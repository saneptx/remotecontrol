//! AWDL 自动处理（界面里的开关）。
//!
//! 关闭 awdl0 需要 root 权限。开启开关时通过 macOS 标准的管理员密码框安装一个后台守护
//! （packaging/macos/awdl-guard.sh）：只在远程画面窗口运行期间压住 AWDL，其余时间不干预。

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = include_str!("../../../packaging/macos/awdl-guard.sh");
const PLIST: &str = "/Library/LaunchDaemons/com.remotecontrol.awdl-guard.plist";

pub fn guard_installed() -> bool {
    Path::new(PLIST).exists()
}

/// 已安装的是旧版守护（按进程名 rc-client 判断，主界面开着也会关闭 AWDL），需要更新。
pub fn guard_outdated() -> bool {
    guard_installed()
        && std::fs::read_to_string("/usr/local/libexec/remotecontrol-awdl-guard").map(|s| !s.contains("rc-client view")).unwrap_or(false)
}

/// AWDL 当前是否开启。
pub fn active() -> bool {
    Command::new("/sbin/ifconfig")
        .arg("awdl0")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().next().is_some_and(|l| l.contains("<UP")))
        .unwrap_or(false)
}

/// 以管理员身份运行守护脚本（会弹出系统的密码框）。`action` 为 install / uninstall。
fn run_privileged(action: &str) -> Result<()> {
    let dir = crate::config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("awdl-guard.sh");
    std::fs::write(&script, SCRIPT)?;
    let cmd = format!("/bin/bash '{}' {action}", script.display().to_string().replace('\'', "'\\''"));
    let apple = format!(
        "do shell script \"{}\" with prompt \"RemoteControl 需要管理员权限来设置 AWDL 自动处理。\" with administrator privileges",
        cmd.replace('\\', "\\\\").replace('"', "\\\"")
    );
    let out = Command::new("osascript").args(["-e", &apple]).output().context("运行 osascript")?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("-128") {
            bail!("已取消");
        }
        bail!("{}", err.trim());
    }
    Ok(())
}

pub fn set_enabled(on: bool) -> Result<()> {
    run_privileged(if on { "install" } else { "uninstall" })
}
