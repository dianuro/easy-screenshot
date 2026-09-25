//! Wayland 覆盖层：正常交互优先为每个输出创建一个真正的 xdg-shell fullscreen 窗口，
//! 在没有 xdg-shell 时才回退到 `zwlr_layer_surface_v1`。
//! 启动时先通过 Portal/KWin 锁定完整桌面帧，再用 `wl_shm` 把这份静态画面铺到所有输出，
//! 叠加「框外轻微高斯模糊/冷灰暗化 + 2px 冷蓝边框」并接收鼠标拖拽与键盘。用户确认时只裁剪启动帧，不再抓屏。
//!
//! 关键实现约定（本机实测得出，改动前请先读 [`crate::geometry::plan_crop`] 的注释）：
//! - 逻辑尺寸 = 1536×864，物理 1920×1080，分数缩放 1.25。`wl_output.scale` 会谎报成 2，
//!   所以这里**只使用逻辑坐标**：surface-local 坐标 == buffer 像素坐标 == 逻辑坐标。
//! - buffer 按逻辑尺寸 1:1 分配并 `set_buffer_scale(1)`，代价是被合成器放大 1.25 倍显示，
//!   对「模糊/暗化 + 边框」完全够用（鼠标指针由合成器绘制，不受影响）。

use std::collections::{HashMap, HashSet};
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    data_device_manager::{
        DataDeviceManagerState, WritePipe,
        data_device::{DataDevice, DataDeviceHandler},
        data_offer::{DataOfferHandler, DragOffer},
        data_source::{CopyPasteSource, DataSourceHandler},
    },
    delegate_compositor, delegate_data_device, delegate_keyboard, delegate_layer, delegate_output,
    delegate_pointer, delegate_registry, delegate_seat, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic, generic::NoIoDrop},
        calloop_wayland_source::WaylandSource,
        client::{
            Connection, QueueHandle,
            globals::registry_queue_init,
            protocol::{
                wl_data_device::WlDataDevice, wl_data_device_manager::DndAction,
                wl_data_source::WlDataSource, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm,
                wl_surface,
            },
        },
        protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
            Shape, WpCursorShapeDeviceV1,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Modifiers, RawModifiers},
        pointer::{
            PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        xdg::{
            XdgShell, XdgSurface,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};

use crate::geometry::{Rect, normalize_drag, workspace_union};
use crate::keys;
use crate::portal;

/// 边框宽度（逻辑像素）。
pub const BORDER_WIDTH: u32 = 2;
/// 框外暗化程度。
pub const DIM_ALPHA: u8 = 0x4D;
/// 暗化的冷灰色调（R,G,B），避免给冻结画面叠加黄色调。
const DIM_TINT: [u8; 3] = [10, 14, 20];

/// 边框像素：#4CA6FF，完全不透明（预乘无影响）。
const BORDER_PX: [u8; 4] = [0xFF, 0xA6, 0x4C, 0xFF];

/// 实验性的 transient wl_data_device 路径默认关闭。
///
/// 一次 `send` 只能证明消费方读过数据，不能证明它会接管 selection；当前
/// wl_data_device 没有这种确认机制。保持旧实现仅供协议实验，CLI 不暴露入口。
const ENABLE_TRANSIENT_SELECTION: bool = false;

/// 供 `--smoke` 自检输出的每个输出信息。
#[derive(Debug, Clone)]
pub struct SmokeLine {
    pub name: String,
    pub configure: (u32, u32),
    pub logical: Option<(i32, i32, u32, u32)>,
    pub scale_factor: i32,
}

/// 覆盖层的最终结果。
#[derive(Debug, Clone)]
pub struct OverlayResult {
    /// 全局逻辑坐标下的选区。
    pub rect_global: Rect,
    /// 选区所在输出的逻辑矩形（裁剪比例候选之一）。
    pub output_rect: Rect,
    /// 所有输出逻辑矩形的并集（裁剪比例首选候选）。
    pub workspace: Rect,
    pub output_name: String,
}

pub enum Outcome {
    /// 用户确认，给出选区（以及交互模式下已经完成的截图 / 剪贴板状态）。
    Copy(Box<CopyOutcome>),
    /// 用户按 Ctrl+S：已按内容哈希保存成 PNG，程序可以结束。
    Saved(SavedOutcome),
    /// 用户取消（Esc / 右键 / 表面被关闭）。
    Cancel,
    /// `--smoke`：只验证能建起覆盖层，不做交互。
    Smoke(Vec<SmokeLine>),
}

/// Ctrl+S 保存成功后的结果。
pub struct SavedOutcome {
    pub result: OverlayResult,
    pub path: PathBuf,
}

/// 交互模式下的完整结果。
pub struct CopyOutcome {
    pub result: OverlayResult,
    /// 交互模式下由覆盖层自己完成的截图；`--rect` 等预置模式为 `None`，
    /// 由调用方负责截图。
    pub shot: Option<portal::CroppedShot>,
    pub clipboard: Clipboard,
}

/// 剪贴板发布路径的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Clipboard {
    /// 实验性路径确实完成过一次 wl_data_device 传输。
    ///
    /// 这不保证 selection 在进程退出后仍然存在，因此默认路径不会返回它。
    Focused,
    /// 调用方应使用会 fork 后台持有数据的 `wl-copy` 发布 PNG。
    NeedsFallback(String),
    /// `--no-clipboard`。
    Skipped,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// 只创建覆盖层、打印 configure 尺寸后退出（不抢键盘焦点）。
    pub smoke: bool,
    /// 预置选区（全局逻辑坐标）：完全不创建表面，直接返回结果。用于自动化验证。
    pub preset: Option<Rect>,
    /// 预置选区但**仍然创建覆盖层**：启动帧锁定后直接显示选区。调试用。
    pub preselect: Option<Rect>,
    /// 由覆盖层自己完成 Portal 截图（交互模式）。`false` 时调用方负责截图。
    /// 正常路径的剪贴板发布仍由调用方通过 `wl-copy` 完成。
    pub capture: bool,
    /// 等待 portal 响应的超时。
    pub timeout: Duration,
    /// 保留 Portal 生成的完整桌面文件。
    pub keep_file: bool,
    pub no_clipboard: bool,
    pub verbose: bool,
}

/// 交互模式下选定的选区，等待截图并把发布结果交给调用方。
struct PendingCopy {
    rect_global: Rect,
    output_rect: Rect,
    workspace: Rect,
    output_name: String,
}

type PreparedBackground = (wl_surface::WlSurface, Vec<u8>, Vec<u8>);

enum SurfaceRole {
    Xdg(Window),
    Layer(LayerSurface),
}

impl SurfaceRole {
    fn wl_surface(&self) -> &wl_surface::WlSurface {
        match self {
            Self::Xdg(window) => window.wl_surface(),
            Self::Layer(layer) => layer.wl_surface(),
        }
    }
}

struct SurfaceCtx {
    /// 实际窗口/层表面角色。正常交互优先使用 xdg-shell fullscreen，
    /// 不可用时才回退到 layer-shell。
    _role: SurfaceRole,
    output: wl_output::WlOutput,
    name: String,
    /// 逻辑尺寸（来自 configure）。
    size: (u32, u32),
    pool: Option<SlotPool>,
    pool_size: (u32, u32),
    /// 启动帧按本输出逻辑尺寸缩放后的清晰 RGBA 缓存；锁定前为 `None`。
    background: Option<Vec<u8>>,
    /// 同一画面的轻微高斯模糊缓存，用于选区外。
    blurred_background: Option<Vec<u8>>,
    /// 当前选区（surface-local 逻辑坐标）；松手后保留。
    selection: Option<Rect>,
    /// 拖拽起点；`Some` 表示正在拖拽。
    drag_origin: Option<(f64, f64)>,
    /// 上一次重绘时用来算 damage 的选区。
    last_drawn: Option<Rect>,
    first_frame: bool,
    configured: bool,
}

struct Overlay {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    /// 正常交互优先用它创建真正的 fullscreen xdg 窗口，让 KWin 抑制屏幕热角。
    xdg_shell: Option<XdgShell>,
    data_device_manager: Option<DataDeviceManagerState>,
    data_device: Option<DataDevice>,
    surfaces: HashMap<wl_surface::WlSurface, SurfaceCtx>,
    /// xdg configure 可能先于 wl_output 的逻辑几何到达；这些表面等尺寸信息补齐后再绘制。
    pending_xdg_configures: HashSet<wl_surface::WlSurface>,
    order: Vec<wl_surface::WlSurface>,
    _seat: Option<wl_seat::WlSeat>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_shape: Option<CursorShapeManager>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    modifiers: Modifiers,
    /// 当前拥有选区的表面。
    active: Option<wl_surface::WlSurface>,
    /// 最近一次输入事件的 serial（`set_selection` 需要它）。
    last_serial: Option<u32>,

    // —— 剪贴板 ——
    clipboard_png: Option<Arc<Vec<u8>>>,
    clipboard_source: Option<CopyPasteSource>,
    clip_requested: bool,
    clip_cancelled: bool,
    clip_write_done: Arc<AtomicBool>,

    // —— 交互流程 ——
    /// 启动时锁定的完整桌面帧；用户确认前始终保留。
    frozen: Option<portal::FullShot>,
    /// 所有输出都已配置且输出逻辑信息就绪后，延迟这么久再抓取干净启动帧。
    lock_due: Option<Instant>,
    /// 启动帧已映射到全部输出；此前忽略选择输入。
    locked: bool,
    pending: Option<PendingCopy>,
    shot: Option<portal::CroppedShot>,
    clipboard: Option<Clipboard>,
    serve_deadline: Option<Instant>,
    finished: bool,
    capture_error: Option<anyhow::Error>,

    opts: Options,
    /// 预置选区（`--rect`）时使用；`Some` 表示完全不创建表面。
    preset_rect: Option<Rect>,
    outcome: Option<Outcome>,
    /// 只枚举输出时用的截止时间。
    deadline: Option<Instant>,
}

/// 跑一次覆盖层交互。
pub fn run(opts: Options) -> Result<Outcome> {
    let conn = Connection::connect_to_env()
        .context("连接 Wayland 失败：请确认在 Wayland 会话中运行，且 WAYLAND_DISPLAY 正确")?;
    let (globals, event_queue) =
        registry_queue_init::<Overlay>(&conn).context("枚举 Wayland 全局对象失败")?;

    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh).context("合成器不支持 wl_compositor")?;
    let shm = Shm::bind(&globals, &qh).context("合成器不支持 wl_shm")?;
    let cursor_shape = CursorShapeManager::bind(&globals, &qh).ok();

    // 正常交互优先创建真正的 xdg-shell fullscreen 窗口。KWin 会把它视为
    // fullscreen Window，从而抑制 Activities/屏幕角落热角；没有 xdg-shell
    // 或 --smoke 时才使用 layer-shell。
    let prefer_xdg = !opts.smoke && opts.preset.is_none();
    let xdg_shell = if prefer_xdg {
        XdgShell::bind(&globals, &qh).ok()
    } else {
        None
    };
    if opts.verbose && prefer_xdg {
        if xdg_shell.is_some() {
            eprintln!("[verbose] 使用 xdg-shell fullscreen（KWin 会按全屏窗口屏蔽屏幕热角）");
        } else {
            eprintln!("[verbose] 未找到 xdg-shell，回退到 layer-shell");
        }
    }
    let layer_shell = if opts.preset.is_some() {
        None
    } else if !prefer_xdg || xdg_shell.is_none() {
        let layer_version = globals.contents().with_list(|list| {
            list.iter()
                .find(|g| g.interface == "zwlr_layer_shell_v1")
                .map_or(0, |g| g.version)
        });
        if layer_version == 0 {
            if xdg_shell.is_none() {
                return Err(anyhow!(
                    "合成器既不支持 xdg-shell fullscreen，也不支持 zwlr_layer_shell_v1"
                ));
            }
            None
        } else {
            if layer_version < 4 && !opts.smoke {
                return Err(anyhow!(
                    "合成器的 zwlr_layer_shell_v1 只有 v{layer_version}，缺少 exclusive 键盘交互（需要 v4+）"
                ));
            }
            Some(LayerShell::bind(&globals, &qh).context("绑定 zwlr_layer_shell_v1 失败")?)
        }
    } else {
        None
    };
    // 正常路径不使用 wl_data_device：一次 send 并不代表消费方会接管 selection。
    // 旧实现仅在显式打开实验开关时绑定，供后续研究可靠的 ownership 确认机制。
    let data_device_manager = if ENABLE_TRANSIENT_SELECTION && opts.capture && !opts.no_clipboard {
        match DataDeviceManagerState::bind(&globals, &qh) {
            Ok(m) => Some(m),
            Err(e) => {
                if opts.verbose {
                    eprintln!("[verbose] 合成器不支持 wl_data_device_manager（{e}）");
                }
                None
            }
        }
    } else {
        None
    };
    let mut state = Overlay {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        xdg_shell,
        data_device_manager,
        data_device: None,
        surfaces: HashMap::new(),
        pending_xdg_configures: HashSet::new(),
        order: Vec::new(),
        _seat: None,
        pointer: None,
        keyboard: None,
        cursor_shape,
        cursor_device: None,
        modifiers: Modifiers::default(),
        active: None,
        last_serial: None,
        clipboard_png: None,
        clipboard_source: None,
        clip_requested: false,
        clip_cancelled: false,
        clip_write_done: Arc::new(AtomicBool::new(false)),
        frozen: None,
        lock_due: None,
        locked: false,
        pending: None,
        shot: None,
        clipboard: None,
        serve_deadline: None,
        finished: false,
        capture_error: None,
        preset_rect: opts.preset,
        opts,
        outcome: None,
        deadline: None,
    };

    let mut event_loop: EventLoop<Overlay> = EventLoop::try_new().context("创建事件循环失败")?;
    let handle = event_loop.handle();

    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| anyhow!("把 Wayland 事件源插入事件循环失败: {e:?}"))?;

    // SIGINT（从终端按 Ctrl+C）走和覆盖层里按 Ctrl+C 一样的路径。
    if state.opts.preset.is_none() && !state.opts.smoke {
        match keys::install_sigint_pipe() {
            Ok(fd) => {
                let sig_qh = qh.clone();
                handle
                    .insert_source(
                        Generic::new(fd, Interest::READ, Mode::Level),
                        move |_, fd: &mut NoIoDrop<OwnedFd>, state: &mut Overlay| {
                            if state.opts.verbose {
                                eprintln!("[verbose] 收到 SIGINT（自管道）");
                            }
                            keys::drain(fd);
                            state.request_copy(&sig_qh, "SIGINT");
                            Ok(PostAction::Continue)
                        },
                    )
                    .map_err(|e| anyhow!("注册 SIGINT 事件源失败: {e:?}"))?;
            }
            Err(e) => {
                if state.opts.verbose {
                    eprintln!("[verbose] 安装 SIGINT 处理器失败，将只依赖 Wayland 键盘事件: {e}");
                }
            }
        }
    }

    if state.preset_rect.is_some() {
        // 预置模式：不建表面，只等输出信息就位。
        state.deadline = Some(Instant::now() + Duration::from_secs(3));
    } else {
        create_surfaces(&mut state, &compositor, layer_shell.as_ref(), &qh)?;
    }

    while !state.finished {
        // 有截止时间就定期醒来检查，否则一直阻塞到下一个事件。
        let timeout = if state.deadline.is_some()
            || state.lock_due.is_some()
            || state.serve_deadline.is_some()
        {
            Some(Duration::from_millis(50))
        } else {
            None
        };
        event_loop
            .dispatch(timeout, &mut state)
            .context("Wayland 事件循环出错")?;
        state.after_dispatch(&qh);
    }

    // 用户取消或裁剪失败时，完整帧仍由 overlay 持有；成功裁剪则已经把
    // portal_path 移交给 main，不会在这里删除。
    if !state.opts.keep_file
        && let Some(frozen) = state.frozen.as_ref()
    {
        portal::discard_full_shot(frozen, state.opts.verbose);
    }

    if let Some(e) = state.capture_error.take() {
        return Err(e);
    }

    // 拆掉覆盖层，并在返回前多跑几轮事件循环，让合成器有时间重新合成一帧
    // 不含覆盖层的画面。
    if !state.surfaces.is_empty() {
        state.surfaces.clear();
        state.order.clear();
        let until = Instant::now() + Duration::from_millis(100);
        while Instant::now() < until {
            let _ = event_loop.dispatch(Some(Duration::from_millis(20)), &mut state);
        }
    }

    Ok(state.outcome.take().expect("循环退出时一定有结果"))
}

fn create_surfaces(
    state: &mut Overlay,
    compositor: &CompositorState,
    layer_shell: Option<&LayerShell>,
    qh: &QueueHandle<Overlay>,
) -> Result<()> {
    let outputs: Vec<wl_output::WlOutput> = state.output_state.outputs().collect();
    if outputs.is_empty() {
        return Err(anyhow!("合成器没有报告任何输出（显示器）"));
    }
    for output in outputs {
        let name = state
            .output_state
            .info(&output)
            .and_then(|i| i.name.clone())
            .unwrap_or_else(|| "unknown".into());
        let surface = compositor.create_surface(qh);
        let role = if let Some(xdg_shell) = state.xdg_shell.as_ref() {
            let window = xdg_shell.create_window(surface, WindowDecorations::None, qh);
            window.set_title("easy-screenshot");
            window.set_app_id("easy-screenshot");
            window.set_fullscreen(Some(&output));
            // 首次无 buffer commit 请求 xdg_surface.configure；真正的 buffer
            // 会在 WindowHandler::configure 中挂载。
            window.wl_surface().commit();
            SurfaceRole::Xdg(window)
        } else {
            let layer_shell = layer_shell.ok_or_else(|| {
                anyhow!("没有可用的 xdg-shell 或 zwlr_layer_shell_v1，无法创建覆盖层")
            })?;
            let layer = layer_shell.create_layer_surface(
                qh,
                surface,
                Layer::Overlay,
                Some("easy-screenshot"),
                Some(&output),
            );
            layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            // -1：不预留任何空间，铺满整个输出（连面板一起盖住）。
            layer.set_exclusive_zone(-1);
            layer.set_keyboard_interactivity(if state.opts.smoke {
                KeyboardInteractivity::None
            } else {
                KeyboardInteractivity::Exclusive
            });
            // 0,0 = 交给合成器按锚定决定（四边锚定即整屏）
            layer.set_size(0, 0);
            layer.commit();
            SurfaceRole::Layer(layer)
        };

        let key = role.wl_surface().clone();
        state.surfaces.insert(
            key.clone(),
            SurfaceCtx {
                _role: role,
                output,
                name,
                size: (0, 0),
                pool: None,
                pool_size: (0, 0),
                background: None,
                blurred_background: None,
                selection: None,
                drag_origin: None,
                last_drawn: None,
                first_frame: true,
                configured: false,
            },
        );
        state.order.push(key);
    }
    Ok(())
}

impl Overlay {
    /// 每次 dispatch 之后推进交互流程。
    fn after_dispatch(&mut self, qh: &QueueHandle<Self>) {
        if self.finished {
            return;
        }
        // 输出名字要等 wl_output 事件到齐才有，所以这里惰性回填。
        self.refresh_output_names();
        self.complete_pending_xdg_configures(qh);
        if let Some(preset) = self.preset_rect {
            let deadline_hit = self.deadline.is_some_and(|t| Instant::now() >= t);
            if self.outputs_ready() || deadline_hit {
                self.finish_preset(preset);
            }
            return;
        }
        if self.opts.smoke {
            let all = !self.surfaces.is_empty() && self.surfaces.values().all(|c| c.configured);
            if all {
                let lines = self
                    .order
                    .iter()
                    .filter_map(|k| self.surfaces.get(k))
                    .map(|c| SmokeLine {
                        name: c.name.clone(),
                        configure: c.size,
                        logical: self
                            .output_state
                            .info(&c.output)
                            .and_then(|i| Some((i.logical_position?, i.logical_size?)))
                            .map(|(p, s)| (p.0, p.1, s.0 as u32, s.1 as u32)),
                        scale_factor: self
                            .output_state
                            .info(&c.output)
                            .map_or(1, |i| i.scale_factor),
                    })
                    .collect();
                self.outcome = Some(Outcome::Smoke(lines));
                self.finished = true;
            }
            return;
        }

        // —— 启动阶段：全屏表面先保持透明，等待一帧干净画面后锁定桌面 ——
        if !self.locked {
            let ready = !self.surfaces.is_empty()
                && self.surfaces.values().all(|c| c.configured)
                && self.outputs_ready();
            if self.lock_due.is_none() && ready {
                // 只需要给合成器一点时间提交透明的全屏表面。原实现固定等待
                // 120ms，在 KDE 上会明显拖慢每次启动；配置和首帧通常几十毫秒内
                // 就已到达，50ms 足够避免抓到覆盖层本身。
                self.lock_due = Some(Instant::now() + Duration::from_millis(50));
            }
            if let Some(due) = self.lock_due
                && Instant::now() >= due
            {
                self.lock_due = None;
                self.lock_desktop(qh);
            }
            if !self.locked {
                return;
            }
        }

        if !self.seed_preselect(qh) {
            return;
        }

        // —— 服务阶段：等消费方（Klipper 等）来取数据 ——
        if self.pending.is_some() && self.shot.is_some() {
            let served = self.clip_requested && self.clip_write_done.load(Ordering::SeqCst);
            let deadline_hit = self.serve_deadline.is_some_and(|t| Instant::now() >= t);
            if served || self.clip_cancelled || deadline_hit {
                if !self.clip_requested {
                    // 实验路径设置 selection 后没人取，可能是 serial 过期；
                    // 正常路径不会进入这里，调用方仍统一使用 wl-copy。
                    self.clipboard = Some(Clipboard::NeedsFallback(
                        "设置选区后没有客户端来取数据（serial 可能已过期）".into(),
                    ));
                }
                if self.opts.verbose {
                    eprintln!(
                        "[verbose] 剪贴板服务结束（被取走={}, 被替换={}）",
                        self.clip_requested, self.clip_cancelled
                    );
                }
                self.finish();
            }
        }
    }

    /// xdg-shell 的 configure 可能只带状态而不带尺寸；在输出逻辑几何到达后补完配置。
    fn complete_pending_xdg_configures(&mut self, qh: &QueueHandle<Self>) {
        let keys: Vec<_> = self.pending_xdg_configures.iter().cloned().collect();
        for key in keys {
            let Some(size) = self
                .surfaces
                .get(&key)
                .and_then(|ctx| self.output_state.info(&ctx.output))
                .and_then(|info| info.logical_size)
                .map(|(w, h)| (w as u32, h as u32))
            else {
                continue;
            };
            if size.0 == 0 || size.1 == 0 {
                continue;
            }
            self.pending_xdg_configures.remove(&key);
            {
                let Some(ctx) = self.surfaces.get_mut(&key) else {
                    continue;
                };
                if ctx.size != size {
                    ctx.size = size;
                    ctx.first_frame = true;
                    ctx.pool = None;
                    ctx.background = None;
                    ctx.blurred_background = None;
                }
                ctx.configured = true;
            }
            if self.opts.verbose {
                eprintln!(
                    "[verbose] xdg fullscreen configure {}x{}（输出逻辑尺寸补齐）",
                    size.0, size.1
                );
            }
            self.redraw(qh, &key);
        }
    }

    /// 在覆盖层已经全屏、但表面仍完全透明时抓取一次桌面，并映射到全部输出。
    fn lock_desktop(&mut self, qh: &QueueHandle<Self>) {
        if self.frozen.is_some() || self.locked {
            return;
        }
        if self.opts.verbose {
            eprintln!("[verbose] 正在锁定启动画面…");
        }

        let full = match portal::capture_full(self.opts.timeout, self.opts.verbose) {
            Ok(full) => full,
            Err(e) => {
                self.capture_error = Some(e);
                self.finished = true;
                return;
            }
        };

        let prepared = (|| -> Result<Vec<PreparedBackground>> {
            let Some(workspace) = workspace_union(&self.output_rects()) else {
                bail!("锁定启动画面时找不到任何有效输出");
            };
            let mut prepared = Vec::with_capacity(self.order.len());
            for key in &self.order {
                let Some(ctx) = self.surfaces.get(key) else {
                    continue;
                };
                let info = self
                    .output_state
                    .info(&ctx.output)
                    .ok_or_else(|| anyhow!("输出 {} 没有可用的逻辑几何", ctx.name))?;
                let position = info
                    .logical_position
                    .ok_or_else(|| anyhow!("输出 {} 没有逻辑位置", ctx.name))?;
                let size = info
                    .logical_size
                    .ok_or_else(|| anyhow!("输出 {} 没有逻辑尺寸", ctx.name))?;
                let output_rect = Rect::new(position.0, position.1, size.0 as u32, size.1 as u32);
                let crop =
                    crate::geometry::plan_crop(output_rect, full.size, &[workspace, output_rect])
                        .with_context(|| {
                        format!(
                            "无法把输出 {} 映射到 {}x{} 启动画面",
                            ctx.name, full.size.0, full.size.1
                        )
                    })?;
                let cropped = portal::crop_rgba(&full.rgba, full.size, crop);
                let background = resize_rgba_bilinear(&cropped, (crop.w, crop.h), ctx.size)?;
                let blurred_background = gaussian_blur_rgba(&background, ctx.size.0, ctx.size.1);
                // 生产绘制路径直接使用预先转换好的 ARGB。这样鼠标移动时只需
                // 一次 memcpy + 选区局部复制，而不是每帧对整屏执行颜色转换。
                let background = rgba_to_argb_image(&background);
                let blurred_background = dimmed_rgba_to_argb_image(&blurred_background);
                prepared.push((key.clone(), background, blurred_background));
            }
            Ok(prepared)
        })();

        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(e) => {
                portal::discard_full_shot(&full, self.opts.verbose);
                self.capture_error = Some(e);
                self.finished = true;
                return;
            }
        };

        self.frozen = Some(full);
        self.locked = true;
        for (key, background, blurred_background) in prepared {
            if let Some(ctx) = self.surfaces.get_mut(&key) {
                ctx.background = Some(background);
                ctx.blurred_background = Some(blurred_background);
                ctx.first_frame = true;
            }
            self.redraw(qh, &key);
        }

        if self.opts.verbose {
            eprintln!(
                "[verbose] 启动画面已锁定并映射到 {} 个全屏输出",
                self.surfaces.len()
            );
        }
    }

    /// 结束交互：把结果打包进 `outcome`。
    fn finish(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let clipboard = self
            .clipboard
            .clone()
            .unwrap_or(Clipboard::NeedsFallback("未设置剪贴板".into()));
        self.outcome = Some(Outcome::Copy(Box::new(CopyOutcome {
            result: OverlayResult {
                rect_global: pending.rect_global,
                output_rect: pending.output_rect,
                workspace: pending.workspace,
                output_name: pending.output_name,
            },
            shot: self.shot.take(),
            clipboard,
        })));
        self.finished = true;
    }

    /// 从启动时锁定的完整桌面帧裁剪选区；这里不会再调用 Portal 抓屏。
    fn run_capture(&mut self, qh: &QueueHandle<Self>) {
        let Some(pending) = self.pending.as_ref() else {
            return;
        };
        let (rect, candidates) = (
            pending.rect_global,
            [pending.workspace, pending.output_rect],
        );

        let shot = match self.frozen.as_ref() {
            Some(frozen) => portal::crop_full(frozen, rect, &candidates, self.opts.verbose),
            None => Err(anyhow!("内部错误：确认截图时启动画面已经丢失")),
        };
        let shot = match shot {
            Ok(shot) => shot,
            Err(e) => {
                self.capture_error = Some(e);
                self.finished = true;
                return;
            }
        };

        if self.opts.verbose {
            eprintln!("[verbose] 截图完成 {}x{}", shot.crop.0, shot.crop.1);
        }
        // portal_path 已移交给 shot/main；释放整帧 RGBA，后续只保留裁剪结果。
        self.frozen = None;
        self.shot = Some(shot);

        if self.opts.no_clipboard {
            self.clipboard = Some(Clipboard::Skipped);
            self.finish();
            return;
        }

        // 默认不在本进程里持有一个短命的 wl_data_device selection。调用方改用
        // wl-copy 的 fork 后台进程持有数据，退出后仍可继续粘贴。
        if !ENABLE_TRANSIENT_SELECTION {
            self.clipboard = Some(Clipboard::NeedsFallback(
                "使用 wl-copy 保持剪贴板数据源存活".into(),
            ));
            self.finish();
            return;
        }

        let png = Arc::new(self.shot.as_ref().expect("刚保存了截图").png.clone());
        self.clipboard_png = Some(png);

        match self.set_selection(qh) {
            Ok(()) => {
                self.clipboard = Some(Clipboard::Focused);
                // 最多服务 20 秒；正常情况下消费方毫秒级来取，取完就退出。
                self.serve_deadline = Some(Instant::now() + Duration::from_secs(20));
            }
            Err(reason) => {
                if self.opts.verbose {
                    eprintln!("[verbose] 覆盖层无法自己设置剪贴板：{reason}");
                }
                self.clipboard = Some(Clipboard::NeedsFallback(reason));
                self.finish();
            }
        }
    }

    /// 实验性：用当前有焦点的表面设置一次性 wl_data_device selection。
    ///
    /// 该路径无法确认消费方是否会接管所有权，所以 [`ENABLE_TRANSIENT_SELECTION`]
    /// 默认关闭，CLI 统一走 `wl-copy`。
    fn set_selection(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        let serial = self
            .last_serial
            .ok_or_else(|| "没有可用的输入 serial（覆盖层没拿到键盘焦点？）".to_string())?;

        if self.data_device.is_none() {
            let seat = self
                .seat_state
                .seats()
                .next()
                .ok_or_else(|| "没有 wl_seat".to_string())?;
            let manager = self
                .data_device_manager
                .as_ref()
                .ok_or_else(|| "合成器不支持 wl_data_device_manager".to_string())?;
            self.data_device = Some(manager.get_data_device(qh, &seat));
        }

        let manager = self
            .data_device_manager
            .as_ref()
            .ok_or_else(|| "合成器不支持 wl_data_device_manager".to_string())?;
        let source = manager.create_copy_paste_source(qh, [crate::clipboard::MIME_PNG]);
        let device = self.data_device.as_ref().expect("上面刚创建过");
        source.set_selection(device, serial);
        self.clipboard_source = Some(source);
        if self.opts.verbose {
            eprintln!("[verbose] 已用 wl_data_device 设置选区（serial={serial}）");
        }
        Ok(())
    }

    /// `--preselect`：等所有表面配置好、输出信息就绪后，把一个预置选区画上去，
    /// 剩下的交给用户按 Ctrl+C / Esc。返回 `false` 表示还没准备好。
    fn seed_preselect(&mut self, qh: &QueueHandle<Self>) -> bool {
        let Some(preset) = self.opts.preselect else {
            return true;
        };
        if self.active.is_some() {
            return true;
        }
        if self.surfaces.is_empty()
            || !self.surfaces.values().all(|c| c.configured)
            || !self.outputs_ready()
        {
            return false;
        }

        let (cx, cy) = (
            preset.x + preset.w as i32 / 2,
            preset.y + preset.h as i32 / 2,
        );
        let found = self.order.iter().find_map(|key| {
            let ctx = self.surfaces.get(key)?;
            let info = self.output_state.info(&ctx.output)?;
            let (p, s) = (info.logical_position?, info.logical_size?);
            let hit = cx >= p.0 && cx < p.0 + s.0 && cy >= p.1 && cy < p.1 + s.1;
            hit.then_some((key.clone(), p))
        });

        let Some((key, origin)) = found else {
            return false;
        };
        if let Some(ctx) = self.surfaces.get_mut(&key) {
            ctx.selection = Some(Rect::new(
                preset.x - origin.0,
                preset.y - origin.1,
                preset.w,
                preset.h,
            ));
        }
        self.active = Some(key.clone());
        if self.opts.verbose {
            eprintln!(
                "[verbose] 已预置选区 {}x{}+{}+{}（调试用 --preselect）",
                preset.w, preset.h, preset.x, preset.y
            );
        }
        self.redraw(qh, &key);
        true
    }

    /// 把 `wl_output` 的名字回填到还没有名字的表面上。
    fn refresh_output_names(&mut self) {
        if self.surfaces.values().all(|c| c.name != "unknown") {
            return;
        }
        let updates: Vec<(wl_surface::WlSurface, String)> = self
            .surfaces
            .iter()
            .filter(|(_, c)| c.name == "unknown")
            .filter_map(|(k, c)| {
                self.output_state
                    .info(&c.output)
                    .and_then(|i| i.name.clone())
                    .map(|n| (k.clone(), n))
            })
            .collect();
        for (key, name) in updates {
            if let Some(ctx) = self.surfaces.get_mut(&key) {
                ctx.name = name;
            }
        }
    }

    fn outputs_ready(&self) -> bool {
        let mut any = false;
        for output in self.output_state.outputs() {
            any = true;
            match self.output_state.info(&output) {
                Some(info) if info.logical_position.is_some() && info.logical_size.is_some() => {}
                _ => return false,
            }
        }
        any
    }

    fn output_rects(&self) -> Vec<Rect> {
        self.output_state
            .outputs()
            .filter_map(|o| self.output_state.info(&o))
            .filter_map(|i| {
                let p = i.logical_position?;
                let s = i.logical_size?;
                Some(Rect::new(p.0, p.1, s.0 as u32, s.1 as u32))
            })
            .collect()
    }

    fn finish_preset(&mut self, preset: Rect) {
        let rects = self.output_rects();
        let workspace = workspace_union(&rects).unwrap_or(preset);
        let (cx, cy) = (
            preset.x + preset.w as i32 / 2,
            preset.y + preset.h as i32 / 2,
        );
        let output_rect = rects
            .iter()
            .find(|r| cx >= r.x && cx < r.right() && cy >= r.y && cy < r.bottom())
            .copied()
            .unwrap_or(workspace);
        let output_name = self
            .output_state
            .outputs()
            .filter_map(|o| self.output_state.info(&o))
            .find(|i| {
                let (Some(p), Some(s)) = (i.logical_position, i.logical_size) else {
                    return false;
                };
                p == (output_rect.x, output_rect.y)
                    && s == (output_rect.w as i32, output_rect.h as i32)
            })
            .and_then(|i| i.name)
            .unwrap_or_else(|| "unknown".into());
        self.pending = Some(PendingCopy {
            rect_global: preset,
            output_rect,
            workspace,
            output_name,
        });
        // 预置模式（`--rect`）：不截图、不设剪贴板，交由调用方处理。
        self.clipboard = Some(Clipboard::NeedsFallback(
            "预置选区模式由调用方设置剪贴板".into(),
        ));
        self.finish();
    }

    /// 当前选区的全局逻辑矩形、所在输出矩形、工作区并集与输出名。
    /// 没有有效选区时返回 `None`。
    fn current_selection(&self) -> Option<(Rect, Rect, Rect, String)> {
        let active = self.active.clone()?;
        let (selection, output, name) = {
            let ctx = self.surfaces.get(&active)?;
            (ctx.selection?, ctx.output.clone(), ctx.name.clone())
        };
        let info = self.output_state.info(&output);
        let origin = info
            .as_ref()
            .and_then(|i| i.logical_position)
            .unwrap_or((0, 0));
        if info.as_ref().and_then(|i| i.logical_position).is_none() && self.opts.verbose {
            eprintln!("[verbose] 合成器没有提供输出的逻辑位置，按 (0,0) 处理");
        }
        let output_rect = match info.as_ref().and_then(|i| i.logical_size) {
            Some(s) => Rect::new(origin.0, origin.1, s.0 as u32, s.1 as u32),
            None => {
                let size = self.surfaces.get(&active).map_or((0, 0), |c| c.size);
                Rect::new(origin.0, origin.1, size.0, size.1)
            }
        };
        let rect_global = Rect::new(
            origin.0 + selection.x,
            origin.1 + selection.y,
            selection.w,
            selection.h,
        );
        let workspace = workspace_union(&self.output_rects()).unwrap_or(output_rect);
        Some((rect_global, output_rect, workspace, name))
    }

    /// 处理「复制」请求（Ctrl+C / Enter / SIGINT）。
    fn request_copy(&mut self, qh: &QueueHandle<Self>, why: &str) {
        if self.finished || self.pending.is_some() || !self.locked {
            return;
        }
        let Some((rect_global, output_rect, workspace, name)) = self.current_selection() else {
            if self.opts.verbose {
                eprintln!("[verbose] 收到 {why}，但没有有效选区");
            }
            return;
        };
        if self.opts.verbose {
            eprintln!(
                "[verbose] {why}: 选区 {}x{}+{}+{}（全局逻辑坐标），输出 {name}",
                rect_global.w, rect_global.h, rect_global.x, rect_global.y
            );
        }
        self.pending = Some(PendingCopy {
            rect_global,
            output_rect,
            workspace,
            output_name: name,
        });

        if !self.opts.capture {
            // 没有截图能力（预置模式）：直接交回调用方。
            self.clipboard = Some(Clipboard::NeedsFallback(
                "预置选区模式由调用方设置剪贴板".into(),
            ));
            self.finish();
            return;
        }

        // 直接从启动时锁定的完整帧裁剪；不会隐藏覆盖层，也不会再次访问屏幕。
        self.run_capture(qh);
    }

    /// 处理「保存」请求（Ctrl+S）：把当前选区裁剪成 PNG，按内容哈希命名保存，
    /// 成功后结束程序。失败则保持覆盖层，允许重试或改按 Ctrl+C。
    fn request_save(&mut self, _qh: &QueueHandle<Self>) {
        if self.finished || !self.locked {
            return;
        }
        let Some((rect_global, output_rect, workspace, name)) = self.current_selection() else {
            if self.opts.verbose {
                eprintln!("[verbose] 收到 Ctrl+S，但没有有效选区");
            }
            return;
        };
        let Some(frozen) = self.frozen.as_ref() else {
            if self.opts.verbose {
                eprintln!("[verbose] 收到 Ctrl+S，但启动画面已不可用");
            }
            return;
        };
        let shot = match portal::crop_full(
            frozen,
            rect_global,
            &[workspace, output_rect],
            self.opts.verbose,
        ) {
            Ok(shot) => shot,
            Err(e) => {
                eprintln!("保存截图失败：{e:#}");
                return;
            }
        };
        if self.opts.verbose {
            eprintln!(
                "[verbose] Ctrl+S: 选区 {}x{}+{}+{}（全局逻辑坐标），输出 {name}",
                rect_global.w, rect_global.h, rect_global.x, rect_global.y
            );
        }
        match crate::save::save_png(&shot.png) {
            Ok(path) => {
                self.outcome = Some(Outcome::Saved(SavedOutcome {
                    result: OverlayResult {
                        rect_global,
                        output_rect,
                        workspace,
                        output_name: name,
                    },
                    path,
                }));
                self.finished = true;
            }
            Err(e) => eprintln!("保存截图失败：{e:#}"),
        }
    }

    /// 请求重绘某个表面。
    fn redraw(&mut self, qh: &QueueHandle<Self>, key: &wl_surface::WlSurface) {
        let Some(mut ctx) = self.surfaces.remove(key) else {
            return;
        };
        let result = self.paint_and_commit(qh, key, &mut ctx);
        self.surfaces.insert(key.clone(), ctx);
        if let Err(e) = result
            && self.opts.verbose
        {
            eprintln!("[verbose] 绘制失败: {e}");
        }
    }

    fn paint_and_commit(
        &self,
        _qh: &QueueHandle<Self>,
        key: &wl_surface::WlSurface,
        ctx: &mut SurfaceCtx,
    ) -> Result<()> {
        let (w, h) = ctx.size;
        if w == 0 || h == 0 {
            return Ok(());
        }
        if ctx.pool.is_none() || ctx.pool_size != (w, h) {
            // 三倍容量，保证拖动时最多有三个 buffer 同时在飞。
            let len = (w as usize) * (h as usize) * 4 * 3;
            ctx.pool = Some(SlotPool::new(len, &self.shm).context("创建 shm 池失败")?);
            ctx.pool_size = (w, h);
            ctx.first_frame = true;
        }

        let stride = (w * 4) as i32;
        let (buffer, canvas) = ctx
            .pool
            .as_mut()
            .expect("池刚创建过")
            .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            .context("申请 shm buffer 失败")?;

        let len = canvas.len().min((w as usize) * (h as usize) * 4);
        if let Some(background) = ctx.background.as_deref() {
            let dimmed = ctx.blurred_background.as_deref().unwrap_or(background);
            paint_frozen_argb(canvas, background, dimmed, w, h, ctx.selection);
        } else {
            // 启动帧锁定前保持全透明。表面仍然映射并持有键盘焦点，
            // 但 Portal 看到的是底下未被覆盖的桌面。
            canvas[..len].fill(0);
        }

        if ctx.first_frame {
            key.damage_buffer(0, 0, w as i32, h as i32);
            ctx.first_frame = false;
        } else if let Some(d) = damage_rect(ctx.last_drawn, ctx.selection, (w, h)) {
            key.damage_buffer(d.x, d.y, d.w as i32, d.h as i32);
        }
        ctx.last_drawn = ctx.selection;

        buffer.attach_to(key).context("挂载 buffer 失败")?;
        key.set_buffer_scale(1);
        key.commit();
        Ok(())
    }
}

/// 把启动帧缩放成目标 RGBA 图，用于把物理像素 portal 图像映射到逻辑尺寸 surface。
fn resize_rgba_bilinear(src: &[u8], src_size: (u32, u32), dst_size: (u32, u32)) -> Result<Vec<u8>> {
    let (src_w, src_h) = src_size;
    let (dst_w, dst_h) = dst_size;
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        bail!("冻结画面的源尺寸或目标尺寸为 0");
    }
    let src_len = (src_w as usize)
        .checked_mul(src_h as usize)
        .and_then(|n| n.checked_mul(4))
        .context("冻结画面源尺寸溢出")?;
    if src.len() < src_len {
        bail!("冻结画面源数据不完整");
    }
    let dst_len = (dst_w as usize)
        .checked_mul(dst_h as usize)
        .and_then(|n| n.checked_mul(4))
        .context("冻结画面目标尺寸溢出")?;
    let mut dst = Vec::with_capacity(dst_len);

    for y in 0..dst_h {
        let fy =
            (((y as f64 + 0.5) * src_h as f64 / dst_h as f64) - 0.5).clamp(0.0, (src_h - 1) as f64);
        let y0 = (fy.floor() as u32).min(src_h - 1);
        let y1 = y0.saturating_add(1).min(src_h - 1);
        let wy = fy - y0 as f64;

        for x in 0..dst_w {
            let fx = (((x as f64 + 0.5) * src_w as f64 / dst_w as f64) - 0.5)
                .clamp(0.0, (src_w - 1) as f64);
            let x0 = (fx.floor() as u32).min(src_w - 1);
            let x1 = x0.saturating_add(1).min(src_w - 1);
            let wx = fx - x0 as f64;

            let p00 = ((y0 * src_w + x0) * 4) as usize;
            let p10 = ((y0 * src_w + x1) * 4) as usize;
            let p01 = ((y1 * src_w + x0) * 4) as usize;
            let p11 = ((y1 * src_w + x1) * 4) as usize;
            for channel in 0..4 {
                let top = src[p00 + channel] as f64 * (1.0 - wx) + src[p10 + channel] as f64 * wx;
                let bottom =
                    src[p01 + channel] as f64 * (1.0 - wx) + src[p11 + channel] as f64 * wx;
                dst.push((top * (1.0 - wy) + bottom * wy).round() as u8);
            }
        }
    }
    Ok(dst)
}

/// 3×3 binomial Gaussian blur（轻微、边缘钳制），只用于选择页面的预览层。
fn gaussian_blur_rgba(src: &[u8], w: u32, h: u32) -> Vec<u8> {
    let len = (w as usize) * (h as usize) * 4;
    if src.len() < len || w == 0 || h == 0 {
        return src.to_vec();
    }
    let mut horizontal = vec![0u8; len];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let left = &src[((y * w as usize) + x.saturating_sub(1)) * 4..][..4];
            let center = &src[((y * w as usize) + x) * 4..][..4];
            let right = &src[((y * w as usize) + (x + 1).min(w as usize - 1)) * 4..][..4];
            let out = ((y * w as usize) + x) * 4;
            for channel in 0..4 {
                horizontal[out + channel] = ((left[channel] as u16
                    + 2 * center[channel] as u16
                    + right[channel] as u16
                    + 2)
                    / 4) as u8;
            }
        }
    }

    let mut blurred = vec![0u8; len];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let top = y.saturating_sub(1);
            let bottom = (y + 1).min(h as usize - 1);
            let out = ((y * w as usize) + x) * 4;
            for channel in 0..4 {
                blurred[out + channel] = ((horizontal[(top * w as usize + x) * 4 + channel] as u16
                    + 2 * horizontal[(y * w as usize + x) * 4 + channel] as u16
                    + horizontal[(bottom * w as usize + x) * 4 + channel] as u16
                    + 2)
                    / 4) as u8;
            }
        }
    }
    blurred
}

/// 把整张 RGBA 图转换成 wl_shm 使用的 ARGB8888 内存布局。
fn rgba_to_argb_image(src: &[u8]) -> Vec<u8> {
    let mut dst = Vec::with_capacity(src.len());
    for pixel in src.chunks_exact(4) {
        dst.extend_from_slice(&rgba_to_argb([pixel[0], pixel[1], pixel[2], pixel[3]]));
    }
    dst
}

/// 生成可直接复制到 ARGB canvas 的暗化图像。
fn dimmed_rgba_to_argb_image(src: &[u8]) -> Vec<u8> {
    let mut dst = Vec::with_capacity(src.len());
    for pixel in src.chunks_exact(4) {
        dst.extend_from_slice(&rgba_to_argb(dim_pixel(pixel)));
    }
    dst
}

/// 把预先转换好的 ARGB 图像画成覆盖层。
///
/// 选区外的暗化图和选区内的清晰图都已提前完成颜色转换。鼠标移动时这里
/// 只执行整图 memcpy 和选区局部 memcpy，避免每帧重复进行逐像素算术。
fn paint_frozen_argb(
    canvas: &mut [u8],
    background: &[u8],
    dimmed_background: &[u8],
    w: u32,
    h: u32,
    selection: Option<Rect>,
) {
    let px = (w as usize) * (h as usize);
    let len = px * 4;
    if canvas.len() < len || background.len() < len || dimmed_background.len() < len {
        return;
    }

    canvas[..len].copy_from_slice(&dimmed_background[..len]);

    let Some(sel) = selection else { return };
    let (ix0, iy0, ix1, iy1) = clipped_selection(sel, w, h);
    if ix1 <= ix0 || iy1 <= iy0 {
        return;
    }
    let row_bytes = (ix1 - ix0) as usize * 4;
    for y in iy0..iy1 {
        let offset = (y as usize * w as usize + ix0 as usize) * 4;
        canvas[offset..offset + row_bytes].copy_from_slice(&background[offset..offset + row_bytes]);
    }
    paint_border(canvas, w, h, sel);
}

/// 把冻结的 RGBA 桌面画成完全不透明的 ARGB8888 覆盖层。
///
/// 选区外使用预计算的轻微高斯模糊 + 冷灰暗化，选区内保留清晰冻结像素；
/// 最终裁剪仍从 Portal/KWin 的原始帧完成，不使用这里的视觉处理结果。
/// 生产路径使用上面的 [`paint_frozen_argb`]，此函数保留给测试和纯 RGBA 调用方。
#[cfg(test)]
pub fn paint_frozen(
    canvas: &mut [u8],
    background: &[u8],
    blurred_background: &[u8],
    w: u32,
    h: u32,
    selection: Option<Rect>,
) {
    let px = (w as usize) * (h as usize);
    if canvas.len() < px * 4 || background.len() < px * 4 || blurred_background.len() < px * 4 {
        return;
    }

    for (dst, src) in canvas[..px * 4]
        .chunks_exact_mut(4)
        .zip(blurred_background[..px * 4].chunks_exact(4))
    {
        dst.copy_from_slice(&rgba_to_argb(dim_pixel(src)));
    }

    let Some(sel) = selection else { return };
    let (ix0, iy0, ix1, iy1) = clipped_selection(sel, w, h);
    if ix1 <= ix0 || iy1 <= iy0 {
        return;
    }

    for y in iy0..iy1 {
        let row = ((y as usize) * (w as usize)) * 4;
        let s = row + (ix0 as usize) * 4;
        let e = row + (ix1 as usize) * 4;
        for (dst, src) in canvas[s..e]
            .chunks_exact_mut(4)
            .zip(background[s..e].chunks_exact(4))
        {
            dst.copy_from_slice(&rgba_to_argb(opaque_pixel(src)));
        }
    }

    paint_border(canvas, w, h, sel);
}

fn dim_pixel(src: &[u8]) -> [u8; 4] {
    let alpha = src[3] as u16;
    let mut out = [0; 4];
    for channel in 0..3 {
        let base = src[channel] as u16 * alpha / 255;
        out[channel] = ((base * (255 - DIM_ALPHA) as u16
            + DIM_TINT[channel] as u16 * DIM_ALPHA as u16)
            / 255) as u8;
    }
    out[3] = 0xFF;
    out
}

#[cfg(test)]
fn opaque_pixel(src: &[u8]) -> [u8; 4] {
    let alpha = src[3] as u16;
    let mut out = [0; 4];
    for channel in 0..3 {
        out[channel] = (src[channel] as u16 * alpha / 255) as u8;
    }
    out[3] = 0xFF;
    out
}

/// RGBA8（逻辑颜色顺序）转为小端 ARGB8888 的内存字节顺序 B,G,R,A。
fn rgba_to_argb(pixel: [u8; 4]) -> [u8; 4] {
    let alpha = pixel[3] as u16;
    let premultiply = |channel: u8| ((channel as u16 * alpha + 127) / 255) as u8;
    [
        premultiply(pixel[2]),
        premultiply(pixel[1]),
        premultiply(pixel[0]),
        pixel[3],
    ]
}

fn clipped_selection(sel: Rect, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let x0 = sel.x.max(0) as u32;
    let y0 = sel.y.max(0) as u32;
    let x1 = (sel.right().max(0) as u32).min(w);
    let y1 = (sel.bottom().max(0) as u32).min(h);
    (x0, y0, x1, y1)
}

fn paint_border(canvas: &mut [u8], w: u32, h: u32, sel: Rect) {
    let (ix0, iy0, ix1, iy1) = clipped_selection(sel, w, h);
    if ix1 <= ix0 || iy1 <= iy0 {
        return;
    }
    let b = BORDER_WIDTH as i32;
    let ox0 = (sel.x - b).max(0) as u32;
    let oy0 = (sel.y - b).max(0) as u32;
    let ox1 = (sel.right() + b).max(0).min(w as i32) as u32;
    let oy1 = (sel.bottom() + b).max(0).min(h as i32) as u32;
    for y in oy0..oy1 {
        if y < iy0 || y >= iy1 {
            fill_row(canvas, w, y, ox0, ox1, &BORDER_PX);
        } else {
            if ox0 < ix0 {
                fill_row(canvas, w, y, ox0, ix0, &BORDER_PX);
            }
            if ix1 < ox1 {
                fill_row(canvas, w, y, ix1, ox1, &BORDER_PX);
            }
        }
    }
}

fn fill_row(canvas: &mut [u8], w: u32, y: u32, x0: u32, x1: u32, color: &[u8; 4]) {
    if x1 <= x0 {
        return;
    }
    let s = ((y as usize) * (w as usize) + x0 as usize) * 4;
    let e = ((y as usize) * (w as usize) + x1 as usize) * 4;
    for chunk in canvas[s..e].chunks_exact_mut(4) {
        chunk.copy_from_slice(color);
    }
}

/// 新旧选区之间实际发生变化的包围盒（含边框外扩），用于精确 damage。
///
/// 选区从 `old` 变成 `new` 时，只有「外框(old) ∪ 外框(new)」里的像素可能变。
/// 两者都为 `None` 说明什么也没画，不需要 damage。
pub fn damage_rect(old: Option<Rect>, new: Option<Rect>, size: (u32, u32)) -> Option<Rect> {
    let outer = |r: Rect| r.expand(BORDER_WIDTH);
    let acc = match (old, new) {
        (None, None) => return None,
        (Some(a), Some(b)) => outer(a).union(&outer(b)),
        (Some(a), None) => outer(a),
        (None, Some(b)) => outer(b),
    };
    let (w, h) = (size.0 as i32, size.1 as i32);
    let x0 = acc.x.clamp(0, w);
    let y0 = acc.y.clamp(0, h);
    let x1 = acc.right().clamp(0, w);
    let y1 = acc.bottom().clamp(0, h);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32))
}

impl Overlay {
    fn handle_pointer(&mut self, qh: &QueueHandle<Self>, events: &[PointerEvent]) {
        for event in events {
            let key = event.surface.clone();
            if !self.surfaces.contains_key(&key) {
                continue;
            }
            // 启动帧锁定前允许合成器把光标切成十字，但不允许开始/修改选区。
            if !self.locked && !matches!(&event.kind, PointerEventKind::Enter { .. }) {
                continue;
            }
            let pos = event.position;
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.last_serial = Some(serial);
                    if self.opts.verbose {
                        eprintln!("[verbose] 指针进入覆盖层 @{pos:?}（{key:?}）");
                    }
                    if let Some(device) = self.cursor_device.as_ref() {
                        device.set_shape(serial, Shape::Crosshair);
                    }
                }
                PointerEventKind::Leave { .. } => {}
                PointerEventKind::Motion { .. } => {
                    let origin = self.surfaces.get(&key).and_then(|c| c.drag_origin);
                    if let (Some(origin), Some(ctx)) = (origin, self.surfaces.get_mut(&key)) {
                        let size = ctx.size;
                        ctx.selection = normalize_drag(origin, pos, size);
                        self.active = Some(key.clone());
                        self.redraw(qh, &key);
                    }
                }
                PointerEventKind::Press { button, serial, .. } => {
                    self.last_serial = Some(serial);
                    match button {
                        // BTN_LEFT
                        0x110 => {
                            if let Some(ctx) = self.surfaces.get_mut(&key) {
                                ctx.drag_origin = Some(pos);
                                ctx.selection = None;
                            }
                            self.active = Some(key.clone());
                            self.redraw(qh, &key);
                        }
                        // BTN_RIGHT：取消
                        0x111 => {
                            self.outcome = Some(Outcome::Cancel);
                            self.finished = true;
                        }
                        _ => {}
                    }
                }
                PointerEventKind::Release { button, serial, .. } => {
                    self.last_serial = Some(serial);
                    if button == 0x110 {
                        if let Some(ctx) = self.surfaces.get_mut(&key) {
                            ctx.drag_origin = None;
                            // 松手后 selection 保持不变 —— 框线留在屏幕上。
                            if ctx.selection.is_none() {
                                self.active = None;
                            }
                        }
                        self.redraw(qh, &key);
                    }
                }
                PointerEventKind::Axis { .. } => {}
            }
        }
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
        // 故意忽略：我们固定按逻辑坐标 1:1 绘制，见文件头说明。
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        if self.outcome.is_none() {
            self.outcome = Some(Outcome::Cancel);
        }
        self.finished = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let key = layer.wl_surface().clone();
        {
            let Some(ctx) = self.surfaces.get_mut(&key) else {
                return;
            };
            let (w, h) = configure.new_size;
            if w == 0 || h == 0 {
                return;
            }
            if ctx.size != (w, h) {
                ctx.size = (w, h);
                ctx.first_frame = true;
                ctx.pool = None;
                ctx.background = None;
                ctx.blurred_background = None;
                if self.opts.verbose {
                    eprintln!("[verbose] layer surface configure {}x{}", w, h);
                }
            }
            ctx.configured = true;
        }
        self.redraw(qh, &key);
    }
}

impl WindowHandler for Overlay {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        if self.outcome.is_none() {
            self.outcome = Some(Outcome::Cancel);
        }
        self.finished = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        serial: u32,
    ) {
        let key = window.wl_surface().clone();
        let fallback_size = self
            .surfaces
            .get(&key)
            .and_then(|ctx| self.output_state.info(&ctx.output))
            .and_then(|info| info.logical_size)
            .map(|(w, h)| (w as u32, h as u32));
        let (w, h) = match configure.new_size {
            (Some(w), Some(h)) => (w.get(), h.get()),
            _ => fallback_size.unwrap_or((0, 0)),
        };
        window.xdg_surface().ack_configure(serial);
        if w == 0 || h == 0 {
            self.pending_xdg_configures.insert(key);
            return;
        }
        self.pending_xdg_configures.remove(&key);
        {
            let Some(ctx) = self.surfaces.get_mut(&key) else {
                return;
            };
            if ctx.size != (w, h) {
                ctx.size = (w, h);
                ctx.first_frame = true;
                ctx.pool = None;
                ctx.background = None;
                ctx.blurred_background = None;
                if self.opts.verbose {
                    eprintln!("[verbose] xdg fullscreen configure {}x{}", w, h);
                }
            }
            ctx.configured = true;
        }
        self.redraw(qh, &key);
    }
}

impl SeatHandler for Overlay {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self._seat = Some(seat);
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            if self.opts.verbose {
                eprintln!(
                    "[verbose] 键盘能力已就绪（覆盖层将拿到 exclusive 键盘焦点）: {}",
                    self.keyboard.is_some()
                );
            }
        }
        if capability == Capability::Pointer
            && self.pointer.is_none()
            && let Ok(pointer) = self.seat_state.get_pointer(qh, &seat)
        {
            let device = self
                .cursor_shape
                .as_ref()
                .map(|manager| manager.get_shape_device(&pointer, qh));
            self.cursor_device = device;
            self.pointer = Some(pointer);
            if self.opts.verbose {
                eprintln!(
                    "[verbose] 指针能力已就绪（十字光标: {}）",
                    self.cursor_device.is_some()
                );
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard
            && let Some(k) = self.keyboard.take()
        {
            k.release();
        }
        if capability == Capability::Pointer
            && let Some(p) = self.pointer.take()
        {
            p.release();
            self.cursor_device = None;
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Overlay {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[smithay_client_toolkit::seat::keyboard::Keysym],
    ) {
        self.last_serial = Some(serial);
        // 合成器把键盘焦点交给我们了 —— 这条日志是「Ctrl+C 能收到」的直接证据。
        if self.opts.verbose && self.surfaces.contains_key(surface) {
            eprintln!("[verbose] 键盘焦点已进入覆盖层");
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
    ) {
        self.last_serial = Some(serial);
        if self.opts.verbose && self.surfaces.contains_key(surface) {
            eprintln!("[verbose] 键盘焦点离开覆盖层");
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.last_serial = Some(serial);
        if !self.locked {
            return;
        }
        match keys::action_for(event.keysym.raw(), &self.modifiers) {
            Some(keys::Action::Copy) => self.request_copy(qh, "Ctrl+C/Enter"),
            Some(keys::Action::Save) => self.request_save(qh),
            Some(keys::Action::Cancel) if !self.finished => {
                self.outcome = Some(Outcome::Cancel);
                self.finished = true;
            }
            _ => {}
        }
    }

    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl PointerHandler for Overlay {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        self.handle_pointer(qh, events);
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

/// 我们把 PNG 作为剪贴板数据源提供给合成器，有客户端来取时写到它给的 fd 里。
///
/// 写数据必须放到线程里：PNG 可能有几 MB，直接写会阻塞事件循环。
impl DataSourceHandler for Overlay {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        mime: Option<String>,
    ) {
        if self.opts.verbose {
            eprintln!("[verbose] 剪贴板消费方接受类型: {mime:?}");
        }
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        self.clip_requested = true;
        if self.opts.verbose {
            eprintln!("[verbose] 剪贴板数据被请求（{mime}）");
        }
        let Some(png) = self.clipboard_png.clone() else {
            return; // fd 随 fd 变量析构而关闭
        };
        let done = self.clip_write_done.clone();
        std::thread::spawn(move || {
            use std::io::Write;
            // SAFETY: fd 由 Wayland 通过 SCM_RIGHTS 传来，本进程拥有它。
            let owned = unsafe { OwnedFd::from_raw_fd(fd.into_raw_fd()) };
            let mut file = std::fs::File::from(owned);
            if let Err(e) = file.write_all(&png).and_then(|()| file.flush()) {
                eprintln!("easy-screenshot: 写剪贴板数据失败: {e}");
            }
            done.store(true, Ordering::SeqCst);
            // file 析构 → 关闭 fd → 读取方收到 EOF
        });
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {
        // 选区被别人替换（例如 Klipper 取走数据后自己接管）——正常结束。
        self.clip_cancelled = true;
        if self.opts.verbose {
            eprintln!("[verbose] 剪贴板选区被替换");
        }
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

/// 我们用不到拖放，也不会去读别人的选区，所以这些回调全是空实现。
impl DataDeviceHandler for Overlay {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &wl_surface::WlSurface,
    ) {
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataOfferHandler for Overlay {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

delegate_compositor!(Overlay);
delegate_output!(Overlay);
delegate_shm!(Overlay);
delegate_seat!(Overlay);
delegate_xdg_shell!(Overlay);
delegate_xdg_window!(Overlay);
delegate_keyboard!(Overlay);
delegate_pointer!(Overlay);
delegate_layer!(Overlay);
delegate_data_device!(Overlay);
delegate_registry!(Overlay);

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(canvas: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * w + x) * 4) as usize;
        [canvas[o], canvas[o + 1], canvas[o + 2], canvas[o + 3]]
    }

    fn dimmed_pixel(rgb: [u8; 3]) -> [u8; 4] {
        rgba_to_argb(dim_pixel(&[rgb[0], rgb[1], rgb[2], 255]))
    }

    fn argb_pixel(rgb: [u8; 3]) -> [u8; 4] {
        rgba_to_argb([rgb[0], rgb[1], rgb[2], 255])
    }

    #[test]
    fn rgba_to_argb_uses_bgra_memory_order() {
        assert_eq!(rgba_to_argb([10, 20, 30, 255]), [30, 20, 10, 255]);
    }

    #[test]
    fn paint_frozen_without_selection_dims_all() {
        let (w, h) = (8u32, 6u32);
        let background = [10u8, 20, 30, 255].repeat((w * h) as usize);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        paint_frozen(&mut canvas, &background, &background, w, h, None);
        for y in 0..h {
            for x in 0..w {
                assert_eq!(
                    px(&canvas, w, x, y),
                    dimmed_pixel([10, 20, 30]),
                    "at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn paint_frozen_keeps_selection_original_and_draws_border() {
        let (w, h) = (20u32, 20u32);
        let background = [10u8, 20, 30, 255].repeat((w * h) as usize);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        let sel = Rect::new(6, 6, 8, 8);
        paint_frozen(&mut canvas, &background, &background, w, h, Some(sel));

        assert_eq!(px(&canvas, w, 6, 6), argb_pixel([10, 20, 30]));
        assert_eq!(px(&canvas, w, 13, 13), argb_pixel([10, 20, 30]));
        assert_eq!(px(&canvas, w, 4, 5), BORDER_PX);
        assert_eq!(px(&canvas, w, 14, 6), BORDER_PX);
        assert_eq!(px(&canvas, w, 6, 15), BORDER_PX);
        assert_eq!(px(&canvas, w, 3, 5), dimmed_pixel([10, 20, 30]));
        assert_eq!(px(&canvas, w, 6, 16), dimmed_pixel([10, 20, 30]));
    }

    #[test]
    fn paint_frozen_clips_selection_at_edges() {
        let (w, h) = (10u32, 10u32);
        let background = [10u8, 20, 30, 255].repeat((w * h) as usize);
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        paint_frozen(
            &mut canvas,
            &background,
            &background,
            w,
            h,
            Some(Rect::new(-4, -4, 8, 8)),
        );
        assert_eq!(px(&canvas, w, 0, 0), argb_pixel([10, 20, 30]));
        assert_eq!(px(&canvas, w, 3, 3), argb_pixel([10, 20, 30]));
        assert_eq!(px(&canvas, w, 5, 5), BORDER_PX);
        assert_eq!(px(&canvas, w, 4, 0), BORDER_PX);
        assert_eq!(px(&canvas, w, 7, 7), dimmed_pixel([10, 20, 30]));
    }

    #[test]
    fn bilinear_resize_preserves_uniform_rgba() {
        let src = [10u8, 20, 30, 255].repeat(4);
        let dst = resize_rgba_bilinear(&src, (2, 2), (4, 4)).unwrap();
        assert_eq!(dst, [10, 20, 30, 255].repeat(16));
    }

    #[test]
    fn gaussian_blur_softens_a_bright_center() {
        let (w, h) = (3u32, 3u32);
        let mut src = vec![0u8; (w * h * 4) as usize];
        let center = (w as usize + 1) * 4;
        src[center..center + 4].copy_from_slice(&[255, 255, 255, 255]);
        let blurred = gaussian_blur_rgba(&src, w, h);
        let center = (w as usize + 1) * 4;
        let neighbor = (w as usize) * 4;
        assert!(blurred[center] < 255);
        assert!(blurred[neighbor] > 0);
    }

    #[test]
    fn damage_covers_old_and_new_selection() {
        let old = Some(Rect::new(10, 10, 10, 10));
        let new = Some(Rect::new(50, 0, 10, 10));
        let d = damage_rect(old, new, (100, 100)).unwrap();
        // 必须同时覆盖 (8,8)-(22,22) 和 (48,-2)-(62,12)
        assert!(d.x <= 8 && d.y <= 0);
        assert!(d.right() >= 62 && d.bottom() >= 22);
    }

    #[test]
    fn damage_is_none_when_nothing_changes() {
        assert_eq!(damage_rect(None, None, (10, 10)), None);
    }

    #[test]
    fn damage_first_selection_is_only_that_box() {
        let d = damage_rect(None, Some(Rect::new(20, 20, 10, 10)), (100, 100)).unwrap();
        assert_eq!(d, Rect::new(18, 18, 14, 14));
    }
}
