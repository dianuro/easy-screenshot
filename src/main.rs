//! easy-screenshot —— 极简 Linux 截图工具。
//!
//! 用法：运行 → 全屏锁定启动画面 → 鼠标拖框 → 松手后框线留在屏幕上 →
//! `Ctrl+C` 把启动帧的框内区域放进剪贴板并退出；`Ctrl+S` 按内容哈希把选区
//! 保存成 PNG（目录可在配置文件中自定义）并退出。
//!
//! 架构（每块都在对应模块的文件头里有更详细的「为什么」）：
//! - [`overlay`]：Wayland xdg-shell fullscreen 覆盖层（不可用时回退 layer-shell）+ shm 绘制 + 指针/键盘交互；
//! - [`portal`]：启动时通过 KWin ScreenShot2（授权后）或 XDG Desktop Portal 锁定整屏，
//!   再按逻辑坐标裁剪（本机 KWin 6.7 下
//!   `wlr-screencopy` 与 X11 抓屏都不可用，使用 KWin 直连或 Portal 后备）；
//! - [`clipboard`]：调 `wl-copy` 把 PNG 放进系统剪贴板；
//! - [`keys`]：按键语义判定 + SIGINT 自管道；
//! - [`config`]：配置文件解析（Ctrl+S 的保存目录）；
//! - [`save`]：按 PNG 内容哈希命名并写盘；
//! - [`geometry`]：纯几何/路径工具，全部可单测。

mod clipboard;
mod config;
mod geometry;
mod keys;
mod overlay;
mod portal;
mod save;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};

use geometry::Rect;

const EXIT_OK: u8 = 0;
const EXIT_CANCELLED: u8 = 1;
const EXIT_ERROR: u8 = 2;

#[derive(Debug)]
struct Cli {
    /// 显式安装当前程序的 KDE KWin 静默抓屏授权条目。
    install_kwin_permission: bool,
    /// 保留 portal 落在 ~/Pictures 的整屏 PNG（默认删除）。
    keep_file: bool,
    /// 额外把裁剪结果写到这个路径。
    save: Option<PathBuf>,
    /// 不调用 wl-copy。
    no_clipboard: bool,
    /// 等待 portal 响应的超时。
    timeout: Duration,
    verbose: bool,
    /// 只验证覆盖层能否建立（不抢键盘焦点，不做交互）。
    smoke: bool,
    /// 跳过交互，直接对给定逻辑矩形截图（自动化/自检用）。
    rect: Option<Rect>,
    /// 全屏锁定启动画面后预置选区（调试用）。
    preselect: Option<Rect>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("easy-screenshot: {e:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

fn run() -> Result<ExitCode> {
    let Some(cli) = parse_args()? else {
        return Ok(ExitCode::from(EXIT_OK));
    };
    if cli.install_kwin_permission {
        let path = portal::install_kwin_permission()?;
        println!("已安装 KDE KWin 静默抓屏授权：{}", path.display());
        println!("之后在 KDE Wayland 上运行 easy-screenshot 将优先使用 KWin 直连。");
        return Ok(ExitCode::from(EXIT_OK));
    }
    if cli.no_clipboard && cli.save.is_none() {
        bail!("--no-clipboard 需要配合 --save <文件>，否则截图结果无处可去");
    }
    // Wayland selection 的所有权不会在第一次读取后自动转移。直接用
    // wl_data_device 提供一次数据后，本进程一退出，剪贴板就可能变空；因此
    // 交互和 --rect 模式都统一交给会 fork 后台持有数据的 wl-copy。
    let needs_wl_copy = !cli.smoke && !cli.no_clipboard;
    if needs_wl_copy && !clipboard::wl_copy_available() {
        bail!(
            "找不到 wl-copy：请安装 wl-clipboard（Arch: pacman -S wl-clipboard），\
             或使用 --save <文件> --no-clipboard 直接保存"
        );
    }

    let created_after = SystemTime::now();

    let interactive = cli.rect.is_none();
    let outcome = overlay::run(overlay::Options {
        smoke: cli.smoke,
        preset: cli.rect,
        preselect: cli.preselect,
        capture: interactive,
        timeout: cli.timeout,
        keep_file: cli.keep_file,
        no_clipboard: cli.no_clipboard,
        verbose: cli.verbose,
    })?;

    if let overlay::Outcome::Smoke(lines) = &outcome {
        for line in lines {
            let logical = match line.logical {
                Some((x, y, w, h)) => format!("{w}x{h}+{x}+{y}"),
                None => "未知".to_string(),
            };
            println!(
                "输出 {}：全屏窗口 configure {}x{}，逻辑尺寸 {}，wl_output.scale {}",
                line.name, line.configure.0, line.configure.1, logical, line.scale_factor
            );
        }
        return Ok(ExitCode::from(EXIT_OK));
    }

    let copy = match outcome {
        overlay::Outcome::Copy(c) => c,
        overlay::Outcome::Saved(saved) => {
            if cli.verbose {
                eprintln!(
                    "[verbose] Ctrl+S 保存：选区 {}x{}+{}+{}（全局逻辑，输出 {}），已写入 {}",
                    saved.result.rect_global.w,
                    saved.result.rect_global.h,
                    saved.result.rect_global.x,
                    saved.result.rect_global.y,
                    saved.result.output_name,
                    saved.path.display()
                );
            }
            println!("已保存截图到 {}", saved.path.display());
            return Ok(ExitCode::from(EXIT_OK));
        }
        overlay::Outcome::Cancel => {
            eprintln!("已取消，剪贴板未改动。");
            return Ok(ExitCode::from(EXIT_CANCELLED));
        }
        overlay::Outcome::Smoke(_) => unreachable!("上面已经处理"),
    };
    let overlay::CopyOutcome {
        result,
        shot,
        clipboard,
    } = *copy;

    if cli.verbose {
        eprintln!(
            "[verbose] 选区 {}x{}+{}+{}（全局逻辑，输出 {}），工作区 {}x{}+{}+{}",
            result.rect_global.w,
            result.rect_global.h,
            result.rect_global.x,
            result.rect_global.y,
            result.output_name,
            result.workspace.w,
            result.workspace.h,
            result.workspace.x,
            result.workspace.y,
        );
    }

    // 交互模式已经在启动时锁定完整桌面并从该帧完成裁剪；只有 --rect 预置模式
    // 没有覆盖层，需要在这里按给定矩形抓取一次。
    let shot = match shot {
        Some(shot) => shot,
        None => {
            std::thread::sleep(Duration::from_millis(150));
            let candidates = [result.workspace, result.output_rect];
            match portal::capture(result.rect_global, &candidates, cli.timeout, cli.verbose) {
                Ok(shot) => shot,
                Err(e) if e.downcast_ref::<portal::Cancelled>().is_some() => {
                    eprintln!("截图被取消或未授权：{e}");
                    return Ok(ExitCode::from(EXIT_CANCELLED));
                }
                Err(e) => return Err(e),
            }
        }
    };

    let (w, h) = shot.crop;
    // 分数缩放下物理像素比逻辑像素多（本机 1.25 倍），两条都给出来避免误会。
    let size_note = if (w, h) != (result.rect_global.w, result.rect_global.h) {
        format!(
            " {w}x{h} 像素（选区 {}x{} 逻辑像素）",
            result.rect_global.w, result.rect_global.h
        )
    } else {
        format!(" {w}x{h} 像素")
    };

    if let Some(path) = &cli.save {
        std::fs::write(path, &shot.png).with_context(|| format!("写入 {} 失败", path.display()))?;
    }

    let mut copied = false;
    if !cli.no_clipboard {
        match clipboard {
            // 仅供默认关闭的实验性 direct-selection 路径使用；它不保证进程退出后仍可粘贴。
            overlay::Clipboard::Focused => copied = true,
            overlay::Clipboard::Skipped => {}
            overlay::Clipboard::NeedsFallback(reason) => {
                if cli.verbose {
                    eprintln!("[verbose] 使用 wl-copy 发布剪贴板数据：{reason}");
                }
                match clipboard::copy_png(&shot.png) {
                    Ok(()) => copied = true,
                    Err(e) => {
                        if cli.save.is_some() {
                            eprintln!("警告：复制到剪贴板失败（{e}），但截图已保存。");
                        } else {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    if !cli.keep_file
        && let Some(portal_path) = &shot.portal_path
    {
        if portal::should_delete_portal_file(portal_path, created_after) {
            if let Err(e) = std::fs::remove_file(portal_path) {
                eprintln!(
                    "提示：没能删掉 portal 生成的 {}（{e}）；可以用 --keep-file 明确保留，\
                     或手动删除",
                    portal_path.display()
                );
            }
        } else if cli.verbose {
            eprintln!(
                "[verbose] 保留 portal 生成的整屏文件 {}（不在可安全删除的目录内）",
                portal_path.display()
            );
        }
    }

    let saved = cli
        .save
        .as_ref()
        .map(|p| format!("，并保存到 {}", p.display()))
        .unwrap_or_default();
    match (copied, cli.save.is_some()) {
        (true, _) => println!("已复制{size_note}到剪贴板{saved}"),
        (false, true) => println!("已保存{size_note}{saved}"),
        (false, false) => unreachable!("前面已经拦截"),
    }
    // 顺便报告完整桌面帧的原始尺寸与缩放，方便排查比例问题。
    if cli.verbose {
        eprintln!(
            "[verbose] 完整桌面帧 {}x{}，裁剪按逻辑区域 {}x{}+{}+{} 换算，缩放 {:.4}x{:.4}",
            shot.source_size.0,
            shot.source_size.1,
            shot.region.w,
            shot.region.h,
            shot.region.x,
            shot.region.y,
            shot.scale.0,
            shot.scale.1
        );
    }
    // KDE 默认不保存剪贴板图片：这是「复制了却粘贴不到」的最常见原因，值一次提示。
    if copied && let Some(hint) = clipboard::klipper_image_hint() {
        eprintln!("{hint}");
    }
    Ok(ExitCode::from(EXIT_OK))
}

fn parse_args() -> Result<Option<Cli>> {
    let mut cli = Cli {
        install_kwin_permission: false,
        keep_file: false,
        save: None,
        no_clipboard: false,
        timeout: Duration::from_secs(60),
        verbose: false,
        smoke: false,
        rect: None,
        preselect: None,
    };

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("easy-screenshot {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--install-kwin-permission" => cli.install_kwin_permission = true,
            "--keep-file" => cli.keep_file = true,
            "--no-clipboard" => cli.no_clipboard = true,
            "--verbose" => cli.verbose = true,
            "--smoke" => cli.smoke = true,
            "--save" => {
                let v = args.next().context("--save 需要一个文件路径参数")?;
                cli.save = Some(PathBuf::from(v));
            }
            "--timeout" => {
                let v = args.next().context("--timeout 需要一个秒数参数")?;
                let secs: u64 = v
                    .parse()
                    .with_context(|| format!("--timeout 的参数不是合法秒数: {v}"))?;
                if secs == 0 {
                    bail!("--timeout 必须大于 0");
                }
                cli.timeout = Duration::from_secs(secs);
            }
            "--rect" => {
                let v = args.next().context("--rect 需要 X,Y,W,H 参数")?;
                cli.rect = Some(parse_rect(&v)?);
            }
            "--preselect" => {
                let v = args.next().context("--preselect 需要 X,Y,W,H 参数")?;
                cli.preselect = Some(parse_rect(&v)?);
            }
            other => bail!("未知参数 {other}（用 --help 查看用法）"),
        }
    }
    Ok(Some(cli))
}

/// 解析 `X,Y,W,H`（全局逻辑坐标）。
fn parse_rect(s: &str) -> Result<Rect> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        bail!("--rect 需要 X,Y,W,H 四个整数，例如 --rect 0,0,1536,864");
    }
    let n: Vec<i64> = parts
        .iter()
        .map(|p| {
            p.parse::<i64>()
                .with_context(|| format!("--rect 里的 {p} 不是整数"))
        })
        .collect::<Result<_>>()?;
    if n[2] <= 0 || n[3] <= 0 {
        bail!("--rect 的宽高必须为正数");
    }
    if n[2] > u32::MAX as i64 || n[3] > u32::MAX as i64 {
        bail!("--rect 的宽高过大");
    }
    Ok(Rect::new(
        n[0] as i32,
        n[1] as i32,
        n[2] as u32,
        n[3] as u32,
    ))
}

fn print_help() {
    println!(
        "\
easy-screenshot {version} —— 极简 Linux 截图工具

用法：
    easy-screenshot [选项]

运行后：
    1. 覆盖所有输出，在启动时锁定一次完整桌面画面；
    2. 冻结画面铺满屏幕、框外轻微高斯模糊并冷灰暗化，鼠标变成十字光标；
       （KDE 上使用真正的 xdg fullscreen 窗口，抑制 Activities 屏幕热角）
    3. 按住鼠标左键拖出一个框；
    4. 松开左键，框线留在冻结画面上（此时还没裁剪）；
    5. 按 Ctrl+C（或 Enter）从启动帧裁剪并放进系统剪贴板，然后退出；
       按 Ctrl+S 把当前选区保存为 PNG（文件名 = 图片哈希）并退出；
       按 Esc 或点右键取消，退出码 1。

    按 Ctrl+S 时保存目录：
       默认 $XDG_PICTURES_DIR（或 ~/Pictures），可在配置文件里用 save_dir 覆盖：
       $XDG_CONFIG_HOME/easy-screenshot/config（默认 ~/.config/easy-screenshot/config）
       例如：save_dir = /path/to/dir

选项：
    --save <文件>      额外把裁剪结果写成一个 PNG 文件
    --no-clipboard     不复制到剪贴板（需配合 --save）
    --keep-file        保留 Portal 后端落在 ~/Pictures 的整屏 PNG（默认会删除）
    --install-kwin-permission  显式准备 KDE KWin 静默抓屏授权条目（KDE 首次运行会自动执行）
    --timeout <秒>     等待抓屏响应的超时，默认 60
    --verbose          打印调试信息（选区、缩放比、抓屏耗时）
    --smoke            只验证能否建立覆盖层，打印 configure 尺寸后退出
    --rect X,Y,W,H     跳过交互，直接对给定逻辑矩形截图（自动化/自检用）
    --preselect X,Y,W,H  锁定启动画面后预置选区（调试用）

退出码：
    0 成功    1 用户取消    2 出错

依赖：
    运行在 Wayland 会话；需要 xdg-shell（否则回退 layer-shell）及截图后端；
    复制到剪贴板需要 wl-clipboard（wl-copy）。",
        version = env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_parsing() {
        assert_eq!(
            parse_rect("0,0,1536,864").unwrap(),
            Rect::new(0, 0, 1536, 864)
        );
        assert_eq!(
            parse_rect(" 10 , 20 , 30 , 40 ").unwrap(),
            Rect::new(10, 20, 30, 40)
        );
        assert!(parse_rect("0,0,10").is_err());
        assert!(parse_rect("0,0,0,10").is_err());
        assert!(parse_rect("a,0,10,10").is_err());
    }
}
