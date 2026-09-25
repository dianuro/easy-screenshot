//! 配置文件解析：允许自定义 Ctrl+S 的保存目录。
//!
//! 文件位置：`$XDG_CONFIG_HOME/easy-screenshot/config`（默认
//! `~/.config/easy-screenshot/config`）。格式为一行一个 `key = value`，
//! 目前只认 `save_dir`，`#` 开头的行为注释，值可用引号包裹，`~/` 会展开成 HOME。

use std::path::PathBuf;

const CONFIG_REL: &str = "easy-screenshot/config";

/// Ctrl+S 保存截图的目标目录。
///
/// 优先读配置文件里的 `save_dir`，否则用 `XDG_PICTURES_DIR`，
/// 再退回到 `~/Pictures`。
pub fn save_dir() -> PathBuf {
    read_save_dir_from_config().unwrap_or_else(default_save_dir)
}

fn default_save_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_PICTURES_DIR") {
        return PathBuf::from(d);
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join("Pictures");
    }
    PathBuf::from("Pictures")
}

fn config_path() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(d).join(CONFIG_REL));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join(CONFIG_REL))
}

fn read_save_dir_from_config() -> Option<PathBuf> {
    let path = config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "save_dir" {
            continue;
        }
        let value = value.trim().trim_matches('"').trim_matches('\'');
        if value.is_empty() {
            return None;
        }
        return Some(expand_home(value));
    }
    None
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_home() {
        let home = std::env::var_os("HOME").expect("测试环境应有 HOME");
        assert_eq!(
            expand_home("~/Pictures"),
            PathBuf::from(&home).join("Pictures")
        );
        assert_eq!(expand_home("~"), PathBuf::from(&home));
        assert_eq!(expand_home("/tmp/x"), PathBuf::from("/tmp/x"));
    }
}
