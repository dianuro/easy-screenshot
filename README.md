# easy-screenshot

极简 Wayland 截图工具：冻结当前桌面，拖动鼠标框选，复制到剪贴板并退出。

## 功能

- 启动时锁定完整桌面，选区期间画面不会变化
- 冻结画面铺满所有显示器
- 框外模糊、暗化，框内保持清晰
- 支持 Ctrl+C / Enter 复制，Esc / 右键取消
- 支持 KWin 原生抓屏；不可用时自动回退到 XDG Desktop Portal
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
5. 按 `Esc` 或鼠标右键取消

截图来源是程序启动时捕获的完整桌面帧，确认时不会再次抓屏。

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

KDE 下建议先授权 KWin 原生抓屏接口，启动更快，也不会短暂显示 Portal 截图窗口：

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

如果移动或重新编译了二进制文件，需要重新执行授权命令。未授权时程序会自动回退到 XDG Portal。

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

先确认已执行 `--install-kwin-permission`。没有授权时，程序必须通过 Portal 抓屏，速度会明显慢一些。

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

## 退出码

- `0`：成功
- `1`：用户取消
- `2`：错误
