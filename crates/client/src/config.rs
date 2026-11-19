//! 控制端配置：本机设备标识、已配对的被控端（含配对密钥）与界面设置。
//! 保存在 `~/Library/Application Support/RemoteControl/client.json`（macOS）或
//! `%APPDATA%\RemoteControl\client.json`（Windows），仅当前用户可读。

use anyhow::{Context, Result};
use rc_protocol::{self as proto, pair::Key, DeviceId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct KnownHost {
    pub id: String,
    pub name: String,
    key: String,
    /// 上次成功连接的地址（ip:port），用于手动连接或发现不到时直接连。
    #[serde(default)]
    pub last_addr: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// Command 键当作 Ctrl 发送
    #[serde(default)]
    pub cmd_as_ctrl: bool,
    /// 始终按原始分辨率传输
    #[serde(default)]
    pub native: bool,
    /// 播放远端的声音
    #[serde(default = "yes")]
    pub audio: bool,
    /// 界面打开期间允许其他设备控制本机
    #[serde(default = "yes")]
    pub host: bool,
    /// 关闭主窗口时隐藏到托盘 / 菜单栏，而不是退出
    #[serde(default = "yes")]
    pub close_to_tray: bool,
    /// 控制时让对方电脑的扬声器静音（声音照样传过来）
    #[serde(default = "yes")]
    pub mute_host: bool,
    /// 把本机麦克风传给对方（对方需要装虚拟声卡）
    #[serde(default)]
    pub mic: bool,
    /// 远程窗口在前台时，把切应用、切桌面等系统快捷键发给对方
    #[serde(default = "yes")]
    pub system_keys: bool,
}

fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self { cmd_as_ctrl: true, native: false, audio: true, host: true, close_to_tray: true, mute_host: true, mic: false, system_keys: true }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    device_id: String,
    #[serde(default)]
    hosts: Vec<KnownHost>,
    #[serde(default)]
    settings: Settings,
}

pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("RemoteControl")
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        home.join("Library/Application Support/RemoteControl")
    }
}

fn path() -> PathBuf {
    config_dir().join("client.json")
}

fn read() -> File {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write(f: &File) -> Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join("client.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(f)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path()).context("写入 client.json")
}

fn parse_key(s: &str) -> Option<Key> {
    let a = proto::parse_hex16(s.get(..32)?)?;
    let b = proto::parse_hex16(s.get(32..)?)?;
    let mut k = [0u8; 32];
    k[..16].copy_from_slice(&a);
    k[16..].copy_from_slice(&b);
    Some(k)
}

/// 本机设备标识（首次调用时生成并保存）。
pub fn device_id() -> Result<DeviceId> {
    let mut f = read();
    if let Some(id) = proto::parse_hex16(&f.device_id) {
        return Ok(id);
    }
    let id = proto::random_nonce();
    f.device_id = proto::hex(&id);
    write(&f)?;
    Ok(id)
}

pub fn device_name() -> String {
    #[cfg(windows)]
    return std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows".into());
    #[cfg(not(windows))]
    std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Mac".into())
}

pub fn key_for(host_id: &DeviceId) -> Option<Key> {
    let hex = proto::hex(host_id);
    read().hosts.iter().find(|h| h.id == hex).and_then(|h| parse_key(&h.key))
}

/// 配对成功或连接成功后记录（更新名称与地址）。
pub fn remember(host_id: &DeviceId, name: &str, key: &Key, addr: &str) -> Result<()> {
    let mut f = read();
    let hex = proto::hex(host_id);
    f.hosts.retain(|h| h.id != hex);
    f.hosts.push(KnownHost { id: hex, name: name.into(), key: proto::hex(key), last_addr: Some(addr.into()) });
    write(&f)
}

pub fn known_hosts() -> Vec<KnownHost> {
    read().hosts
}

pub fn forget(host_id: &str) -> Result<()> {
    let mut f = read();
    f.hosts.retain(|h| h.id != host_id);
    write(&f)
}

pub fn settings() -> Settings {
    read().settings
}

pub fn save_settings(s: &Settings) -> Result<()> {
    let mut f = read();
    f.settings = s.clone();
    write(&f)
}
