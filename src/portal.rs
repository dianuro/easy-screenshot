//! 通过 KDE KWin ScreenShot2 或 XDG Desktop Portal 截图。
//!
//! KDE 上优先走 KWin 原生直连接口，避免 xdg-desktop-portal-kde 即使在
//! `interactive=false` 时也短暂显示自己的 ScreenshotDialog；未授权时回退 Portal。
//!
//! 本机（KWin 6.7 / Plasma）实测结论，也是这个模块存在的原因：
//! - `wlr-screencopy` / `ext-image-copy-capture` 不存在，`grim` 直接报
//!   "compositor doesn't support the screen capture protocol"；
//! - XWayland 的 root 窗口 `XGetImage` 报 `BadMatch`；
//! - `org.kde.KWin.ScreenShot2` 有「截图沙箱」授权限制（需要带
//!   `X-KDE-DBUS-Restricted-Interfaces` 的 .desktop 文件才会被放行）；
//! - 已安装 KDE 授权条目时，`org.kde.KWin.ScreenShot2.CaptureWorkspace` 可无 Portal UI 直连；
//! - 否则 `org.freedesktop.portal.Screenshot` 作为跨桌面后备路径。
//!
//! portal 返回的是**整屏 PNG 的原生像素尺寸**（本机 1920×1080，而逻辑尺寸是
//! 1536×864）。交互模式在启动时保留完整 RGBA 帧作为冻结背景，确认选区时只从
//! 这份原始帧裁剪；物理/逻辑坐标换算见 [`crate::geometry::plan_crop`]。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use zbus::zvariant::{OwnedValue, Value};

use crate::geometry::{self, Crop, Rect};

const PORTAL_SERVICE: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENSHOT_IFACE: &str = "org.freedesktop.portal.Screenshot";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const KWIN_SERVICE: &str = "org.kde.KWin.ScreenShot2";
const KWIN_PATH: &str = "/org/kde/KWin/ScreenShot2";
const KWIN_IFACE: &str = "org.kde.KWin.ScreenShot2";

/// 用户在 portal 侧取消（或拒绝权限）。main 用它区分退出码 1 和 2。
#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "portal 截图被取消或未授权")
    }
}

impl std::error::Error for Cancelled {}

/// Portal 抓取并解码后的完整桌面帧。
///
/// 交互模式会长期持有这份 RGBA 数据：覆盖层显示它、最终裁剪也只使用它，
/// 因而用户选区期间桌面即使继续变化，也不会混入最终截图。
pub struct FullShot {
    /// 完整桌面图像的物理像素尺寸。
    pub size: (u32, u32),
    /// 完整桌面的 RGBA8 像素。
    pub rgba: Vec<u8>,
    /// Portal 后端落盘的文件；KWin 直连没有文件，因此为 `None`。
    pub portal_path: Option<PathBuf>,
    /// 发起本次请求之前的时间，用于保守的文件删除守卫。
    pub created_after: SystemTime,
}

/// 一次成功采集的产物。
pub struct CroppedShot {
    /// 裁剪后的 PNG 字节。
    pub png: Vec<u8>,
    /// 裁剪结果的**实际像素**尺寸（物理像素，可能大于逻辑尺寸，例如 1.25 倍缩放）。
    pub crop: (u32, u32),
    /// portal 返回的整屏图像尺寸（物理像素）。
    pub source_size: (u32, u32),
    /// 实际用来换算的逻辑区域。
    pub region: Rect,
    /// 缩放比（x, y）。
    pub scale: (f64, f64),
    /// Portal 后端落盘的文件；KWin 直连时为 `None`。
    pub portal_path: Option<PathBuf>,
}

/// 抓取并解码完整桌面，不在这里裁剪。
///
/// KDE/Wayland 优先尝试 KWin 的受限直连接口，避免 xdg-desktop-portal-kde
/// 即使在 `interactive=false` 时仍短暂创建自己的对话框。未授权或非 KDE
/// 环境则回退到标准 XDG Desktop Portal。
pub fn capture_full(timeout: Duration, verbose: bool) -> Result<FullShot> {
    let force_portal = std::env::var_os("EASY_SCREENSHOT_FORCE_PORTAL").is_some();
    if !force_portal && is_kde_session() {
        match ensure_kwin_permission() {
            Ok(path) => {
                if verbose {
                    eprintln!("[verbose] KDE 静默抓屏授权已就绪：{}", path.display());
                }
            }
            Err(e) => {
                if verbose {
                    eprintln!("[verbose] 自动准备 KDE 静默抓屏授权失败，继续尝试 Portal：{e:#}");
                }
            }
        }
        match capture_kwin_full(timeout, verbose) {
            Ok(shot) => return Ok(shot),
            Err(e) => {
                if verbose {
                    eprintln!("[verbose] KWin 静默抓取不可用，回退到 XDG Portal：{e:#}");
                }
            }
        }
    }
    capture_portal_full(timeout, verbose)
}

fn is_kde_session() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("KDE"))
}

/// 确保当前可执行文件拥有 KDE 的受限 D-Bus 授权条目。
///
/// KDE 首次抓屏前会自动创建/更新该文件，因此不需要用户额外执行命令。
/// 只有内容已经匹配时才写入，移动或重新编译二进制后会自动更新 `Exec`。
pub fn ensure_kwin_permission() -> Result<PathBuf> {
    let (path, contents) = kwin_permission_file()?;
    if std::fs::read_to_string(&path).ok().as_deref() != Some(contents.as_str()) {
        std::fs::write(&path, contents).with_context(|| format!("写入 {} 失败", path.display()))?;
    }
    // KWin 的 KApplicationTrader 可能在进程内缓存应用列表；即使文件内容没变，
    // 也要刷新一次，确保新增的桌面文件能被当前 KWin 进程看到。
    refresh_kde_service_cache();
    Ok(path)
}

/// KWin 通过 KApplicationTrader 的缓存查找 `.desktop` 授权条目。
/// 写入文件后必须刷新缓存，否则本次进程仍可能看到旧的服务列表并回退 Portal。
fn refresh_kde_service_cache() {
    match Command::new("kbuildsycoca6")
        .arg("--noincremental")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => {
            // kbuildsycoca6 通知 KDE 服务更新缓存后，短暂等待缓存切换完成。
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(status) => {
            eprintln!(
                "[verbose] 刷新 KDE 应用缓存失败（退出状态 {status}），KWin 静默抓屏可能不可用"
            );
        }
        Err(e) => {
            eprintln!("[verbose] 找不到 kbuildsycoca6，KWin 静默抓屏可能不可用：{e}");
        }
    }
}

/// 显式安装 KDE 授权条目；正常 KDE 启动会自动调用同样的逻辑。
pub fn install_kwin_permission() -> Result<PathBuf> {
    ensure_kwin_permission()
}

fn kwin_permission_file() -> Result<(PathBuf, String)> {
    let executable = std::env::current_exe()
        .context("获取当前程序路径失败")?
        .canonicalize()
        .context("解析当前程序真实路径失败")?;
    let executable = executable
        .to_str()
        .context("当前程序路径不是 UTF-8，无法写入桌面授权条目")?;
    let escaped = executable.replace('\\', "\\\\").replace('"', "\\\"");
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .context("找不到 XDG_DATA_HOME 或 HOME，无法安装 KDE 授权条目")?;
    let applications = data_home.join("applications");
    std::fs::create_dir_all(&applications)
        .with_context(|| format!("创建 {} 失败", applications.display()))?;
    let path = applications.join("easy-screenshot.desktop");
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=easy-screenshot\n\
         Exec=\"{escaped}\"\n\
         Terminal=false\n\
         NoDisplay=true\n\
         X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2\n"
    );
    Ok((path, contents))
}

/// KDE 的原生 ScreenShot2 接口：直接接收 KWin 的原始像素，不创建 Portal UI。
fn capture_kwin_full(timeout: Duration, verbose: bool) -> Result<FullShot> {
    let started = Instant::now();
    let created_after = SystemTime::now();
    let (mut file, temp_path) = create_kwin_temp_file()?;

    let result = (|| -> Result<FullShot> {
        let conn = zbus::blocking::connection::Builder::session()?
            .method_timeout(timeout)
            .build()
            .context("连接 KWin ScreenShot2 失败")?;
        let mut options: HashMap<String, Value<'static>> = HashMap::new();
        options.insert("native-resolution".into(), Value::from(true));
        options.insert("hide-caller-windows".into(), Value::from(true));
        let fd = zbus::zvariant::Fd::from(file.as_fd());
        let reply = conn
            .call_method(
                Some(KWIN_SERVICE),
                KWIN_PATH,
                Some(KWIN_IFACE),
                "CaptureWorkspace",
                &(options, fd),
            )
            .context("调用 KWin ScreenShot2.CaptureWorkspace 失败")?;
        let metadata: HashMap<String, OwnedValue> = reply
            .body()
            .deserialize()
            .context("解析 KWin ScreenShot2 返回值失败")?;
        if metadata
            .get("type")
            .and_then(owned_value_as_string)
            .as_deref()
            != Some("raw")
        {
            bail!("KWin ScreenShot2 没有返回 raw 图像数据");
        }
        let width = metadata_u32(&metadata, "width")?;
        let height = metadata_u32(&metadata, "height")?;
        let stride = metadata_u32(&metadata, "stride")?;
        let format = metadata_u32(&metadata, "format")?;
        let expected = (stride as usize)
            .checked_mul(height as usize)
            .context("KWin 图像尺寸溢出")?;
        wait_for_file_size(&file, expected as u64, timeout, started)?;
        file.seek(SeekFrom::Start(0))
            .context("定位 KWin 原始图像失败")?;
        let mut raw = Vec::with_capacity(expected);
        file.read_to_end(&mut raw)
            .context("读取 KWin 原始图像失败")?;
        if raw.len() < expected {
            bail!("KWin 原始图像不完整：{} < {} 字节", raw.len(), expected);
        }
        let rgba = decode_kwin_raw(&raw, width, height, stride, format)?;
        if verbose {
            eprintln!(
                "[verbose] KWin 直接返回完整桌面 {}x{}，耗时 {:?}",
                width,
                height,
                started.elapsed()
            );
        }
        Ok(FullShot {
            size: (width, height),
            rgba,
            portal_path: None,
            created_after,
        })
    })();

    if let Err(e) = std::fs::remove_file(&temp_path)
        && verbose
    {
        eprintln!(
            "[verbose] 没能删除 KWin 临时原始帧 {}（{e}）",
            temp_path.display()
        );
    }
    result
}

fn create_kwin_temp_file() -> Result<(File, PathBuf)> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for nonce in 0..100u32 {
        let path = std::env::temp_dir().join(format!(
            "easy-screenshot-kwin-{}-{stamp}-{nonce}.raw",
            std::process::id()
        ));
        match OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => return Ok((file, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("创建 KWin 临时原始帧失败"),
        }
    }
    bail!("无法创建唯一的 KWin 临时原始帧")
}

fn wait_for_file_size(
    file: &File,
    expected: u64,
    timeout: Duration,
    started: Instant,
) -> Result<()> {
    loop {
        let size = file.metadata().context("读取 KWin 临时帧大小失败")?.len();
        if size >= expected {
            return Ok(());
        }
        let elapsed = started.elapsed();
        if elapsed >= timeout {
            bail!(
                "等待 KWin 原始帧超时：{size} / {expected} 字节（{} 秒）",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(20).min(timeout - elapsed));
    }
}

fn metadata_u32(metadata: &HashMap<String, OwnedValue>, key: &str) -> Result<u32> {
    let value = metadata
        .get(key)
        .with_context(|| format!("KWin ScreenShot2 返回值缺少 {key}"))?;
    let parsed = match &**value {
        Value::U8(v) => u32::from(*v),
        Value::U16(v) => u32::from(*v),
        Value::U32(v) => *v,
        Value::U64(v) => u32::try_from(*v).context("KWin 图像尺寸超出 u32")?,
        Value::I32(v) if *v >= 0 => *v as u32,
        Value::I64(v) if *v >= 0 => u32::try_from(*v).context("KWin 图像尺寸超出 u32")?,
        _ => bail!("KWin ScreenShot2 的 {key} 不是非负整数"),
    };
    Ok(parsed)
}

fn decode_kwin_raw(
    raw: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
) -> Result<Vec<u8>> {
    // QImage::Format_RGB32 / ARGB32 / ARGB32_Premultiplied。
    let bytes_per_pixel = match format {
        4..=6 => 4usize,
        other => bail!("KWin 返回了暂不支持的 QImage 格式 {other}"),
    };
    let row_bytes = (width as usize)
        .checked_mul(bytes_per_pixel)
        .context("KWin 图像行宽溢出")?;
    let stride = stride as usize;
    if stride < row_bytes {
        bail!("KWin stride {stride} 小于图像行宽 {row_bytes}");
    }
    let expected = stride
        .checked_mul(height as usize)
        .context("KWin 图像高度溢出")?;
    if raw.len() < expected {
        bail!("KWin 原始帧不完整：{} < {expected} 字节", raw.len());
    }
    let mut rgba = Vec::with_capacity(
        (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .context("KWin RGBA 图像大小溢出")?,
    );
    for y in 0..height as usize {
        let row = y * stride;
        for x in 0..width as usize {
            let p = row + x * bytes_per_pixel;
            let blue = raw[p];
            let green = raw[p + 1];
            let red = raw[p + 2];
            let alpha = match format {
                4 => 255,
                5 => raw[p + 3],
                _ => raw[p + 3],
            };
            let (red, green, blue) = if format == 6 && alpha > 0 {
                (
                    unpremultiply(red, alpha),
                    unpremultiply(green, alpha),
                    unpremultiply(blue, alpha),
                )
            } else if format == 6 {
                (0, 0, 0)
            } else {
                (red, green, blue)
            };
            rgba.extend_from_slice(&[red, green, blue, alpha]);
        }
    }
    Ok(rgba)
}

fn unpremultiply(channel: u8, alpha: u8) -> u8 {
    (((channel as u16 * 255) + (alpha as u16 / 2)) / alpha as u16).min(255) as u8
}

fn capture_portal_full(timeout: Duration, verbose: bool) -> Result<FullShot> {
    let started = Instant::now();
    let created_after = SystemTime::now();
    let uri = request_screenshot(timeout, verbose)?;
    let path = geometry::file_uri_to_path(&uri)
        .with_context(|| format!("portal 返回的 uri 无法解析为文件路径: {uri}"))?;

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            cleanup_capture_file(&path, created_after, verbose);
            return Err(e).with_context(|| format!("读取 portal 截图失败: {}", path.display()));
        }
    };
    let (sw, sh, rgba) = match decode_png_rgba(&bytes) {
        Ok(decoded) => decoded,
        Err(e) => {
            cleanup_capture_file(&path, created_after, verbose);
            return Err(e).context("解码 portal 返回的 PNG 失败");
        }
    };

    if verbose {
        eprintln!(
            "[verbose] portal 返回完整桌面 {}x{}，耗时 {:?}",
            sw,
            sh,
            started.elapsed()
        );
    }

    Ok(FullShot {
        size: (sw, sh),
        rgba,
        portal_path: Some(path),
        created_after,
    })
}

/// 从已经抓取的完整桌面帧裁剪出 `rect`，不再访问屏幕。
pub fn crop_full(
    shot: &FullShot,
    rect: Rect,
    candidates: &[Rect],
    verbose: bool,
) -> Result<CroppedShot> {
    let (sw, sh) = shot.size;
    let crop: Crop = geometry::plan_crop(rect, (sw, sh), candidates).with_context(|| {
        format!(
            "选区 {}x{}+{}+{} 无法映射到 {}x{} 的启动画面上（逻辑区域候选: {:?}）",
            rect.w, rect.h, rect.x, rect.y, sw, sh, candidates
        )
    })?;

    // 反推实际使用的比例，便于 --verbose 排查缩放问题。
    let region = geometry::plan_crop_region(rect, (sw, sh), candidates).unwrap_or(rect);
    let scale = (sw as f64 / region.w as f64, sh as f64 / region.h as f64);
    let cropped = crop_rgba(&shot.rgba, (sw, sh), crop);
    let png = encode_png_rgba(crop.w, crop.h, &cropped)?;

    if verbose {
        eprintln!(
            "[verbose] 从完整桌面帧裁剪 {}x{}+{}+{}，缩放 {:.4}x{:.4}",
            crop.w, crop.h, crop.x, crop.y, scale.0, scale.1
        );
    }

    Ok(CroppedShot {
        png,
        crop: (crop.w, crop.h),
        source_size: (sw, sh),
        region,
        scale,
        portal_path: shot.portal_path.clone(),
    })
}

/// 放弃完整帧时删除 portal 文件；`--keep-file` 的调用方不会调用这里。
pub fn discard_full_shot(shot: &FullShot, verbose: bool) {
    if let Some(path) = &shot.portal_path {
        cleanup_capture_file(path, shot.created_after, verbose);
    }
}

fn cleanup_capture_file(path: &std::path::Path, created_after: SystemTime, verbose: bool) {
    if !should_delete_portal_file(path, created_after) {
        return;
    }
    if let Err(e) = std::fs::remove_file(path)
        && verbose
    {
        eprintln!(
            "[verbose] 没能删除被放弃的 portal 截图 {}（{e}）",
            path.display()
        );
    }
}

/// 调用 portal 截整屏，然后裁剪出 `rect`，编码成 PNG 返回。
pub fn capture(
    rect: Rect,
    candidates: &[Rect],
    timeout: Duration,
    verbose: bool,
) -> Result<CroppedShot> {
    let full = capture_full(timeout, verbose)?;
    let result = crop_full(&full, rect, candidates, verbose);
    if result.is_err() {
        discard_full_shot(&full, verbose);
    }
    result
}

/// 发起一次非交互截图，等待并返回结果的 `file://` uri。
fn request_screenshot(timeout: Duration, verbose: bool) -> Result<String> {
    let conn = zbus::blocking::Connection::session().context("连接会话 D-Bus 失败")?;
    let unique = conn
        .unique_name()
        .context("拿不到 D-Bus unique name")?
        .to_string();

    // portal 约定：带 handle_token 时，请求对象路径可以由调用方自己推导出来。
    // 先订阅再调用，避免信号比订阅先到的竞态。
    let sender = unique.trim_start_matches(':').replace('.', "_");
    let token = format!("eas{:x}", std::process::id());
    let expected = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(REQUEST_IFACE)
        .context("构造 MatchRule 失败")?
        .member("Response")
        .context("构造 MatchRule 失败")?
        .path(expected.as_str())
        .context("构造 MatchRule 失败")?
        .build();

    let messages = zbus::blocking::MessageIterator::for_match_rule(rule, &conn, Some(4))
        .context("订阅 portal Response 信号失败")?;

    let mut options: HashMap<String, Value<'static>> = HashMap::new();
    options.insert("handle_token".into(), Value::from(token.clone()));
    options.insert("interactive".into(), Value::from(false));
    options.insert("modal".into(), Value::from(false));

    let reply = conn
        .call_method(
            Some(PORTAL_SERVICE),
            PORTAL_PATH,
            Some(SCREENSHOT_IFACE),
            "Screenshot",
            &("".to_string(), options),
        )
        .with_context(|| {
            format!(
                "调用 {SCREENSHOT_IFACE} 失败；请确认已安装并运行 xdg-desktop-portal \
                 以及对应的截图后端（本机为 xdg-desktop-portal-kde）"
            )
        })?;

    let handle: zbus::zvariant::OwnedObjectPath = reply
        .body()
        .deserialize()
        .context("解析 Screenshot() 返回值失败")?;
    if handle.as_str() != expected {
        if verbose {
            eprintln!("[verbose] portal 使用了非预期路径 {handle}（预期 {expected}），仍继续等待");
        }
        bail!("portal 返回的请求路径 {handle} 与预期 {expected} 不一致，无法可靠等待响应");
    }

    // 阻塞式等待信号；超时后直接放弃（线程会被进程退出带走）。
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for msg in messages.flatten() {
            if tx.send(msg).is_err() {
                break;
            }
        }
    });

    let msg = rx.recv_timeout(timeout).map_err(|_| {
        anyhow::anyhow!(
            "等待 portal 截图响应超时（{} 秒）。若屏幕上出现了权限对话框，请在 \
             「系统设置 → 应用权限」里允许本程序截图",
            timeout.as_secs()
        )
    })?;

    let body = msg.body();
    let (response, results): (u32, HashMap<String, OwnedValue>) = body
        .deserialize()
        .context("解析 portal Response 信号失败")?;

    if response != 0 {
        // 1 = 用户取消，2 = 其他错误
        return Err(Cancelled.into());
    }

    let uri = results
        .get("uri")
        .and_then(owned_value_as_string)
        .context("portal 的响应里没有 uri 字段")?;
    Ok(uri)
}

fn owned_value_as_string(v: &OwnedValue) -> Option<String> {
    match &**v {
        Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

/// PNG → (宽, 高, RGBA8 像素)
pub fn decode_png_rgba(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // 缩小内存占用：单线程解码即可。
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().context("读取 PNG 头失败")?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).context("解码 PNG 数据帧失败")?;
    buf.truncate(info.buffer_size());

    let (w, h) = (info.width, info.height);
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        other => bail!("不支持的 PNG 颜色类型: {other:?}"),
    };

    let px = (w as usize) * (h as usize);
    if buf.len() < px * channels {
        bail!("PNG 数据不完整: {} < {}", buf.len(), px * channels);
    }

    let rgba = match channels {
        4 => buf,
        3 => {
            let mut out = Vec::with_capacity(px * 4);
            for c in buf.chunks_exact(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 0xFF]);
            }
            out
        }
        2 => {
            let mut out = Vec::with_capacity(px * 4);
            for c in buf.chunks_exact(2) {
                out.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
            out
        }
        _ => {
            let mut out = Vec::with_capacity(px * 4);
            for g in buf.iter() {
                out.extend_from_slice(&[*g, *g, *g, 0xFF]);
            }
            out
        }
    };
    Ok((w, h, rgba))
}

/// 从 RGBA8 大图里裁出 `crop`。
pub fn crop_rgba(src: &[u8], src_size: (u32, u32), crop: Crop) -> Vec<u8> {
    let stride = src_size.0 as usize * 4;
    let mut out = Vec::with_capacity(crop.w as usize * crop.h as usize * 4);
    for row in 0..crop.h as usize {
        let y = crop.y as usize + row;
        let start = y * stride + crop.x as usize * 4;
        out.extend_from_slice(&src[start..start + crop.w as usize * 4]);
    }
    out
}

/// RGBA8 → PNG
pub fn encode_png_rgba(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().context("写入 PNG 头失败")?;
        writer.write_image_data(rgba).context("写入 PNG 数据失败")?;
        writer.finish().context("完成 PNG 编码失败")?;
    }
    Ok(out)
}

/// portal 文件是否应该被删除。见 [`geometry::may_delete_portal_file`]。
pub fn should_delete_portal_file(path: &std::path::Path, created_after: SystemTime) -> bool {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("XDG_PICTURES_DIR") {
        dirs.push(PathBuf::from(d));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Pictures"));
    }
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        dirs.push(PathBuf::from(d));
    } else if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cache"));
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(std::env::temp_dir());
    geometry::may_delete_portal_file(path, &dirs, created_after)
}

/// 读取任意 reader 的全部内容（留给将来扩展用，同时方便测试）。
#[allow(dead_code)]
pub fn read_all(mut r: impl Read) -> std::io::Result<Vec<u8>> {
    let mut v = Vec::new();
    r.read_to_end(&mut v)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient_png(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 7, 255]);
            }
        }
        encode_png_rgba(w, h, &px).unwrap()
    }

    #[test]
    fn png_roundtrip_and_crop() {
        let png = gradient_png(64, 48);
        let (w, h, rgba) = decode_png_rgba(&png).unwrap();
        assert_eq!((w, h), (64, 48));
        assert_eq!(rgba.len(), 64 * 48 * 4);

        let crop = Crop {
            x: 8,
            y: 4,
            w: 16,
            h: 10,
        };
        let out = crop_rgba(&rgba, (w, h), crop);
        assert_eq!(out.len(), 16 * 10 * 4);
        // 裁剪后左上角应当是原图 (8,4) 处的像素
        assert_eq!(&out[0..4], &[8, 4, 7, 255]);
        // 右下角是原图 (8+16-1, 4+10-1) = (23, 13)
        let last = out.len() - 4;
        assert_eq!(&out[last..], &[23, 13, 7, 255]);
    }

    #[test]
    fn final_crop_uses_the_locked_full_frame() {
        let (w, h) = (9u32, 7u32);
        let full = FullShot {
            size: (w, h),
            rgba: {
                let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                for y in 0..h {
                    for x in 0..w {
                        rgba.extend_from_slice(&[(x % 256) as u8, (y % 256) as u8, 7, 255]);
                    }
                }
                rgba
            },
            portal_path: Some(PathBuf::from("/tmp/locked-frame.png")),
            created_after: SystemTime::now(),
        };
        let shot = crop_full(
            &full,
            Rect::new(2, 1, 3, 2),
            &[Rect::new(0, 0, w, h)],
            false,
        )
        .unwrap();

        assert_eq!((shot.crop.0, shot.crop.1), (3, 2));
        let (out_w, out_h, rgba) = decode_png_rgba(&shot.png).unwrap();
        assert_eq!((out_w, out_h), (3, 2));
        assert_eq!(&rgba[0..4], &[2, 1, 7, 255]);
        assert_eq!(&rgba[4..8], &[3, 1, 7, 255]);
    }

    #[test]
    fn crop_of_full_image_is_identity() {
        let png = gradient_png(9, 7);
        let (w, h, rgba) = decode_png_rgba(&png).unwrap();
        let out = crop_rgba(&rgba, (w, h), Crop { x: 0, y: 0, w, h });
        assert_eq!(out, rgba);
    }

    #[test]
    fn kwin_premultiplied_bgra_becomes_straight_rgba() {
        let raw = [
            10, 20, 30, 255, // B,G,R,A
            40, 50, 60, 128, // B,G,R,A
        ];
        let rgba = decode_kwin_raw(&raw, 2, 1, 8, 6).unwrap();
        assert_eq!(&rgba[0..4], &[30, 20, 10, 255]);
        // 原始 B,G,R 为 40,50,60，反预乘后输出 R,G,B 为 120,100,80。
        assert_eq!(&rgba[4..8], &[120, 100, 80, 128]);
    }

    #[test]
    fn decodes_rgb_png_too() {
        let mut out = Vec::new();
        {
            let mut e = png::Encoder::new(&mut out, 2, 2);
            e.set_color(png::ColorType::Rgb);
            e.set_depth(png::BitDepth::Eight);
            let mut w = e.write_header().unwrap();
            w.write_image_data(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
                .unwrap();
            w.finish().unwrap();
        }
        let (w, h, rgba) = decode_png_rgba(&out).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(&rgba[0..4], &[1, 2, 3, 255]);
        assert_eq!(&rgba[4..8], &[4, 5, 6, 255]);
    }
}
