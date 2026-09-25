//! 保存截图：把 PNG 按内容哈希命名后写入可配置目录。

use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// 把 PNG 字节写入保存目录，文件名 = SHA-256 十六进制 + `.png`。
/// 返回写入的完整路径。
pub fn save_png(png: &[u8]) -> Result<PathBuf> {
    let dir = crate::config::save_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("创建保存目录 {} 失败", dir.display()))?;
    let path = dir.join(format!("{}.png", sha256_hex(png)));
    std::fs::write(&path, png).with_context(|| format!("写入 {} 失败", path.display()))?;
    Ok(path)
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(&mut s, "{byte:02x}").expect("写进 String 不会失败");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
