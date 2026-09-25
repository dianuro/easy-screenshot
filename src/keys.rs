//! 键盘动作判定 + `Ctrl+C`(SIGINT) 自管道。
//!
//! 覆盖层通过 xdg-shell fullscreen 窗口（回退时通过 layer-shell 的 exclusive
//! keyboard interactivity）拿到按键；但如果合成器没有把按键路由过来（比如焦点还在
//! 终端上），用户按 Ctrl+C 会被终端翻译成 SIGINT。这里把 SIGINT 也接进同一条路径，
//! 两种情况下行为一致。

use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};

use smithay_client_toolkit::seat::keyboard::Modifiers;

/// Linux evdev/xkb keysym 原始值（见 `xkeysym`）。用原始值比较，避免猜常量名。
const KEYSYM_C_LOWER: u32 = 0x0063; // XK_c
const KEYSYM_C_UPPER: u32 = 0x0043; // XK_C
const KEYSYM_S_LOWER: u32 = 0x0073; // XK_s
const KEYSYM_S_UPPER: u32 = 0x0053; // XK_S
const KEYSYM_ESCAPE: u32 = 0xff1b; // XK_Escape
const KEYSYM_RETURN: u32 = 0xff0d; // XK_Return
const KEYSYM_KP_ENTER: u32 = 0xff8d; // XK_KP_Enter

/// 用户按下的语义动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 把当前选区复制到剪贴板并退出。
    Copy,
    /// 把当前选区按内容哈希保存为 PNG，并退出。
    Save,
    /// 放弃本次截图。
    Cancel,
}

/// 把一次按键翻译成动作。`keysym_raw` 是 `Keysym::raw()`。
pub fn action_for(keysym_raw: u32, modifiers: &Modifiers) -> Option<Action> {
    match keysym_raw {
        KEYSYM_ESCAPE => Some(Action::Cancel),
        // Ctrl+C（大小写都认，Ctrl+Shift+C 也顺带支持）
        KEYSYM_C_LOWER | KEYSYM_C_UPPER if modifiers.ctrl => Some(Action::Copy),
        // Ctrl+S（大小写都认）：保存到目录并退出
        KEYSYM_S_LOWER | KEYSYM_S_UPPER if modifiers.ctrl => Some(Action::Save),
        // Enter 作为 Ctrl+C 的等价键：某些环境里 Ctrl+C 更容易被别的程序抢走
        KEYSYM_RETURN | KEYSYM_KP_ENTER => Some(Action::Copy),
        _ => None,
    }
}

static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn handle_sigint(_sig: libc::c_int) {
    // 只做 async-signal-safe 的事情：往管道写一个字节，让事件循环醒过来。
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = b'c';
        // SAFETY: fd 是一个有效的、在进程生命周期内保持打开的写端。
        unsafe {
            libc::write(fd, std::ptr::addr_of!(byte).cast(), 1);
        }
    }
}

/// 安装 SIGINT 处理器，返回需要被事件循环监听的管道读端。
///
/// 处理器本身只写一个字节；读端由 calloop 的 `Generic` 源监听，读到字节后执行
/// 与覆盖层里按 Ctrl+C 完全相同的动作。
pub fn install_sigint_pipe() -> io::Result<OwnedFd> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: fds 是长度为 2 的有效数组。
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    SIGNAL_WRITE_FD.store(write_fd, Ordering::SeqCst);

    // SAFETY: sigaction 结构体按 libc 的定义初始化，handler 是合法的 extern "C" 函数。
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handle_sigint as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }

    // 写端由 handler 直接使用，不让 Rust 在进程结束时关闭它；读端交给调用方。
    std::mem::forget(unsafe { OwnedFd::from_raw_fd(write_fd) });
    Ok(unsafe { OwnedFd::from_raw_fd(read_fd) })
}

/// 把管道里积压的字节读干净，避免水平触发模式下反复唤醒。
pub fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    loop {
        // SAFETY: fd 是有效的读端；buf 是有效缓冲区。
        let n = unsafe {
            libc::read(
                std::os::fd::AsRawFd::as_raw_fd(fd),
                buf.as_mut_ptr().cast(),
                buf.len(),
            )
        };
        if n <= 0 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mods(ctrl: bool) -> Modifiers {
        Modifiers {
            ctrl,
            ..Default::default()
        }
    }

    #[test]
    fn ctrl_c_copies() {
        assert_eq!(action_for(KEYSYM_C_LOWER, &mods(true)), Some(Action::Copy));
        assert_eq!(action_for(KEYSYM_C_UPPER, &mods(true)), Some(Action::Copy));
    }

    #[test]
    fn bare_c_does_nothing() {
        assert_eq!(action_for(KEYSYM_C_LOWER, &mods(false)), None);
    }

    #[test]
    fn ctrl_s_saves() {
        assert_eq!(action_for(KEYSYM_S_LOWER, &mods(true)), Some(Action::Save));
        assert_eq!(action_for(KEYSYM_S_UPPER, &mods(true)), Some(Action::Save));
        assert_eq!(action_for(KEYSYM_S_LOWER, &mods(false)), None);
    }

    #[test]
    fn escape_cancels_enter_copies() {
        assert_eq!(
            action_for(KEYSYM_ESCAPE, &mods(false)),
            Some(Action::Cancel)
        );
        assert_eq!(action_for(KEYSYM_ESCAPE, &mods(true)), Some(Action::Cancel));
        assert_eq!(action_for(KEYSYM_RETURN, &mods(false)), Some(Action::Copy));
        assert_eq!(
            action_for(KEYSYM_KP_ENTER, &mods(false)),
            Some(Action::Copy)
        );
    }
}
