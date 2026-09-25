//! 剪贴板。
//!
//! Wayland 的选区（剪贴板）是「由客户端持有」的：谁 set_selection，谁就要一直在线
//! 把数据喂给来粘贴的程序。消费方完成一次读取并不会自动接管 selection；本进程若
//! 随即退出，下一次粘贴就可能已经没有数据。
//!
//! 因此正常路径统一调用 `wl-copy`：它设置选区后 fork 到后台继续持有数据，主程序
//! 可以安全退出。实测同一张图片连续读取两次均能拿到完全相同的 PNG。
//!
//! **KDE 的坑**：Klipper（plasmashell 内的剪贴板管理器）默认 `SaveImages=false`
//! —— 「保存图片」没开，它就不会把图片存进历史、也不会接管所有权；同时
//! `PreventEmptyClipboard=true` 让它在持有进程退出后把剪贴板恢复成上一份内容。
//! 结果就是：文本复制能长期存活，截图却只在 `wl-copy` 常驻进程活着时能粘贴，
//! 剪贴板历史里也永远看不到截图。见 [`klipper_image_hint`]。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

/// 剪贴板里图片使用的 MIME 类型。
pub const MIME_PNG: &str = "image/png";

/// 检测到 KDE 剪贴板没开「保存图片」时给用户的提示（`None` = 不需要提示）。
///
/// 只依赖 `klipperrc` 里的这一行，不需要 D-Bus；读不到文件就按默认值 `false` 处理。
pub fn klipper_image_hint() -> Option<&'static str> {
    const HINT: &str = "提示：KDE 剪贴板默认「保存图片」是关闭的，截图不会进剪贴板历史、\
                        也只在后台持有进程存活期间可粘贴；可在 系统设置 → 剪贴板 里勾选「保存图片」。";

    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !desktop.split(':').any(|d| d.eq_ignore_ascii_case("KDE")) {
        return None;
    }
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    if klipper_saves_images(&dir.join("klipperrc")) {
        None
    } else {
        Some(HINT)
    }
}

/// `klipperrc` 里是否明确开启了 `SaveImages`。文件不存在 = 用默认值 = 没开。
fn klipper_saves_images(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|line| {
        let line = line.trim().to_ascii_lowercase();
        match line.split_once('=') {
            Some((key, value)) => {
                key.trim() == "saveimages" && matches!(value.trim(), "true" | "1" | "yes" | "on")
            }
            None => false,
        }
    })
}

/// 检查 `wl-copy` 是否可用（用于启动时给出明确的错误提示）。
pub fn wl_copy_available() -> bool {
    which("wl-copy").is_some()
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|p| p.is_file())
}

/// 把 PNG 放进系统剪贴板。
///
/// 依赖外部命令 `wl-copy`（wl-clipboard 包）。它会在设置完选区后 fork 出一个常驻
/// 后台进程持有选区，因此主进程可以安全退出。
///
/// 两个坑（都踩过）：
/// 1. **不要**用 `wait_with_output()`：那个后台进程会继承我们的 stderr 管道，
///    父进程退出后写端仍未关闭，读到 EOF 会永远阻塞。
/// 2. 即使如此也不无限等：父进程正常是毫秒级退出，超过 10 秒就认为是「已经转后台」，
///    不再阻塞。
pub fn copy_png(png: &[u8]) -> Result<()> {
    if !wl_copy_available() {
        bail!(
            "找不到 wl-copy：请安装 wl-clipboard（Arch: pacman -S wl-clipboard），\
             或改用 --save <文件> 直接保存截图"
        );
    }

    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg(MIME_PNG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        // 交给终端，避免后台进程把管道持有到天荒地老（见上面第 1 条）。
        .stderr(Stdio::inherit())
        .spawn()
        .context("启动 wl-copy 失败")?;

    {
        let mut stdin = child.stdin.take().context("wl-copy 的标准输入不可用")?;
        stdin.write_all(png).context("把 PNG 写入 wl-copy 失败")?;
        // 关闭 stdin，wl-copy 才能读完并接管选区。
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait().context("等待 wl-copy 失败")? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => bail!("wl-copy 异常退出：{:?}", status),
            None if Instant::now() >= deadline => return Ok(()),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn which_finds_sh() {
        // 用 sh 做一个「PATH 查找能工作」的冒烟测试，避免依赖具体环境里的 wl-copy。
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn klipper_save_images_detection() {
        let dir = std::env::temp_dir().join(format!("es-klipper-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("klipperrc");

        // 文件不存在 = 默认值 false = 没开
        assert!(!klipper_saves_images(&f));

        std::fs::write(&f, "[General]\nSaveImages=false\n").unwrap();
        assert!(!klipper_saves_images(&f));

        std::fs::write(&f, "[General]\nSaveImages = true\n").unwrap();
        assert!(klipper_saves_images(&f));

        std::fs::write(&f, "[General]\nSaveImages=1\n").unwrap();
        assert!(klipper_saves_images(&f));

        // 别的键里出现 saveimages 不算
        std::fs::write(&f, "[General]\nNotSaveImages=true\n").unwrap();
        assert!(!klipper_saves_images(&f));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
