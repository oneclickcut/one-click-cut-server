//! 跨层小工具：随机串、时间、哈希。

use rand::RngExt as _;

/// base62 字母表。不含容易混淆的分隔符，可以安全地放进 URL 与 key。
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// 生成 `len` 个 base62 字符的随机串。
///
/// 用于画布 slug（12 字符，约 71 bit 熵）与 agent token（32 字符）。
/// 熵足够高是**未公开画布唯一的保护手段**——读取一律公开，靠的就是 URL 不可枚举。
pub fn random_base62(len: usize) -> String {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| BASE62[rng.random_range(0..BASE62.len())] as char)
        .collect()
}

/// 当前 Unix 秒。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// SHA-256，返回 32 字节。
///
/// token 在 KV 里存的是哈希而不是明文：库被拖走也不能直接拿去用。
/// 这里用 SHA-256 而不是 BLAKE3 只是为了与 token 这类「短随机串」的
/// 常规做法一致；两者在这个场景下都够快（token 本身就是高熵的，
/// 不需要慢哈希来抵抗暴力破解）。
pub fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base62长度与字符集() {
        for len in [1, 12, 32] {
            let s = random_base62(len);
            assert_eq!(s.chars().count(), len);
            assert!(
                s.bytes().all(|b| BASE62.contains(&b)),
                "只允许 base62 字符: {s}"
            );
        }
    }

    #[test]
    fn base62不重复() {
        // 12 字符约 71 bit 熵，1000 次生成不该有碰撞。
        // 这一条其实是在验证「用了真随机源」而不是某个固定种子。
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(random_base62(12)), "12 字符 slug 出现碰撞");
        }
    }

    #[test]
    fn sha256已知值() {
        // 空串的 SHA-256 是个广为人知的常量，用它确认接线没错。
        assert_eq!(
            hex::encode(sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
