//! Short-lived commands with bounded output, execution, pipe draining and reaping.
//!
//! Pipes are polled directly instead of using reader threads: even a descendant
//! that escapes the isolated tree and retains a pipe cannot strand a worker.

use std::io::{self, Read};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::process_tree::{attach_process_tree, configure_process_tree};

const POLL_INTERVAL: Duration = Duration::from_millis(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_millis(250);

/// The byte limit applies separately to stdout and stderr. Every completion,
/// including a successful primary exit, terminates remaining descendants.
pub(crate) fn output(
    command: &mut Command,
    timeout: Duration,
    max_bytes: usize,
) -> io::Result<Output> {
    output_cancellable(command, timeout, max_bytes, || false)
}

pub(crate) fn output_cancellable(
    command: &mut Command,
    timeout: Duration,
    max_bytes: usize,
    cancelled: impl Fn() -> bool,
) -> io::Result<Output> {
    output_cancellable_with_stdin(command, timeout, max_bytes, Stdio::null(), cancelled)
}

/// A regular-file stdin can carry a bounded bootstrap script without a writer
/// thread or an unbounded pipe write. The caller owns and validates its contents.
pub(crate) fn output_cancellable_with_stdin(
    command: &mut Command,
    timeout: Duration,
    max_bytes: usize,
    stdin: Stdio,
    cancelled: impl Fn() -> bool,
) -> io::Result<Output> {
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "command was cancelled before launch",
        ));
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid command timeout"))?;
    command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_tree(command);
    let mut child = command.spawn()?;
    let mut process_tree = match attach_process_tree(&mut child) {
        Ok(tree) => tree,
        Err(error) => {
            kill_and_reap(&mut child)?;
            return Err(error);
        }
    };
    let result = (|| {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("command stdout pipe was not created"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("command stderr pipe was not created"))?;
        let mut stdout = Pipe::new(stdout, max_bytes)?;
        let mut stderr = Pipe::new(stderr, max_bytes)?;
        let status = loop {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "command was cancelled",
                ));
            }
            stdout.poll()?;
            stderr.poll()?;
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "command exceeded the execution timeout",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        };
        // The primary can exit while descendants retain either pipe. Kill the
        // isolated tree before draining so success also has a bounded lifetime.
        process_tree.terminate(&mut child);
        let drain_deadline = Instant::now() + CLEANUP_TIMEOUT;
        loop {
            stdout.poll()?;
            stderr.poll()?;
            if stdout.closed && stderr.closed {
                return Ok(Output {
                    status,
                    stdout: stdout.bytes,
                    stderr: stderr.bytes,
                });
            }
            if Instant::now() >= drain_deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "command output remained open after process-tree cleanup",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    })();
    // Error paths close both read handles when leaving the closure. No blocking
    // reader or unbounded wait remains, even when output overflows or I/O fails.
    process_tree.terminate(&mut child);
    kill_and_reap(&mut child)?;
    result
}

fn kill_and_reap(child: &mut Child) -> io::Result<ExitStatus> {
    if let Some(status) = child.try_wait()? {
        return Ok(status);
    }
    let kill_error = child.kill().err();
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(kill_error.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "command primary did not exit within the cleanup timeout",
                )
            }));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(unix)]
trait CommandPipe: Read + AsRawFd {}
#[cfg(unix)]
impl<T: Read + AsRawFd> CommandPipe for T {}
#[cfg(windows)]
trait CommandPipe: Read + AsRawHandle {}
#[cfg(windows)]
impl<T: Read + AsRawHandle> CommandPipe for T {}

struct Pipe<R> {
    reader: R,
    bytes: Vec<u8>,
    max_bytes: usize,
    closed: bool,
}

impl<R: CommandPipe> Pipe<R> {
    fn new(reader: R, max_bytes: usize) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let descriptor = reader.as_raw_fd();
            let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
            if flags < 0
                || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self {
            reader,
            bytes: Vec::with_capacity(max_bytes.min(4096)),
            max_bytes,
            closed: false,
        })
    }

    fn poll(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        let mut buffer = [0_u8; 4096];
        // Limit work per tick so continuously writing children cannot starve
        // the other pipe or the execution deadline.
        for _ in 0..16 {
            let requested = buffer.len().min(
                self.max_bytes
                    .saturating_sub(self.bytes.len())
                    .saturating_add(1),
            );
            #[cfg(windows)]
            let requested = {
                use windows_sys::Win32::System::Pipes::PeekNamedPipe;
                let mut available = 0_u32;
                if unsafe {
                    PeekNamedPipe(
                        self.reader.as_raw_handle().cast(),
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    let error = io::Error::last_os_error();
                    if pipe_is_closed(&error) {
                        self.closed = true;
                        return Ok(());
                    }
                    return Err(error);
                }
                if available == 0 {
                    return Ok(());
                }
                requested.min(available as usize)
            };
            match self.reader.read(&mut buffer[..requested]) {
                Ok(0) => {
                    self.closed = true;
                    return Ok(());
                }
                Ok(read) => {
                    if read > self.max_bytes.saturating_sub(self.bytes.len()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("command output exceeds the {}-byte limit", self.max_bytes),
                        ));
                    }
                    self.bytes.extend_from_slice(&buffer[..read]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                #[cfg(windows)]
                Err(error) if pipe_is_closed(&error) => {
                    self.closed = true;
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
fn pipe_is_closed(error: &io::Error) -> bool {
    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
    };
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
        .is_some_and(|code| {
            matches!(
                code,
                ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED
            )
        })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io::Write;
    use std::path::Path;

    use super::*;

    const MODE_ENV: &str = "CODEX_USAGE_MONIT_BOUNDED_PROCESS_FIXTURE";
    const PID_DIR_ENV: &str = "CODEX_USAGE_MONIT_BOUNDED_PROCESS_PID_DIR";

    pub(crate) fn fixture_command(mode: &str, directory: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "bounded_process::tests::process_fixture",
                "--nocapture",
            ])
            .env(MODE_ENV, mode)
            .env(PID_DIR_ENV, directory);
        command
    }

    fn publish_fixture_pid(
        directory: &Path,
        mode: &str,
        pid: u32,
        before_write: impl FnOnce(&Path),
    ) {
        // Consumers may cancel the entire tree as soon as this marker exists.
        // Publish only after closing the complete PID, never the empty file
        // that File::create makes visible before its first write.
        let pending = directory.join(format!(".{mode}.pid-pending"));
        let mut file = fs::File::create(&pending).unwrap();
        before_write(&pending);
        write!(file, "{pid}").unwrap();
        drop(file);
        fs::rename(pending, directory.join(mode)).unwrap();
    }

    #[test]
    fn pid_marker_is_published_only_after_the_complete_pid_is_written() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("descendant");
        let pid = std::process::id();
        publish_fixture_pid(directory.path(), "descendant", pid, |pending| {
            assert!(fs::read(pending).unwrap().is_empty());
            assert!(
                !marker.is_file(),
                "an incomplete PID marker must not signal readiness"
            );
        });
        assert_eq!(fs::read_to_string(marker).unwrap(), pid.to_string());
        assert!(!directory.path().join(".descendant.pid-pending").exists());
    }

    #[test]
    fn process_fixture() {
        let Ok(mode) = std::env::var(MODE_ENV) else {
            return;
        };
        let directory = std::path::PathBuf::from(std::env::var_os(PID_DIR_ENV).unwrap());
        publish_fixture_pid(&directory, &mode, std::process::id(), |_| {});
        match mode.as_str() {
            "stdin" => {
                let mut bytes = Vec::new();
                io::stdin().read_to_end(&mut bytes).unwrap();
                fs::write(directory.join("received"), bytes).unwrap();
                std::process::exit(0);
            }
            "success" | "failure" => {
                println!("fixture stdout");
                eprintln!("fixture stderr");
                io::stdout().flush().unwrap();
                io::stderr().flush().unwrap();
                std::process::exit(if mode == "success" { 0 } else { 17 });
            }
            "flood_stdout" | "flood_stderr" => {
                let mut writer: Box<dyn Write> = if mode == "flood_stdout" {
                    Box::new(io::stdout())
                } else {
                    Box::new(io::stderr())
                };
                loop {
                    if writer.write_all(&[b'x'; 8192]).is_err() {
                        break;
                    }
                }
            }
            "parent_exits" | "parent_hangs" => {
                let mut descendant = fixture_command("descendant", &directory).spawn().unwrap();
                let ready_deadline = Instant::now() + Duration::from_secs(5);
                while !directory.join("descendant").is_file() {
                    assert!(Instant::now() < ready_deadline, "descendant did not start");
                    thread::sleep(POLL_INTERVAL);
                }
                if mode == "parent_exits" {
                    println!("primary complete");
                    io::stdout().flush().unwrap();
                    std::process::exit(0);
                }
                // A wait gives the timeout test an indefinitely live primary
                // and descendant, both holding the captured output handles.
                descendant.wait().unwrap();
            }
            "hang" | "descendant" => loop {
                thread::sleep(Duration::from_secs(60));
            },
            _ => panic!("unknown process fixture mode"),
        }
    }

    fn assert_terminated(directory: &Path, mode: &str) {
        let marker = directory.join(mode);
        let contents = fs::read_to_string(&marker)
            .unwrap_or_else(|error| panic!("read {mode} PID marker {}: {error}", marker.display()));
        let pid: u32 = contents
            .parse()
            .unwrap_or_else(|error| panic!("invalid {mode} PID marker {contents:?}: {error}"));
        assert_ne!(pid, 0, "{mode} PID marker must identify a process");
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(pid) {
            assert!(
                Instant::now() < deadline,
                "{mode} process {pid} survived cleanup"
            );
            thread::sleep(POLL_INTERVAL);
        }
    }

    #[cfg(unix)]
    fn process_is_running(pid: u32) -> bool {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
            return false;
        }
        // Linux containers need not promptly reap orphaned zombies. They are
        // terminated and hold no output pipes, even if kill(pid, 0) succeeds.
        #[cfg(target_os = "linux")]
        if fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(") ")
                    .map(|(_, tail)| tail.starts_with('Z'))
            })
            == Some(true)
        {
            return false;
        }
        true
    }

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            return false;
        }
        let running = unsafe { WaitForSingleObject(process, 0) } == WAIT_TIMEOUT;
        unsafe { CloseHandle(process) };
        running
    }

    #[test]
    fn regular_file_stdin_reaches_child_and_keeps_bounded_output_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let bytes = vec![b'x'; 64 * 1024];
        fs::write(&input, &bytes).unwrap();
        let result = output_cancellable_with_stdin(
            &mut fixture_command("stdin", directory.path()),
            Duration::from_secs(5),
            4096,
            Stdio::from(fs::File::open(input).unwrap()),
            || false,
        )
        .unwrap();
        assert!(result.status.success());
        assert_eq!(fs::read(directory.path().join("received")).unwrap(), bytes);
        assert_terminated(directory.path(), "stdin");
    }

    #[test]
    fn preserves_both_output_streams_and_failure_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        for (mode, expected_code) in [("success", 0), ("failure", 17)] {
            let result = output(
                &mut fixture_command(mode, directory.path()),
                Duration::from_secs(5),
                4096,
            )
            .unwrap();
            assert_eq!(result.status.code(), Some(expected_code));
            assert!(String::from_utf8_lossy(&result.stdout).contains("fixture stdout"));
            assert!(String::from_utf8_lossy(&result.stderr).contains("fixture stderr"));
            assert_terminated(directory.path(), mode);
        }
    }

    #[test]
    fn timeout_terminates_and_reaps_a_hung_primary_and_its_descendant() {
        let directory = tempfile::tempdir().unwrap();
        let before_launch = Cell::new(true);
        let descendant_ready = Cell::new(false);
        let cleanup_started = Cell::new(None);
        let result = output_cancellable(
            &mut fixture_command("parent_hangs", directory.path()),
            Duration::ZERO,
            4096,
            || {
                if before_launch.replace(false) {
                    return false;
                }
                // Wait at the first post-launch poll before checking the
                // already-expired deadline. Startup speed must not determine
                // whether this test actually covers descendant cleanup.
                let ready_deadline = Instant::now() + Duration::from_secs(5);
                while !directory.path().join("descendant").is_file()
                    && Instant::now() < ready_deadline
                {
                    thread::sleep(POLL_INTERVAL);
                }
                descendant_ready.set(directory.path().join("descendant").is_file());
                cleanup_started.set(Some(Instant::now()));
                // Never panic inside this callback: even failed fixture
                // startup must go through the normal process-tree cleanup.
                false
            },
        );
        assert!(
            descendant_ready.get(),
            "descendant did not publish its PID before the readiness deadline: {result:?}"
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(cleanup_started.get().unwrap().elapsed() < Duration::from_secs(5));
        assert_terminated(directory.path(), "parent_hangs");
        assert_terminated(directory.path(), "descendant");
    }

    #[test]
    fn primary_exit_terminates_descendants_that_retain_output_pipes() {
        let directory = tempfile::tempdir().unwrap();
        let started = Instant::now();
        let result = output(
            &mut fixture_command("parent_exits", directory.path()),
            Duration::from_secs(5),
            4096,
        )
        .unwrap();
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).contains("primary complete"));
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_terminated(directory.path(), "parent_exits");
        assert_terminated(directory.path(), "descendant");
    }

    #[test]
    fn cancellation_terminates_and_reaps_the_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let mut command = fixture_command("parent_hangs", directory.path());
        let error = output_cancellable(&mut command, Duration::from_secs(5), 4096, || {
            directory.path().join("descendant").is_file()
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_terminated(directory.path(), "parent_hangs");
        assert_terminated(directory.path(), "descendant");
        let directory = tempfile::tempdir().unwrap();
        let error = output_cancellable(
            &mut fixture_command("success", directory.path()),
            Duration::from_secs(5),
            4096,
            || true,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!directory.path().join("success").exists());
    }

    #[test]
    fn oversized_stdout_and_stderr_terminate_and_reap_the_writer() {
        let directory = tempfile::tempdir().unwrap();
        for mode in ["flood_stdout", "flood_stderr"] {
            let error = output(
                &mut fixture_command(mode, directory.path()),
                Duration::from_secs(5),
                1024,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_terminated(directory.path(), mode);
        }
    }
}
