//! 被控端的设备身份与信任列表（已配对的控制端）。
//!
//! 保存在 `host.json`：Windows 服务模式下在 `%ProgramData%\RemoteControl`（仅 SYSTEM 与管理员可访问），
//! 普通用户手动运行时在 `%LOCALAPPDATA%\RemoteControl`；macOS 在 `~/Library/Application Support/RemoteControl`。
//!
//! 每次访问都重新读文件：`rc-host forget` 等命令修改后，正在运行的服务立即生效。

use anyhow::{Context, Result};
use rc_protocol::{self as proto, pair::Key, DeviceId};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TrustedDevice {
    pub id: String,
    pub name: String,
    key: String,
    /// Unix 时间（秒）
    pub paired_at: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct HostConfig {
    host_id: String,
    #[serde(default)]
    trusted: Vec<TrustedDevice>,
}

pub struct Store {
    path: PathBuf,
    host_id: DeviceId,
    host_name: String,
    lock: Mutex<()>,
}

fn parse_key(s: &str) -> Option<Key> {
    let a = proto::parse_hex16(s.get(..32)?)?;
    let b = proto::parse_hex16(s.get(32..)?)?;
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(&a);
    k[16..].copy_from_slice(&b);
    Some(k)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn writable(dir: &Path) -> bool {
    std::fs::create_dir_all(dir).is_ok()
        && std::fs::OpenOptions::new().create(true).append(true).open(dir.join("host.json")).is_ok()
}

/// 配置目录。
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        let system = crate::win::service::data_dir();
        if writable(&system) {
            return system;
        }
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        local.join("RemoteControl")
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        let dir = home.join("Library/Application Support/RemoteControl");
        let _ = writable(&dir);
        dir
    }
}

/// 本机名称（显示在控制端的设备列表里）。
pub fn computer_name() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows".into())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Mac".into())
    }
}

impl Store {
    pub fn open(name: Option<String>) -> Result<Self> {
        let path = config_dir().join("host.json");
        let mut cfg = read(&path);
        let host_id = match proto::parse_hex16(&cfg.host_id) {
            Some(id) => id,
            None => {
                let id = proto::random_nonce();
                cfg.host_id = proto::hex(&id);
                write(&path, &cfg).with_context(|| format!("写入 {}", path.display()))?;
                id
            }
        };
        let host_name = name.filter(|n| !n.trim().is_empty()).unwrap_or_else(computer_name);
        Ok(Self { path, host_id, host_name, lock: Mutex::new(()) })
    }

    pub fn host_id(&self) -> DeviceId {
        self.host_id
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn key_for(&self, id: &DeviceId) -> Option<Key> {
        let hex = proto::hex(id);
        read(&self.path).trusted.iter().find(|d| d.id == hex).and_then(|d| parse_key(&d.key))
    }

    pub fn add(&self, id: &DeviceId, name: &str, key: &Key) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut cfg = read(&self.path);
        let hex = proto::hex(id);
        cfg.trusted.retain(|d| d.id != hex);
        cfg.trusted.push(TrustedDevice { id: hex, name: name.into(), key: proto::hex(key), paired_at: now_secs() });
        write(&self.path, &cfg)
    }

    pub fn list(&self) -> Vec<TrustedDevice> {
        read(&self.path).trusted
    }

    /// 按名称或标识前缀删除，返回被删除的设备。
    pub fn forget(&self, query: &str) -> Result<Vec<TrustedDevice>> {
        let _guard = self.lock.lock().unwrap();
        let mut cfg = read(&self.path);
        let q = query.trim();
        let all = q == "--all";
        let (removed, kept): (Vec<_>, Vec<_>) =
            cfg.trusted.into_iter().partition(|d| all || d.name == q || (q.len() >= 4 && d.id.starts_with(&q.to_lowercase())));
        cfg.trusted = kept;
        write(&self.path, &cfg)?;
        Ok(removed)
    }
}

fn read(path: &Path) -> HostConfig {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write(path: &Path, cfg: &HostConfig) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 询问本机用户是否允许配对。Windows 上弹出置顶对话框（60 秒无人响应视为拒绝）。
pub fn approve_pairing(client: &str, peer: &str, code: &str) -> bool {
    #[cfg(windows)]
    {
        crate::win::dialog::ask_pairing(client, peer, code)
    }
    #[cfg(not(windows))]
    {
        // 测试用：设置 RC_AUTO_ACCEPT=1 自动同意
        if std::env::var("RC_AUTO_ACCEPT").is_ok_and(|v| v == "1") {
            println!("自动同意配对：{client}（{peer}），配对码 {code}");
            return true;
        }
        #[cfg(target_os = "macos")]
        if !crate::test_source() {
            return mac_dialog(client, peer, code);
        }
        println!("「{client}」（{peer}）请求配对，配对码 {code}。允许吗？[y/N]");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).is_ok() && line.trim().eq_ignore_ascii_case("y")
    }
}

/// macOS：系统对话框确认配对（被控端在后台运行，没有终端可用）。60 秒无人响应视为拒绝。
#[cfg(target_os = "macos")]
fn mac_dialog(client: &str, peer: &str, code: &str) -> bool {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let text = format!(
        "「{}」（{}）请求远程控制这台 Mac。\n\n配对码：{}\n请确认与控制端屏幕上显示的配对码一致。\n\n允许后，该设备以后可以直接连接，无需再次确认。",
        esc(client),
        esc(peer),
        code
    );
    let script = format!(
        "display dialog \"{text}\" with title \"SuperControl 配对请求\" buttons {{\"拒绝\", \"允许\"}} default button \"拒绝\" with icon caution giving up after 60"
    );
    std::process::Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("允许"))
        .unwrap_or(false)
}

/// 设备名只用于显示：去掉控制字符，限制长度。
pub fn sanitize_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        "未命名设备".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_hex_roundtrip() {
        let k = proto::random_32();
        assert_eq!(parse_key(&proto::hex(&k)), Some(k));
        assert_eq!(parse_key("abc"), None);
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_name("  我的 Mac\n"), "我的 Mac");
        assert_eq!(sanitize_name("\u{7}"), "未命名设备");
        assert_eq!(sanitize_name(&"x".repeat(100)).len(), 64);
    }
}
