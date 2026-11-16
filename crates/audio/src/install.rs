//! 一键安装虚拟声卡（接收对方麦克风用）：从官网下载并运行安装程序。
//!
//! - Windows：VB-CABLE（vb-audio.com，捐赠软件）。安装驱动时 Windows 可能弹出“是否安装此设备软件”，需要点“安装”。
//! - macOS：BlackHole 2ch（existential.audio，开源）。安装需要管理员密码。
//!
//! 下载用系统自带的 curl（Windows 10 起自带），解压用系统自带的 tar。

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// 虚拟声卡的名称（显示用）。
pub const NAME: &str = if cfg!(windows) { "VB-CABLE" } else { "BlackHole" };

fn work_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join("SuperControl-audio-driver");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context("创建临时文件夹")?;
    Ok(dir)
}

/// 依次尝试下载，返回第一个成功的。
fn download(urls: &[&str], dest: &Path) -> Result<()> {
    let mut last = String::new();
    for url in urls {
        let mut cmd = Command::new("curl");
        cmd.args(["-L", "-f", "-s", "-S", "--max-time", "120", "-o"]).arg(dest).arg(url);
        hide_window(&mut cmd);
        match cmd.output() {
            Ok(o) if o.status.success() && std::fs::metadata(dest).is_ok_and(|m| m.len() > 100_000) => return Ok(()),
            Ok(o) => last = String::from_utf8_lossy(&o.stderr).trim().to_string(),
            Err(e) => last = e.to_string(),
        }
    }
    bail!("下载失败：{last}")
}

#[cfg(windows)]
fn hide_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
}

#[cfg(not(windows))]
fn hide_window(_cmd: &mut Command) {}

/// 等系统里出现虚拟声卡（安装程序结束后驱动可能还要几秒才就绪）。
fn wait_ready() -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if crate::virtual_cable().is_some() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!("安装程序已结束，但系统里没有出现虚拟声卡（可能取消了安装）")
}

/// 下载并安装。`elevated`：本进程已有管理员 / SYSTEM 权限（Windows 被控端服务），否则会弹出提权确认。
/// 阻塞到安装完成并且设备出现。
pub fn install(elevated: bool) -> Result<()> {
    if crate::virtual_cable().is_some() {
        return Ok(());
    }
    let dir = work_dir()?;
    let result = imp::install(&dir, elevated).and_then(|()| wait_ready());
    let _ = std::fs::remove_dir_all(&dir);
    result
}

#[cfg(windows)]
mod imp {
    use super::*;

    const URLS: &[&str] = &[
        "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack45.zip",
        "https://download.vb-audio.com/Download_CABLE/VBCABLE_Driver_Pack43.zip",
    ];

    pub fn install(dir: &Path, elevated: bool) -> Result<()> {
        let zip = dir.join("vbcable.zip");
        download(URLS, &zip)?;
        let mut tar = Command::new("tar");
        tar.arg("-xf").arg(&zip).arg("-C").arg(dir);
        hide_window(&mut tar);
        let out = tar.output().context("运行 tar")?;
        if !out.status.success() {
            bail!("解压失败：{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let setup = dir.join(if cfg!(target_arch = "x86") { "VBCABLE_Setup.exe" } else { "VBCABLE_Setup_x64.exe" });
        if !setup.exists() {
            bail!("安装包里没有 {}", setup.display());
        }
        // -i 安装，-h 不显示安装程序界面（驱动确认框仍会弹出）
        let status = if elevated {
            Command::new(&setup).args(["-i", "-h"]).status().context("运行安装程序")?
        } else {
            // 普通用户：通过 UAC 提权运行并等它结束
            let ps = format!(
                "$p = Start-Process -FilePath '{}' -ArgumentList '-i','-h' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
                setup.display().to_string().replace('\'', "''")
            );
            let mut cmd = Command::new("powershell");
            cmd.args(["-NoProfile", "-NonInteractive", "-Command", &ps]);
            hide_window(&mut cmd);
            cmd.status().context("运行安装程序")?
        };
        // VB-CABLE 安装程序的退出码不可靠，以设备是否出现为准
        let _ = status;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;

    const URLS: &[&str] = &[
        "https://existential.audio/downloads/BlackHole2ch-0.7.0.pkg",
        "https://existential.audio/downloads/BlackHole2ch-0.6.1.pkg",
    ];

    pub fn install(dir: &Path, _elevated: bool) -> Result<()> {
        let pkg = dir.join("BlackHole2ch.pkg");
        download(URLS, &pkg)?;
        // 系统的管理员密码框；安装包的脚本会重启 coreaudiod 让设备出现
        let script = format!(
            "do shell script \"/usr/sbin/installer -pkg '{}' -target /\" with administrator privileges with prompt \"SuperControl 要安装虚拟声卡 BlackHole，用来接收对方电脑的麦克风声音。\"",
            pkg.display().to_string().replace('\'', "'\\''").replace('"', "\\\"")
        );
        let out = Command::new("/usr/bin/osascript").args(["-e", &script]).output().context("运行 osascript")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains("-128") {
                bail!("已取消安装");
            }
            bail!("安装失败：{}", err.trim());
        }
        Ok(())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::*;
    pub fn install(_dir: &Path, _elevated: bool) -> Result<()> {
        bail!("这个系统不支持自动安装虚拟声卡")
    }
}
