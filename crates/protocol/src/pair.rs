//! 配对与认证。
//!
//! 首次连接时双方各生成一对 X25519 临时密钥，交换公钥后得到共享密钥，再派生出长期的
//! 配对密钥 `K`（双方各自保存，网络上不传输）。双方屏幕上显示由 `K` 计算出的 6 位配对码，
//! 被控端用户核对一致并点“允许”后配对完成。
//!
//! 之后每次连接都用 `K` 做双向挑战应答：控制端证明自己配对过（[`client_mac`]），
//! 被控端也证明自己是配对时的那台机器（[`host_proof`]），防止有人冒充被控端截获键盘输入。

use crate::DeviceId;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

pub type Key = [u8; 32];

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

/// 一次配对用的临时密钥对。
pub struct Ephemeral {
    secret: StaticSecret,
    pub public: [u8; 32],
}

impl Ephemeral {
    pub fn generate() -> Self {
        let secret = StaticSecret::from(crate::random_32());
        let public = PublicKey::from(&secret).to_bytes();
        Self { secret, public }
    }

    /// 由对方公钥派生配对密钥。对方给出无效公钥（低阶点，共享密钥全零）时返回 `None`。
    pub fn derive(&self, their_public: &[u8; 32], host_id: &DeviceId, client_id: &DeviceId) -> Option<Key> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*their_public));
        if !shared.was_contributory() {
            return None;
        }
        // HKDF-SHA256：extract + 一轮 expand
        let prk = hmac(b"remotecontrol-pair-v1", &[shared.as_bytes()]);
        Some(hmac(&prk, &[host_id, client_id, &[1]]))
    }
}

/// 两边屏幕上显示的 6 位配对码，用于人工核对没有被中间人替换公钥。
pub fn pair_code(key: &Key) -> String {
    let h = hmac(key, &[b"code"]);
    format!("{:06}", u32::from_le_bytes([h[0], h[1], h[2], h[3]]) % 1_000_000)
}

pub fn client_mac(key: &Key, host_nonce: &[u8; 16], client_nonce: &[u8; 16]) -> [u8; 32] {
    hmac(key, &[b"client", host_nonce, client_nonce])
}

pub fn host_proof(key: &Key, host_nonce: &[u8; 16], client_nonce: &[u8; 16]) -> [u8; 32] {
    hmac(key, &[b"host", client_nonce, host_nonce])
}

/// 常数时间比较。
pub fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_same_key_and_code() {
        let (host_id, client_id) = ([1u8; 16], [2u8; 16]);
        let h = Ephemeral::generate();
        let c = Ephemeral::generate();
        let kh = h.derive(&c.public, &host_id, &client_id).unwrap();
        let kc = c.derive(&h.public, &host_id, &client_id).unwrap();
        assert_eq!(kh, kc);
        assert_eq!(pair_code(&kh), pair_code(&kc));
        assert_eq!(pair_code(&kh).len(), 6);
        // 换一对临时密钥，得到不同的配对密钥
        let c2 = Ephemeral::generate();
        assert_ne!(h.derive(&c2.public, &host_id, &client_id).unwrap(), kh);
        // 身份绑定进密钥
        assert_ne!(h.derive(&c.public, &client_id, &host_id).unwrap(), kh);
    }

    #[test]
    fn rejects_low_order_point() {
        let h = Ephemeral::generate();
        assert!(h.derive(&[0u8; 32], &[1; 16], &[2; 16]).is_none());
    }

    #[test]
    fn macs_are_directional() {
        let k = [7u8; 32];
        let (hn, cn) = ([1u8; 16], [2u8; 16]);
        let c = client_mac(&k, &hn, &cn);
        assert!(ct_eq(&c, &client_mac(&k, &hn, &cn)));
        assert!(!ct_eq(&c, &host_proof(&k, &hn, &cn)));
        assert!(!ct_eq(&c, &client_mac(&[8u8; 32], &hn, &cn)));
    }
}
