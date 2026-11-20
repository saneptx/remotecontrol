//! 本机物理网卡的硬件地址，随局域网发现应答发给控制端，用于网络唤醒（Wake-on-LAN）。
//! 虚拟网卡（虚拟机、VPN、代理 TUN 等）不会被网络唤醒，排除掉。

/// 名称或描述里出现这些字样的网卡视为虚拟网卡。
#[cfg_attr(not(windows), allow(dead_code))]
const VIRTUAL: &[&str] = &[
    "virtual", "vmware", "hyper-v", "virtualbox", "tap-", "tap ", "tun", "tunnel", "tailscale", "wireguard", "vpn",
    "loopback", "bluetooth", "miniport", "npcap", "wi-fi direct", "docker", "zerotier",
];

#[cfg_attr(not(windows), allow(dead_code))]
fn is_virtual(desc: &str) -> bool {
    let d = desc.to_lowercase();
    VIRTUAL.iter().any(|v| d.contains(v))
}

fn usable(mac: &[u8; 6]) -> bool {
    // 全 0、广播地址、组播地址都不是真实网卡
    *mac != [0; 6] && *mac != [0xff; 6] && mac[0] & 1 == 0 && *mac != [2, 0, 0, 0, 0, 0]
}

/// macOS 的 getifaddrs 对普通程序隐藏真实地址（返回 02:00:00:00:00:00），ifconfig 能拿到。
#[cfg(target_os = "macos")]
pub fn physical_macs() -> Vec<[u8; 6]> {
    let Ok(out) = std::process::Command::new("/sbin/ifconfig").output() else { return Vec::new() };
    parse_ifconfig(&String::from_utf8_lossy(&out.stdout))
}

/// 只要 enX（Wi-Fi、以太网、雷雳网卡）；awdl0、llw0、utun、bridge 等都不是。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_ifconfig(text: &str) -> Vec<[u8; 6]> {
    let mut out = Vec::new();
    let mut physical = false;
    for line in text.lines() {
        if !line.starts_with(['\t', ' ']) {
            physical = line.starts_with("en");
        } else if physical {
            if let Some(mac) = line.trim().strip_prefix("ether ").and_then(|m| rc_protocol::discovery::parse_mac(m.trim())) {
                if usable(&mac) && !out.contains(&mac) {
                    out.push(mac);
                }
            }
        }
    }
    out
}

#[cfg(windows)]
pub fn physical_macs() -> Vec<[u8; 6]> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST,
        IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::Networking::WinSock::AF_UNSPEC;
    const IF_TYPE_ETHERNET: u32 = 6;
    const IF_TYPE_WIFI: u32 = 71;

    let mut out = Vec::new();
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER | GAA_FLAG_SKIP_UNICAST;
    let mut size = 16 * 1024u32;
    let mut buf: Vec<u64> = Vec::new();
    for _ in 0..3 {
        buf.resize(size as usize / 8 + 1, 0);
        let r = unsafe { GetAdaptersAddresses(AF_UNSPEC.0 as u32, flags, None, Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH), &mut size) };
        if r == 0 {
            let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
            while !p.is_null() {
                let a = unsafe { &*p };
                p = a.Next;
                if !matches!(a.IfType, IF_TYPE_ETHERNET | IF_TYPE_WIFI) || a.PhysicalAddressLength != 6 {
                    continue;
                }
                let desc = unsafe { a.Description.to_string() }.unwrap_or_default();
                let name = unsafe { a.FriendlyName.to_string() }.unwrap_or_default();
                if is_virtual(&desc) || is_virtual(&name) {
                    continue;
                }
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&a.PhysicalAddress[..6]);
                if usable(&mac) && !out.contains(&mac) {
                    out.push(mac);
                }
            }
            break;
        }
        if r != 111 {
            // 111 = ERROR_BUFFER_OVERFLOW：按返回的大小重试
            break;
        }
    }
    out
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn physical_macs() -> Vec<[u8; 6]> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters() {
        assert!(is_virtual("VMware Virtual Ethernet Adapter for VMnet8"));
        assert!(is_virtual("Tailscale Tunnel"));
        assert!(is_virtual("Meta Tunnel"));
        assert!(!is_virtual("Killer(R) Wi-Fi 6E AX1675i 160MHz Wireless Network Adapter (211NGW)"));
        assert!(!is_virtual("Killer E3100G 2.5 Gigabit Ethernet Controller"));
        assert!(!usable(&[1, 0, 0x5e, 0, 0, 0xfb]));
        assert!(usable(&[0x7a, 0xdf, 0xdb, 0x37, 0x5e, 0x4e]));
        assert!(!usable(&[2, 0, 0, 0, 0, 0]));
    }

    #[test]
    fn ifconfig() {
        let text = "lo0: flags=8049<UP>\n\tinet 127.0.0.1\nen0: flags=8863<UP>\n\tether 7a:df:db:37:5e:4e\n\tinet 192.168.1.63\nawdl0: flags=8943<UP>\n\tether 3a:11:22:33:44:55\nen1: flags=8963<UP>\n\tether 36:2b:6c:1d:00:40\n";
        assert_eq!(parse_ifconfig(text), vec![[0x7a, 0xdf, 0xdb, 0x37, 0x5e, 0x4e], [0x36, 0x2b, 0x6c, 0x1d, 0x00, 0x40]]);
    }
}
