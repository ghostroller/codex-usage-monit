#![cfg(unix)]

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};

const START_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    parser: vt100::Parser,
    _temp: tempfile::TempDir,
}

impl PtySession {
    fn spawn() -> Self {
        let system = NativePtySystem::default();
        let pair = system.openpty(START_SIZE).unwrap();
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_codex-usage-monit"));
        let fixture_home = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("codex-home")
            .join("normal");
        let temp = tempfile::tempdir().unwrap();
        command.args([
            "--codex-home",
            fixture_home.to_str().unwrap(),
            "--days",
            "3650",
            "--offline",
            "--no-rollout-cache",
        ]);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env_remove("NO_COLOR");
        command.env("CODEX_USAGE_MONIT_STATE_DIR", temp.path().join("state"));
        command.env("CODEX_USAGE_MONIT_CONFIG_DIR", temp.path().join("config"));
        command.env("CODEX_USAGE_MONIT_CACHE_DIR", temp.path().join("cache"));
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if sender.send(buffer[..read].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            master: pair.master,
            child,
            writer,
            output,
            parser: vt100::Parser::new(START_SIZE.rows, START_SIZE.cols, 0),
            _temp: temp,
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    fn click(&mut self, column: u16, row: u16) {
        let column = column + 1;
        let row = row + 1;
        self.send(format!("\u{1b}[<0;{column};{row}M\u{1b}[<0;{column};{row}m").as_bytes());
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        while let Ok(bytes) = self.output.try_recv() {
            self.parser.process(&bytes);
        }
        let size = PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        };
        self.parser.set_size(rows, columns);
        self.master.resize(size).unwrap();
    }

    fn wait_for_new_output(
        &mut self,
        description: &str,
        predicate: impl Fn(&vt100::Screen) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let now = Instant::now();
            if now >= deadline {
                panic!(
                    "timed out waiting for {description}\nterminal:\n{}",
                    self.parser.screen().contents()
                );
            }
            match self
                .output
                .recv_timeout((deadline - now).min(Duration::from_millis(100)))
            {
                Ok(bytes) => {
                    self.parser.process(&bytes);
                    if predicate(self.parser.screen()) {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "PTY closed while waiting for {description}\nterminal:\n{}",
                        self.parser.screen().contents()
                    );
                }
            }
        }
    }

    fn wait_for(&mut self, description: &str, predicate: impl Fn(&vt100::Screen) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if predicate(self.parser.screen()) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                panic!(
                    "timed out waiting for {description}\nterminal:\n{}",
                    self.parser.screen().contents()
                );
            }
            match self
                .output
                .recv_timeout((deadline - now).min(Duration::from_millis(100)))
            {
                Ok(bytes) => self.parser.process(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "PTY closed while waiting for {description}\nterminal:\n{}",
                        self.parser.screen().contents()
                    );
                }
            }
        }
    }

    fn wait_for_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "TUI exited with {status:?}");
                return;
            }
            assert!(Instant::now() < deadline, "TUI did not exit after q");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn top_row(&self) -> String {
        top_row(self.parser.screen())
    }

    fn label_is_bold(&self, label: &str) -> bool {
        label_is_bold(self.parser.screen(), label)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
    }
}

fn top_row(screen: &vt100::Screen) -> String {
    let (_, columns) = screen.size();
    screen.rows(0, columns).next().unwrap_or_default()
}

fn label_is_bold(screen: &vt100::Screen, label: &str) -> bool {
    let row = top_row(screen);
    let Some(column) = row.find(label) else {
        return false;
    };
    screen.cell(0, column as u16).is_some_and(vt100::Cell::bold)
}

fn line_contains(screen: &vt100::Screen, label: &str, value: &str) -> bool {
    screen
        .contents()
        .lines()
        .any(|line| line.contains(label) && line.contains(value))
}

#[test]
fn real_tui_pty_handles_keyboard_mouse_search_resize_and_exit() {
    let mut session = PtySession::spawn();
    session.wait_for("initial fixture session", |screen| {
        screen.contents().contains("codex-usage-monit | desktop")
    });
    session.wait_for("selected Overview tab", |screen| {
        label_is_bold(screen, "Overview") || label_is_bold(screen, "Ovw")
    });
    assert!(!session.label_is_bold("Other"));

    session.send(b"3");
    session.wait_for("keyboard switch to Other", |screen| {
        label_is_bold(screen, "Other")
    });

    session.send(b"4");
    session.wait_for("keyboard switch to Settings", |screen| {
        (label_is_bold(screen, "Settings") || label_is_bold(screen, "Set"))
            && screen.contents().contains("Table columns")
            && screen.contents().contains("EST Longx")
    });
    session.wait_for("API equivalent column enabled", |screen| {
        line_contains(screen, "API equivalent", "On")
    });

    session.send(b"a");
    session.wait_for("API equivalent column disabled", |screen| {
        line_contains(screen, "API equivalent", "Off")
    });
    session.send(b"a");
    session.wait_for("API equivalent column restored", |screen| {
        line_contains(screen, "API equivalent", "On")
    });

    session.send(b"1");
    session.wait_for("keyboard switch back to Overview", |screen| {
        label_is_bold(screen, "Overview") || label_is_bold(screen, "Ovw")
    });

    session.send(b"/2");
    session.wait_for("search editor to consume 2", |screen| {
        screen.contents().contains("Filter:2")
    });
    assert!(
        !session.label_is_bold("Other"),
        "2 must be consumed by the search editor"
    );
    session.send(&[0x1b]);
    session.wait_for("search cancellation", |screen| {
        !screen.contents().contains("Filter:2")
    });

    let other_column = session.top_row().find("Other").unwrap() as u16;
    session.click(other_column + 2, 0);
    session.wait_for("mouse switch to Other", |screen| {
        label_is_bold(screen, "Other")
    });

    session.click(2, 0);
    session.wait_for("mouse switch back to Overview", |screen| {
        label_is_bold(screen, "Overview") || label_is_bold(screen, "Ovw")
    });

    session.resize(60, 24);
    session.wait_for_new_output("compact controls after resize", |screen| {
        let contents = screen.contents();
        let mut lines = contents.lines();
        let header = lines.next().unwrap_or_default();
        let controls = lines.next().unwrap_or_default();
        screen.size() == (24, 60)
            && header.contains("4 Set")
            && [header, controls]
                .iter()
                .any(|row| row.contains("[V]") && row.contains("[M]"))
    });

    session.send(b"4");
    session.wait_for("compact keyboard switch to Settings", |screen| {
        label_is_bold(screen, "Set") && screen.contents().contains("API equivalent")
    });

    session.send(b"q");
    session.wait_for_exit();
}
