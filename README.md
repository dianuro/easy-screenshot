# easy-screenshot

极简 Wayland 截图工具：冻结当前桌面，拖动鼠标框选，复制到剪贴板并退出。

## 功能

- 启动时锁定完整桌面，选区期间画面不会变化
- 冻结画面铺满所有显示器
- 框外模糊、暗化，框内保持清晰
- 支持 Ctrl+C / Enter 复制，Ctrl+S 保存，Esc / 右键取消
- 支持 KWin 原生抓屏；不可用时自动回退到 XDG Desktop Portal
- KDE 首次运行自动准备 KWin 静默抓屏授权，无需额外命令
- 支持分数缩放和多显示器

## 构建

需要 Rust 1.85 或更高版本：

```bash
cargo build --release
```

二进制文件：

```text
target/release/easy-screenshot
```

运行测试：

```bash
cargo test
```

## 运行

```bash
./target/release/easy-screenshot
```

使用流程：

1. 等待冻结画面出现
2. 按住鼠标左键拖动选择区域
3. 松开后可继续调整选区
4. 按 `Ctrl+C` 或 `Enter` 复制到剪贴板
5. 按 `Ctrl+S` 把当前选区保存为 PNG（文件名 = 图片哈希）并退出
6. 按 `Esc` 或鼠标右键取消

截图来源是程序启动时捕获的完整桌面帧，确认时不会再次抓屏。

### Ctrl+S 保存目录

默认保存到 `$XDG_PICTURES_DIR`（通常是 `~/Pictures`），文件名是图片内容的 SHA-256 哈希，格式 PNG。

可在配置文件里自定义目录：

```text
# ~/.config/easy-screenshot/config（或 $XDG_CONFIG_HOME/easy-screenshot/config）
save_dir = /path/to/pictures
```

`#` 开头是注释，值可以用引号包裹，`~/` 会展开成 HOME。

## 常用选项

| 选项 | 说明 |
|---|---|
| `--save <文件>` | 同时保存为 PNG |
| `--no-clipboard` | 不复制到剪贴板，必须和 `--save` 一起使用 |
| `--keep-file` | 保留 Portal 生成的整屏 PNG |
| `--verbose` | 输出抓屏、缩放和选区调试信息 |
| `--timeout <秒>` | 等待抓屏后端的超时时间，默认 60 秒 |
| `--help` | 显示帮助 |

## KDE 静默抓屏

KDE 下首次运行会自动创建 KWin 原生抓屏所需的桌面授权条目，之后启动更快，也不会短暂显示 Portal 截图窗口。可显式执行以下命令预先生成或更新授权条目：

```bash
./target/release/easy-screenshot --install-kwin-permission
```

然后正常运行：

```bash
./target/release/easy-screenshot
```

授权文件写入当前用户的：

```text
~/.local/share/applications/easy-screenshot.desktop
```

如果移动或重新编译了二进制文件，下次启动会自动更新授权条目。自动授权失败时程序会回退到 XDG Portal。

## 依赖

- Wayland 会话
- `xdg-shell`，不可用时需要 `zwlr_layer_shell_v1` v4+
- `wl-clipboard`，用于复制图片
- 非 KWin 环境或 KWin 授权不可用时，需要 `xdg-desktop-portal` 及截图后端

Arch Linux 安装剪贴板工具：

```bash
sudo pacman -S wl-clipboard
```

如果不想安装 `wl-clipboard`，可以只保存文件：

```bash
easy-screenshot --save screenshot.png --no-clipboard
```

## 常见问题

### 启动时仍然较慢

程序会在 KDE 首次抓屏前自动准备授权。如果自动授权失败或不在 KDE 环境，会回退到 Portal，抓屏速度会明显慢一些。

可用 `--verbose` 查看具体耗时：

```bash
easy-screenshot --verbose
```

### 剪贴板粘贴为空

Wayland 剪贴板由客户端持有，程序默认使用 `wl-copy` 保持数据。如果使用 KDE 的 Klipper，建议在：

```text
系统设置 → 剪贴板 → 保存图片
```

打开“保存图片”。

### 多显示器

每个显示器都会被覆盖。选区必须位于同一个显示器内，不支持跨显示器合并选择。

## 开发

```bash
cargo fmt -- --check
cargo test
cargo build --release
```

主要模块：

| 文件 | 作用 |
|---|---|
| `src/main.rs` | 命令行参数和整体流程 |
| `src/overlay.rs` | Wayland 覆盖层、绘制和交互 |
| `src/geometry.rs` | 坐标、缩放和裁剪计算 |
| `src/portal.rs` | KWin / Portal 抓屏和 PNG 处理 |
| `src/clipboard.rs` | `wl-copy` 调用 |
| `src/keys.rs` | 键盘和信号处理 |
| `src/config.rs` | 配置文件（保存目录）解析 |
| `src/save.rs` | 按哈希命名保存 PNG |

## 退出码

- `0`：成功
- `1`：用户取消
- `2`：错误
