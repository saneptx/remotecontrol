//! 开机（登录后）自动启动 SuperControl，以 `--background` 启动：不弹窗口，只在托盘 / 菜单栏里。
//!
//! - macOS：`~/Library/LaunchAgents/com.supercontrol.launcher.plist`（登录时用 `open` 打开应用）
//! - Windows：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `SuperControl` 值
//!
//! 两者都只影响当前用户，不需要管理员权限。

use anyhow::Result;

/// 后台启动的命令行参数。
pub const BACKGROUND_ARG: &str = "--background";

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use anyhow::Context;
    use std::path::PathBuf;

    const LABEL: &str = "com.supercontrol.launcher";

    fn plist() -> PathBuf {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
    }

    /// 本程序所在的 .app（开发时直接运行可执行文件则为 None）。
    fn bundle() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let app = exe.ancestors().nth(3)?.to_path_buf();
        (app.extension()? == "app").then_some(app)
    }

    pub fn enabled() -> bool {
        plist().exists()
    }

    pub fn set(on: bool) -> Result<()> {
        let path = plist();
        if !on {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let args: Vec<String> = match bundle() {
            Some(app) => vec!["/usr/bin/open".into(), "-a".into(), app.display().to_string(), "--args".into(), BACKGROUND_ARG.into()],
            None => vec![std::env::current_exe()?.display().to_string(), BACKGROUND_ARG.into()],
        };
        let items: String = args.iter().map(|a| format!("<string>{}</string>", xml_escape(a))).collect();
        let xml = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{LABEL}</string>
    <key>ProgramArguments</key><array>{items}</array>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
"#
        );
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(&path, xml).with_context(|| format!("写入 {}", path.display()))?;
        Ok(())
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use anyhow::{bail, Context};
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::Command;

    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    const VALUE: &str = "SuperControl";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    fn reg(args: &[&str]) -> Result<std::process::Output> {
        Command::new("reg").args(args).creation_flags(CREATE_NO_WINDOW).output().context("运行 reg")
    }

    pub fn enabled() -> bool {
        reg(&["query", KEY, "/v", VALUE]).is_ok_and(|o| o.status.success())
    }

    pub fn set(on: bool) -> Result<()> {
        let out = if on {
            let exe: PathBuf = std::env::current_exe()?;
            let cmd = format!("\"{}\" {BACKGROUND_ARG}", exe.display());
            reg(&["add", KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &cmd, "/f"])?
        } else {
            if !enabled() {
                return Ok(());
            }
            reg(&["delete", KEY, "/v", VALUE, "/f"])?
        };
        if !out.status.success() {
            bail!("修改开机启动项失败：{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }
}

pub use imp::{enabled, set};
