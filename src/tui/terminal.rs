//! Terminal input, console termination, and restoration for the TUI.
//!
//! Native polling detects a disconnected Unix terminal before crossterm reads
//! it. Native Windows console callbacks coordinate shutdown with terminal
//! restoration. This module owns those platform details; the caller retains
//! the lifetime and drop order of its input monitor, signal, and screen guard.

#[cfg(unix)]
use std::fs::File;
use std::io::{self, IsTerminal, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use crossterm::cursor::Show;
use crossterm::event;
#[cfg(windows)]
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use super::BACKGROUND_CHANNEL_POLL;

pub(super) const MAX_CLIPBOARD_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TerminalEventWait {
    Ready,
    TimedOut,
    #[cfg_attr(not(unix), allow(dead_code))]
    Disconnected,
}

#[cfg(unix)]
enum TerminalInputFd {
    Stdin,
    Tty(File),
}

#[cfg(unix)]
pub(super) struct TerminalInputMonitor {
    fd: TerminalInputFd,
}

#[cfg(unix)]
impl TerminalInputMonitor {
    pub(super) fn open() -> io::Result<Self> {
        if !io::stdout().is_terminal() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "interactive TUI output is not attached to a terminal",
            ));
        }
        // Crossterm uses stdin when it is a TTY and otherwise opens /dev/tty.
        // Mirror that choice so POLLHUP/POLLERR can be observed before calling
        // crossterm's reader. Crossterm 0.28 loops on a zero-byte TTY read, so
        // entering it after a PTY hangup can otherwise consume a full CPU core.
        let fd = if io::stdin().is_terminal() {
            TerminalInputFd::Stdin
        } else {
            TerminalInputFd::Tty(File::options().read(true).write(true).open("/dev/tty")?)
        };
        Ok(Self { fd })
    }

    fn raw_fd(&self) -> RawFd {
        match &self.fd {
            TerminalInputFd::Stdin => libc::STDIN_FILENO,
            TerminalInputFd::Tty(file) => file.as_raw_fd(),
        }
    }

    fn wait(&self, timeout: Duration) -> io::Result<TerminalEventWait> {
        poll_terminal_fd(self.raw_fd(), timeout)
    }
}

#[cfg(not(unix))]
pub(super) struct TerminalInputMonitor;

#[cfg(not(unix))]
impl TerminalInputMonitor {
    pub(super) fn open() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "interactive TUI input/output is not attached to a terminal",
            ))
        } else {
            Ok(Self)
        }
    }
}

#[cfg(unix)]
pub(super) fn poll_terminal_fd(fd: RawFd, timeout: Duration) -> io::Result<TerminalEventWait> {
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: descriptor is one initialized, writable pollfd and remains alive
    // for this synchronous call; nfds is exactly one. poll validates the fd.
    let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if result < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            Ok(TerminalEventWait::TimedOut)
        } else {
            Err(error)
        };
    }
    if descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
        return Ok(TerminalEventWait::Disconnected);
    }
    if descriptor.revents & libc::POLLIN != 0 {
        Ok(TerminalEventWait::Ready)
    } else {
        Ok(TerminalEventWait::TimedOut)
    }
}

pub(super) fn wait_for_terminal_event(
    monitor: &TerminalInputMonitor,
    timeout: Duration,
) -> io::Result<TerminalEventWait> {
    #[cfg(unix)]
    {
        if monitor.wait(timeout)? == TerminalEventWait::Disconnected {
            return Ok(TerminalEventWait::Disconnected);
        }
        // Check the terminal again immediately before entering crossterm. Use
        // a zero timeout there: the blocking wait above owns pacing, while the
        // second hangup check closes the race that created detached busy loops.
        if monitor.wait(Duration::ZERO)? == TerminalEventWait::Disconnected {
            return Ok(TerminalEventWait::Disconnected);
        }
        event::poll(Duration::ZERO).map(|ready| {
            if ready {
                TerminalEventWait::Ready
            } else {
                TerminalEventWait::TimedOut
            }
        })
    }

    #[cfg(not(unix))]
    {
        let _ = monitor;
        event::poll(timeout).map(|ready| {
            if ready {
                TerminalEventWait::Ready
            } else {
                TerminalEventWait::TimedOut
            }
        })
    }
}

pub(super) fn write_osc52_clipboard<W: Write>(writer: &mut W, text: &str) -> io::Result<()> {
    if text.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "clipboard text exceeds the {} KiB limit",
                MAX_CLIPBOARD_TEXT_BYTES / 1024
            ),
        ));
    }
    let payload = BASE64_STANDARD.encode(text.as_bytes());
    writer.write_all(b"\x1b]52;c;")?;
    writer.write_all(payload.as_bytes())?;
    writer.write_all(b"\x07")?;
    writer.flush()
}

pub(super) struct TerminationSignal {
    #[cfg(unix)]
    requested: Arc<AtomicBool>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
    #[cfg(windows)]
    installed: bool,
}

impl TerminationSignal {
    pub(super) fn install() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};

            let requested = Arc::new(AtomicBool::new(false));
            let mut registrations = Vec::with_capacity(3);
            for signal in [SIGHUP, SIGINT, SIGTERM] {
                match signal_hook::flag::register(signal, Arc::clone(&requested)) {
                    Ok(registration) => registrations.push(registration),
                    Err(error) => {
                        for registration in registrations {
                            signal_hook::low_level::unregister(registration);
                        }
                        return Err(error);
                    }
                }
            }
            Ok(Self {
                requested,
                registrations,
            })
        }

        #[cfg(windows)]
        {
            WINDOWS_TERMINATION_REQUESTED.store(false, Ordering::SeqCst);
            WINDOWS_TERMINAL_RESTORED.store(false, Ordering::SeqCst);
            // SAFETY: the callback has the ABI and process lifetime required
            // by the console API and performs only atomic coordination.
            let installed = unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                    Some(windows_console_ctrl_handler),
                    1,
                )
            };
            if installed == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self { installed: true })
            }
        }
    }

    #[cfg(test)]
    pub(super) fn for_test() -> Self {
        #[cfg(unix)]
        {
            Self {
                requested: Arc::new(AtomicBool::new(false)),
                registrations: Vec::new(),
            }
        }
        #[cfg(windows)]
        {
            WINDOWS_TERMINATION_REQUESTED.store(false, Ordering::SeqCst);
            WINDOWS_TERMINAL_RESTORED.store(false, Ordering::SeqCst);
            Self { installed: false }
        }
    }

    #[cfg(test)]
    pub(super) fn request_for_test(&self) {
        #[cfg(unix)]
        self.requested.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        WINDOWS_TERMINATION_REQUESTED.store(true, Ordering::SeqCst);
    }

    pub(super) fn is_requested(&self) -> bool {
        #[cfg(unix)]
        {
            self.requested.load(Ordering::SeqCst)
        }
        #[cfg(windows)]
        {
            WINDOWS_TERMINATION_REQUESTED.load(Ordering::SeqCst)
        }
    }

    pub(super) fn poll_timeout(&self, timeout: Duration) -> Duration {
        timeout.min(BACKGROUND_CHANNEL_POLL)
    }

    pub(super) fn mark_terminal_restored(&self) {
        #[cfg(windows)]
        mark_windows_terminal_restored();
    }
}

#[cfg(unix)]
impl Drop for TerminationSignal {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}

#[cfg(windows)]
pub(super) static WINDOWS_TERMINATION_REQUESTED: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static WINDOWS_TERMINAL_RESTORED: AtomicBool = AtomicBool::new(true);

#[cfg(windows)]
fn mark_windows_terminal_restored() {
    WINDOWS_TERMINAL_RESTORED.store(true, Ordering::SeqCst);
    // SAFETY: the address is a process-lifetime static and is used only as
    // the matching one-byte WaitOnAddress key in the console callback.
    unsafe {
        windows_sys::Win32::System::Threading::WakeByAddressAll(
            WINDOWS_TERMINAL_RESTORED.as_ptr().cast(),
        );
    }
}

#[cfg(windows)]
fn wait_for_windows_terminal_restore() {
    while !WINDOWS_TERMINAL_RESTORED.load(Ordering::SeqCst) {
        let pending = false;
        // Close/logoff/shutdown handlers return on a system-owned thread.
        // Keep that thread alive until the main loop has dropped
        // TerminalGuard; otherwise Windows may terminate the process as soon
        // as this callback returns. This wait performs no terminal or file IO.
        // SAFETY: both pointers refer to valid one-byte bool storage for the
        // duration of the wait. Spurious wakes are handled by the loop.
        unsafe {
            windows_sys::Win32::System::Threading::WaitOnAddress(
                WINDOWS_TERMINAL_RESTORED.as_ptr().cast(),
                (&raw const pending).cast(),
                std::mem::size_of::<bool>(),
                u32::MAX,
            );
        }
    }
}

#[cfg(windows)]
pub(super) extern "system" fn windows_console_ctrl_handler(control: u32) -> i32 {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    match control {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            WINDOWS_TERMINATION_REQUESTED.store(true, Ordering::SeqCst);
            1
        }
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => {
            WINDOWS_TERMINATION_REQUESTED.store(true, Ordering::SeqCst);
            wait_for_windows_terminal_restore();
            1
        }
        _ => 0,
    }
}

#[cfg(windows)]
impl Drop for TerminationSignal {
    fn drop(&mut self) {
        mark_windows_terminal_restored();
        if !self.installed {
            return;
        }
        // SAFETY: this removes the exact static callback installed above.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                Some(windows_console_ctrl_handler),
                0,
            );
        }
    }
}

pub(super) struct TerminalGuard;

#[cfg(not(windows))]
const BUTTON_MOUSE_CAPTURE_ENABLE: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1015h\x1b[?1006h";
#[cfg(not(windows))]
const BUTTON_MOUSE_CAPTURE_DISABLE: &[u8] = b"\x1b[?1006l\x1b[?1015l\x1b[?1002l\x1b[?1000l";

pub(super) fn enable_button_mouse_capture<W: Write>(writer: &mut W) -> io::Result<()> {
    #[cfg(windows)]
    execute!(writer, EnableMouseCapture)?;
    #[cfg(not(windows))]
    {
        writer.write_all(BUTTON_MOUSE_CAPTURE_ENABLE)?;
        writer.flush()?;
    }
    Ok(())
}

pub(super) fn disable_button_mouse_capture<W: Write>(writer: &mut W) -> io::Result<()> {
    #[cfg(windows)]
    execute!(writer, DisableMouseCapture)?;
    #[cfg(not(windows))]
    {
        writer.write_all(BUTTON_MOUSE_CAPTURE_DISABLE)?;
        writer.flush()?;
    }
    Ok(())
}

impl TerminalGuard {
    pub(super) fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        let entered = execute!(stdout, EnterAlternateScreen)
            .and_then(|()| enable_button_mouse_capture(&mut stdout));
        if let Err(error) = entered {
            let _ = disable_button_mouse_capture(&mut stdout);
            let _ = execute!(stdout, LeaveAlternateScreen);
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = disable_button_mouse_capture(&mut stdout);
        let _ = execute!(stdout, LeaveAlternateScreen);
        // Ratatui can hide the cursor during drawing. Keep cursor restoration
        // in the guard so panic/unwind paths are as safe as normal returns.
        let _ = execute!(stdout, Show);
        let _ = disable_raw_mode();
    }
}
