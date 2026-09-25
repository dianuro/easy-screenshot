# easy-screenshot

极简的 Wayland 截图工具：**全屏锁定启动画面 → 拖框 → Ctrl+C → 剪贴板 → 退出**。

```
运行 easy-screenshot
  ↓  每个显示器都被全屏覆盖；KWin 直连（授权时）或 Portal 在启动时锁定一次完整桌面
  ↓  冻结画面铺满所有输出、框外轻微高斯模糊并冷灰暗化，鼠标变十字
按住左键拖出一个框（可在任意输出上选择）
  ↓
松开左键（此时还没裁剪，框线留在冻结画面上，可以反复调整）
  ↓
Ctrl+C / Enter  → 从启动帧裁剪框内区域并放进系统剪贴板，程序退出（退出码 0）
Esc / 右键      → 放弃，退出码 1，剪贴板不动
```

“锁定”指：选区期间桌面可以继续变化，但覆盖层显示、用户看到的选区内容和最终保存的像素都来自启动时的同一张完整桌面帧；确认时不会再次抓屏。

## 为什么是这样实现的

本机是 **KDE Plasma 6 / KWin 6.7.5 / Wayland**，一开始把几条看起来很自然的路都试了一遍，结论如下，都是实测：

| 方案 | 结果 |
|---|---|
| `grim`（wlr-screencopy） | ❌ `compositor doesn't support the screen capture protocol` |
| X11 `XGetImage`（XWayland root） | ❌ `BadMatch`（XWayland rootless 下 root 窗口没有桌面内容） |
| `org.kde.KWin.ScreenShot2.CaptureWorkspace` | ❌ 未授权时返回 `NoAuthorized`。KWin 有「截图沙箱」：调用方必须有一个带绝对路径 `Exec=` 和 `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2` 的 `.desktop` 文件才会被放行 |
| `org.freedesktop.portal.Screenshot` | ✅ 实测可用，无需任何安装/授权步骤 |

所以本工具在 KDE 上优先走 **KWin `ScreenShot2` 直连**，未授权、非 KDE 或直连不可用时回退到 **XDG Desktop Portal**：程序先建立铺满所有输出的真正 xdg-shell fullscreen 窗口（没有 xdg-shell 时才回退到透明 layer-shell），在任何选区出现前通过抓屏后端锁定一次整屏；随后把这份 RGBA 帧缩放到各输出作为静态背景，确认选区后只从内存中的原始帧裁剪。这样既避免选区期间画面继续变化，也顺便能在其它实现了 Portal 的合成器（GNOME、wlroots 等）上工作。KDE 上使用真正的 fullscreen xdg 窗口还会让 KWin 按全屏软件处理覆盖层，从而抑制 Activities/屏幕角落热角。

### 坐标与缩放（改动前必读）

本机是**分数缩放**：

| | 值 |
|---|---|
| 物理模式 | 1920×1080 |
| 逻辑几何 | **1536×864** |
| 真实缩放 | **1.25** |
| `wl_output.scale` 报的值 | **2**（⚠️ 分数缩放下这是向上取整的回退值，**不能用来算坐标**） |
| KWin/Portal 返回的原始图像 | 1920×1080（物理像素） |

因此：

- 覆盖层交互只使用**逻辑坐标**：xdg-shell（回退时为 layer-shell）的 configure 尺寸就是逻辑尺寸，surface-local 坐标 == buffer 像素坐标，绘制数学是恒等的；启动帧的物理像素 RGBA 会用双线性缩放到这个逻辑尺寸，选区外再做轻微高斯模糊与冷灰暗化，选区内保持清晰。它只是用户看到的预览/遮罩，不会改变最终文件像素。
- 最终文件不会使用缩放后的预览像素，而是始终从启动时抓到的原始物理像素帧裁剪。裁剪比例**运行时从实际图像算出来**，不硬编码：在「逻辑工作区（所有输出并集）」和「选区所在输出」两个候选里，挑宽高比与图像一致的那个（误差 < 1%）。全屏框选 ⇒ 1920×1080；400×300 的框 ⇒ 500×375。见 `src/geometry.rs` 的 `plan_crop` 及其单测。

## 构建

```bash
cargo build --release
# 产物：target/release/easy-screenshot
```

依赖：Rust 1.85+（edition 2024）。运行期需要：

- Wayland 会话 + xdg-shell（交互路径优先使用 fullscreen；不可用时回退到 `zwlr_layer_shell_v1` v4+）；
- KDE 上可选 KWin `ScreenShot2` 直连；非 KDE 或回退路径需要 `xdg-desktop-portal` 及其截图后端（KDE 后端为 `xdg-desktop-portal-kde`）；
- 复制到剪贴板需要 `wl-clipboard`（`wl-copy`）。Arch：`pacman -S wl-clipboard`。

### KDE 静默抓屏（避免 Portal 图标闪烁）

KDE 的 `xdg-desktop-portal-kde` 在收到 `interactive=false` 后，**仍会短暂创建并显示自己的 `ScreenshotDialog`**。这就是鼠标旁和任务栏出现“系统门户”图标的原因；不是图片保存或 `wl-copy` 导致的。

本程序在 KDE Wayland 上会优先尝试 KWin 的原生 `ScreenShot2` 直连接口。该接口不创建 Portal 窗口，但需要给当前二进制写入一次 KDE 授权条目：

```bash
# 用你实际要运行的同一个二进制执行
./target/release/easy-screenshot --install-kwin-permission
# 或：cargo run -- --install-kwin-permission
```

命令只写入当前用户的 `~/.local/share/applications/easy-screenshot.desktop`，授权给当前二进制无确认调用 KWin 截屏；它不会修改系统文件。之后正常启动即可静默抓屏。若移动了二进制，需要重新执行该命令。

普通 KDE 交互运行时，如果当前二进制还没有匹配的授权条目，程序会**明确询问**：

```text
检测到 KDE 静默抓屏尚未授权。
是否允许写入用户级授权条目：~/.local/share/applications/easy-screenshot.desktop
这会允许当前二进制无确认调用 KWin ScreenShot2；拒绝则使用 Portal 后备。
继续吗？[y/N]
```

输入 `y`/`yes` 才会写入；直接回车、`n` 或非交互式终端都不会授予权限，并会继续使用 Portal 后备路径。交互终端中选择拒绝后，会记录当前二进制路径的选择，后续运行不会重复询问；二进制路径改变后会再次询问。若想重新查看提示，可删除 `~/.local/state/easy-screenshot/kwin-permission-declined`，或直接执行 `--install-kwin-permission`。设置 `EASY_SCREENSHOT_FORCE_PORTAL=1` 会跳过提示并强制使用 Portal。普通运行不会静默修改授权配置。

未安装授权条目、运行在非 KDE 环境或直连接口不可用时，程序会安全回退到 XDG Portal；此时 KDE 的短暂 Portal 图标仍可能出现。调试时可用 `EASY_SCREENSHOT_FORCE_PORTAL=1` 强制走 Portal。

KDE 的 Activities/桌面总览属于 KWin 的屏幕热角，客户端无法通过 layer-shell 的鼠标事件取消。交互路径现在使用真正的 xdg-shell fullscreen 窗口，KWin 会将其按全屏软件处理，因此在正常 xdg-shell 路径中不会再因鼠标推到角落触发热角；如果合成器不支持 xdg-shell、只能使用 layer-shell 回退路径，热角是否可屏蔽取决于该合成器，KDE 上应优先确认日志出现 `xdg fullscreen configure`。

正常 KDE 交互运行加上 `--verbose` 时，应能看到：

```text
使用 xdg-shell fullscreen（KWin 会按全屏窗口屏蔽屏幕热角）
xdg fullscreen configure 1536x864
```

如果看到“回退到 layer-shell”，说明没有使用实际 xdg fullscreen 路径；KDE 上此时不能保证热角被屏蔽。

## 用法

```bash
easy-screenshot                      # 交互截图
easy-screenshot --help
```

| 选项 | 说明 |
|---|---|
| `--save <文件>` | 额外把裁剪结果写成 PNG |
| `--no-clipboard` | 不复制到剪贴板（需配合 `--save`） |
| `--keep-file` | 保留 Portal 后端落在 `~/Pictures` 的整屏 PNG（KWin 直连没有落盘文件；默认会删掉 Portal 文件） |
| `--install-kwin-permission` | 显式为当前二进制安装 KDE KWin 静默抓屏授权条目（普通运行缺少授权时会先询问） |
| `--timeout <秒>` | 等待抓屏后端响应的超时，默认 60 |
| `--verbose` | 打印覆盖层角色、选区、缩放比、抓屏耗时等调试信息 |

调试/自动化用的选项：

| 选项 | 说明 |
|---|---|
| `--smoke` | 只验证能否建立覆盖层，打印 configure 尺寸后退出（不抢键盘焦点） |
| `--rect X,Y,W,H` | 跳过交互，直接对给定逻辑矩形截图 |
| `--preselect X,Y,W,H` | 全屏锁定启动画面后预置选区（调试用） |

退出码：`0` 成功，`1` 用户取消，`2` 出错。

## 关于剪贴板的一个坑

Wayland 的剪贴板是**由客户端持有**的：谁 `set_selection`，谁就得一直在线把数据喂给来粘贴的程序。消费方完成一次读取并不会自动接管 selection；如果本进程随即退出，下一次粘贴就可能已经没有数据。

项目曾尝试直接用 `wl_data_device.set_selection` 提供 PNG：第一次读取会成功，但程序退出后第二次粘贴为空。这个“一次性传输成功”不能证明 Klipper 已接管并持久保存。

因此正常路径现在统一调用 `wl-copy`。它设置完选区会 fork 出一个常驻后台进程持有数据，主进程可以安全退出；实测间隔一秒连续读取两次，PNG 字节完全一致。注意**不要**用 `wait_with_output()` 等它：那个后台进程会继承 stderr 管道，读到 EOF 会永远阻塞（这个 bug 已经踩过，见 `src/clipboard.rs` 的注释）。

如果不装 `wl-clipboard`，可以用 `--save 文件 --no-clipboard` 只保存。

### KDE 用户必读：Klipper 默认不保存图片

Klipper（plasmashell 里的剪贴板管理器）的默认配置是这样的（见 Plasma 的 `klipper.kcfg`）：

```xml
<entry name="SaveImages" type="Bool"><default>false</default></entry>          <!-- 不把图片存进历史 -->
<entry name="PreventEmptyClipboard" type="Bool"><default>true</default></entry> <!-- 持有进程退出后不让剪贴板变空 -->
```

两条加起来的效果是：

- **文本**复制会被 Klipper 存进历史并接管，所以 `echo aaa | wl-copy` 之后文本能长期存在；
- **图片**既不入历史、也不被接管，只活在 `wl-copy` 的常驻进程里；一旦那个进程退出，Klipper 会把剪贴板恢复成上一份内容 —— 于是你粘贴到的是旧内容或空内容，Klipper 历史里也永远看不到截图。

这是 KDE 的默认行为，不是本工具的 bug（Spectacle 的「复制到剪贴板」在默认配置下同样不留历史）。想让截图变得持久、并且出现在剪贴板历史里：

> **系统设置 → 剪贴板 → 勾选「保存图片」**

勾选后 Klipper 会自己持有图片，即使本工具和它的后台进程都退出了，你依然能粘贴，也能在历史里找到。

程序在检测到没开这个选项时会自动打印一行提示，不会静默。

**还有一个陷阱**：不要在受限沙箱里运行（例如 AI 助手的命令沙箱）。那种环境会在命令结束时清掉整个进程组，`wl-copy` 的常驻进程会一起被杀，剪贴板随后被 Klipper 还原。请在自己的 KDE 终端（Konsole 等）里运行。

## 关于 `~/Pictures` 里的整屏文件

portal 的 `Screenshot` 会把整屏图落到 `~/Pictures/Screenshot_<时间戳>.png`。本工具读完就把它删掉（有严格的守卫：只删本次调用返回的那个路径，且必须位于 Pictures/缓存/临时目录、修改时间在本次运行期间内）。想留着就用 `--keep-file`。

如果你在受限沙箱里运行（比如某些 AI 沙箱，工作区之外只读），删除会失败并打印一行提示——这不是程序的 bug，正常终端里跑就会删掉。

## 开发

```bash
cargo test          # 32 个单测：几何/缩放规则、冻结背景/高斯模糊绘制、ARGB 字节序、KWin 原始帧、裁剪、damage、PNG 往返、按键语义
cargo run -- --verbose
```

模块划分：

| 文件 | 职责 |
|---|---|
| `src/main.rs` | CLI、流程编排、退出码 |
| `src/overlay.rs` | Wayland xdg-shell fullscreen / layer-shell 回退覆盖层、shm 绘制、指针/键盘状态机 |
| `src/geometry.rs` | 纯几何/路径工具（含裁剪缩放规则），全部可单测 |
| `src/portal.rs` | KWin `ScreenShot2` / XDG Portal D-Bus 调用 + PNG 解码/裁剪/编码 |
| `src/clipboard.rs` | `wl-copy` 调用 |
| `src/keys.rs` | 按键语义 + SIGINT 自管道 |

### 已知限制

- 多显示器且各屏缩放不同时，启动帧的全局物理像素映射与裁剪比例会失配，此时退化为「按选区所在输出」的比例，裁剪可能有一两个像素的偏差。
- 覆盖层始终铺满每个输出；鼠标移动到哪块屏幕，就可以在那块屏幕上拖框。选区一次属于一个输出，不支持跨两个输出合并成一个大矩形。
- 只支持 Wayland。X11 原生会话不在范围内（本机 XWayland 抓图实测不可用）。
- portal 的落盘文件名精确到秒，如果极度巧合地同时运行两个实例，可能互相覆盖同一秒的文件（单实例使用无影响）。
- 不提供全局热键、托盘、开机自启、注释/OCR——就是要简单。
