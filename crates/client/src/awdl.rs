//! AWDL 自动处理（界面里的开关）。
//!
//! 关闭 awdl0 需要 root 权限。第一次开启时通过 macOS 标准的管理员密码框安装一个后台守护
//! （packaging/macos/awdl-guard.sh）：只在远程画面窗口运行期间压住 AWDL，其余时间不干预。
//! 守护装好后，开关只是创建 / 删除配置目录里的一个文件（守护每秒检查它），不再需要管理员密码。

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = include_str!("../../../packaging/macos/awdl-guard.sh");
const PLIST: &str = "/Library/LaunchDaemons/com.remotecontrol.awdl-guard.plist";

pub fn guard_installed() -> bool {
    Path::new(PLIST).exists()
}

/// 开关文件：存在时守护才会关闭 AWDL。
fn flag() -> std::path::PathBuf {
    crate::config::config_dir().join("awdl-guard.on")
}

/// 已安装的守护不认开关文件（旧版本），切换开关前需要重新安装一次。
fn guard_outdated() -> bool {
    std::fs::read_to_string("/usr/local/libexec/remotecontrol-awdl-guard").map(|s| !s.contains("FLAG=")).unwrap_or(true)
}

/// 界面上的开关状态。
pub fn enabled() -> bool {
    guard_installed() && flag().exists()
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
fn run_privileged(action: &str, arg: &str) -> Result<()> {
    let dir = crate::config::config_dir();
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("awdl-guard.sh");
    std::fs::write(&script, SCRIPT)?;
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let cmd = format!("/bin/bash {} {action} {}", q(&script.display().to_string()), q(arg));
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

/// 打开 / 关闭开关。只有守护还没装（或是旧版）时才需要管理员密码。
pub fn set_enabled(on: bool) -> Result<()> {
    let flag = flag();
    if !on {
        let _ = std::fs::remove_file(&flag);
        return Ok(());
    }
    std::fs::create_dir_all(flag.parent().unwrap())?;
    std::fs::write(&flag, b"")?;
    if !guard_installed() || guard_outdated() {
        if let Err(e) = run_privileged("install", &flag.display().to_string()) {
            let _ = std::fs::remove_file(&flag);
            return Err(e);
        }
    }
    Ok(())
}
