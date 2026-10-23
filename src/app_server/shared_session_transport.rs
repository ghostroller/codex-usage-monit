//! A bounded WebSocket connection to the desktop's existing App Server.
//! The only child process is Codex's byte proxy; no server is started or stopped.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, ChildStdout};
use std::time::SystemTime;

use anyhow::ensure;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::{Message, WebSocket};

use super::session_management::ManagementRpcError;
use super::*;

const MAX_MANAGEMENT_MESSAGES: usize = 4_096;
const NO_FALLBACK: &str = "session deletion requires the desktop's running App Server; reconnect the desktop host first (no standalone fallback)";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SharedEndpointIdentity {
    configured: PathBuf,
    canonical: PathBuf,
    modified: SystemTime,
    #[cfg(unix)]
    file_identity: (u64, u64),
    #[cfg(windows)]
    file_identity: (u64, [u8; 16]),
}

impl SharedEndpointIdentity {
    fn resolve(configured: &Path) -> Result<Self> {
        let canonical = fs::canonicalize(configured).with_context(|| NO_FALLBACK)?;
        let metadata = fs::metadata(&canonical).with_context(|| NO_FALLBACK)?;
        #[cfg(unix)]
        let file_identity = {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            ensure!(
                metadata.file_type().is_socket(),
                "{NO_FALLBACK}: control endpoint is not a socket"
            );
            ensure!(
                metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
                "{NO_FALLBACK}: control socket is not private to this user"
            );
            (metadata.dev(), metadata.ino())
        };
        #[cfg(windows)]
        let file_identity = windows_endpoint_identity(&canonical)?;
        Ok(Self {
            configured: configured.to_owned(),
            canonical,
            modified: metadata.modified()?,
            #[cfg(any(unix, windows))]
            file_identity,
        })
    }

    pub(crate) fn validate_unchanged(&self) -> Result<()> {
        ensure!(
            Self::resolve(&self.configured)? == *self,
            "desktop App Server endpoint changed; prepare deletion again (no standalone fallback)"
        );
        Ok(())
    }
}

#[cfg(windows)]
fn windows_endpoint_identity(path: &Path) -> Result<(u64, [u8; 16])> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo, GetFileInformationByHandleEx,
    };
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .context("could not verify the existing desktop control socket identity")?;
    let mut info = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error())
            .context("could not identify desktop control socket");
    }
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

struct PipeIo {
    reader: ChildStdout,
    writer: ChildStdin,
}

impl Read for PipeIo {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.reader.read(bytes)
    }
}

impl Write for PipeIo {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.writer.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct WorkerCommand {
    payload: Value,
    request_id: Option<u64>,
    expected_deleted: HashSet<String>,
}

pub(super) struct SharedSessionTransport {
    identity: SharedEndpointIdentity,
    child: Option<ChildGuard>,
    commands: Option<mpsc::SyncSender<WorkerCommand>>,
    results: Option<mpsc::Receiver<Result<Value>>>,
    worker: Option<thread::JoinHandle<()>>,
    stderr_reader: Option<thread::JoinHandle<()>>,
    reader_done: Option<mpsc::Receiver<ReaderTask>>,
    stderr: Arc<Mutex<String>>,
}

impl SharedSessionTransport {
    pub(super) fn connect(config: &CollectConfig, overall_deadline: Instant) -> Result<Self> {
        let home = fs::canonicalize(&config.codex_home).context(NO_FALLBACK)?;
        let identity = SharedEndpointIdentity::resolve(
            &home.join("app-server-control/app-server-control.sock"),
        )?;
        let mut command = codex_command(config)?;
        command
            .args(["app-server", "proxy", "--sock"])
            .arg(&identity.canonical)
            .env("CODEX_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = &config.app_server_path {
            command.env("PATH", path);
        }
        configure_process_tree(&mut command);
        let mut child = command.spawn().context(NO_FALLBACK)?;
        let process_tree = match attach_process_tree(&mut child) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("could not contain the desktop connection proxy");
            }
        };
        let mut child = ChildGuard {
            child,
            process_tree,
            startup_trace: config.startup_trace.clone(),
            trace_log: config.trace_log.clone(),
            reaped: false,
        };
        let io = PipeIo {
            reader: child
                .child
                .stdout
                .take()
                .context("desktop proxy stdout missing")?,
            writer: child
                .child
                .stdin
                .take()
                .context("desktop proxy stdin missing")?,
        };
        let stderr_pipe = child
            .child
            .stderr
            .take()
            .context("desktop proxy stderr missing")?;
        let (commands, incoming) = mpsc::sync_channel(1);
        let (outgoing, results) = mpsc::sync_channel(1);
        let (done, reader_done) = mpsc::channel();
        let worker_done = done.clone();
        let worker = thread::spawn(move || {
            run_worker(io, incoming, outgoing);
            let _ = worker_done.send(ReaderTask::Stdout);
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let output = Arc::clone(&stderr);
        let stderr_reader = thread::spawn(move || {
            capture_stderr(stderr_pipe, output);
            let _ = done.send(ReaderTask::Stderr);
        });
        let mut transport = Self {
            identity,
            child: Some(child),
            commands: Some(commands),
            results: Some(results),
            worker: Some(worker),
            stderr_reader: Some(stderr_reader),
            reader_done: Some(reader_done),
            stderr,
        };
        let deadline = app_server_rpc_deadline(config.app_server_timeout)?.min(overall_deadline);
        transport
            .receive(deadline, "WebSocket handshake")
            .context(NO_FALLBACK)?;
        transport.identity.validate_unchanged()?;
        Ok(transport)
    }

    pub(super) fn endpoint_identity(&self) -> &SharedEndpointIdentity {
        &self.identity
    }

    pub(super) fn notify(&mut self, payload: Value, deadline: Instant) -> Result<()> {
        self.submit(
            WorkerCommand {
                payload,
                request_id: None,
                expected_deleted: HashSet::new(),
            },
            deadline,
            "initialized",
        )?;
        Ok(())
    }

    pub(super) fn rpc(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        expected_deleted: HashSet<String>,
        deadline: Instant,
    ) -> Result<Value> {
        self.identity.validate_unchanged()?;
        self.submit(
            WorkerCommand {
                payload: json!({"id":id,"method":method,"params":params}),
                request_id: Some(id),
                expected_deleted,
            },
            deadline,
            method,
        )
    }

    fn submit(
        &mut self,
        command: WorkerCommand,
        deadline: Instant,
        operation: &str,
    ) -> Result<Value> {
        ensure!(
            deadline > Instant::now(),
            "desktop App Server {operation} deadline expired"
        );
        let sent = self
            .commands
            .as_ref()
            .context("desktop App Server connection is closed")?
            .try_send(command);
        if sent.is_err() {
            self.shutdown();
            bail!("desktop App Server connection worker is unavailable");
        }
        self.receive(deadline, operation)
    }

    fn receive(&mut self, deadline: Instant, operation: &str) -> Result<Value> {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            let error = app_server_timeout_error(operation, &self.stderr);
            self.shutdown();
            return Err(error);
        };
        let result = self
            .results
            .as_ref()
            .context("desktop App Server connection is closed")?
            .recv_timeout(remaining);
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) if error.downcast_ref::<ManagementRpcError>().is_some() => Err(error),
            Ok(Err(error)) => {
                let suffix = stderr_suffix(&self.stderr);
                self.shutdown();
                Err(error).with_context(|| {
                    format!("desktop App Server {operation} failed{suffix}; no standalone fallback")
                })
            }
            Err(error) => {
                let failure = match error {
                    mpsc::RecvTimeoutError::Timeout => {
                        app_server_timeout_error(operation, &self.stderr)
                    }
                    mpsc::RecvTimeoutError::Disconnected => anyhow!(
                        "desktop App Server connection stopped during {operation}{}",
                        stderr_suffix(&self.stderr)
                    ),
                };
                self.shutdown();
                Err(failure)
            }
        }
    }

    fn shutdown(&mut self) {
        // Only the byte proxy belongs to us. Its process group never contains
        // the independently running desktop daemon.
        drop(self.commands.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.terminate_and_reap();
        }
        drop(self.results.take());
        if let (Some(worker), Some(stderr), Some(done)) = (
            self.worker.take(),
            self.stderr_reader.take(),
            self.reader_done.take(),
        ) {
            finish_reader_threads(worker, stderr, done);
        }
    }
}

impl Drop for SharedSessionTransport {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run_worker(
    io: PipeIo,
    incoming: mpsc::Receiver<WorkerCommand>,
    outgoing: mpsc::SyncSender<Result<Value>>,
) {
    let config = WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(APP_SERVER_MAX_FRAME_BYTES + 1024)
        .max_message_size(Some(APP_SERVER_MAX_FRAME_BYTES))
        .max_frame_size(Some(APP_SERVER_MAX_FRAME_BYTES));
    let mut socket =
        match tungstenite::client::client_with_config("ws://localhost/", io, Some(config)) {
            Ok((socket, _)) => socket,
            Err(error) => {
                let _ = outgoing.send(Err(anyhow!("desktop WebSocket handshake failed: {error}")));
                return;
            }
        };
    if outgoing.send(Ok(Value::Null)).is_err() {
        return;
    }
    while let Ok(command) = incoming.recv() {
        let result = run_command(&mut socket, command);
        let fatal = result
            .as_ref()
            .err()
            .is_some_and(|error| error.downcast_ref::<ManagementRpcError>().is_none());
        if outgoing.send(result).is_err() || fatal {
            return;
        }
    }
    // Do not wait for a close handshake: the owner terminates only its proxy.
}

fn run_command<S: Read + Write>(
    socket: &mut WebSocket<S>,
    command: WorkerCommand,
) -> Result<Value> {
    let serialized = serde_json::to_string(&command.payload)?;
    ensure!(
        serialized.len() <= APP_SERVER_MAX_FRAME_BYTES,
        "desktop request exceeds its size bound"
    );
    socket
        .send(Message::Text(serialized.into()))
        .context("could not write desktop request")?;
    let Some(id) = command.request_id else {
        return Ok(Value::Null);
    };
    let mut reply = None;
    let mut deleted = HashSet::new();
    for _ in 0..MAX_MANAGEMENT_MESSAGES {
        let message = match socket.read().context("could not read desktop response")? {
            Message::Text(text) => {
                serde_json::from_str::<Value>(&text).context("invalid desktop JSON response")?
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => {
                bail!("desktop connection closed before the request and deletion events completed")
            }
            _ => bail!("desktop App Server returned a non-text protocol message"),
        };
        if message.get("method").and_then(Value::as_str) == Some("thread/deleted")
            && let Some(thread_id) = message.pointer("/params/threadId").and_then(Value::as_str)
            && command.expected_deleted.contains(thread_id)
        {
            deleted.insert(thread_id.to_owned());
        }
        if response_id(&message) == Some(id) {
            ensure!(
                reply.is_none(),
                "desktop App Server repeated a request response"
            );
            if let Some(error) = message.get("error") {
                return Err(ManagementRpcError {
                    code: error.get("code").and_then(Value::as_i64),
                    message: compact_diagnostic_text(
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown management error"),
                        RPC_ERROR_MESSAGE_LIMIT,
                    ),
                }
                .into());
            }
            let result = message
                .get("result")
                .cloned()
                .context("desktop reply has no result")?;
            if !command.expected_deleted.is_empty() {
                ensure!(
                    result.as_object().is_some_and(Map::is_empty),
                    "unexpected thread/delete response; deletion outcome is unknown"
                );
            }
            reply = Some(result);
        }
        if reply.is_some() && command.expected_deleted.is_subset(&deleted) {
            return Ok(reply.take().unwrap());
        }
    }
    bail!(
        "desktop App Server notification budget exceeded before request and deletion events completed"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tungstenite::protocol::Role;

    const ROOT: &str = "11111111-1111-4111-8111-111111111111";
    const CHILD: &str = "22222222-2222-4222-8222-222222222222";

    #[derive(Default)]
    struct MemoryIo {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for MemoryIo {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            self.input.read(bytes)
        }
    }
    impl Write for MemoryIo {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn socket(messages: Vec<Value>) -> WebSocket<MemoryIo> {
        let mut server = WebSocket::from_raw_socket(MemoryIo::default(), Role::Server, None);
        for message in messages {
            server
                .send(Message::Text(message.to_string().into()))
                .unwrap();
        }
        let io = MemoryIo {
            input: Cursor::new(server.into_inner().output),
            output: Vec::new(),
        };
        WebSocket::from_raw_socket(
            io,
            Role::Client,
            Some(
                WebSocketConfig::default()
                    .max_message_size(Some(APP_SERVER_MAX_FRAME_BYTES))
                    .max_frame_size(Some(APP_SERVER_MAX_FRAME_BYTES)),
            ),
        )
    }

    fn deletion() -> WorkerCommand {
        WorkerCommand {
            payload: json!({"id":7,"method":"thread/delete","params":{"threadId":ROOT}}),
            request_id: Some(7),
            expected_deleted: HashSet::from([ROOT.to_owned(), CHILD.to_owned()]),
        }
    }

    #[test]
    fn shared_session_delete_waits_for_every_exact_notification_before_or_after_reply() {
        let reply = json!({"id":7,"result":{}});
        let root = json!({"method":"thread/deleted","params":{"threadId":ROOT}});
        let child = json!({"method":"thread/deleted","params":{"threadId":CHILD}});
        for messages in [
            vec![root.clone(), reply.clone(), child.clone()],
            vec![reply, child, root],
        ] {
            assert_eq!(
                run_command(&mut socket(messages), deletion()).unwrap(),
                json!({})
            );
        }
        for messages in [
            vec![json!({"id":7,"result":{}})],
            vec![
                json!({"id":7,"result":{}}),
                json!({"method":"thread/deleted","params":{"threadId":ROOT}}),
            ],
            vec![
                json!({"id":7,"result":{}}),
                json!({"method":"thread/deleted","params":{"threadId":format!("{ROOT}-other")}}),
            ],
        ] {
            assert!(run_command(&mut socket(messages), deletion()).is_err());
        }
    }

    #[test]
    fn shared_session_delete_rejects_bad_reply_and_preserves_typed_rpc_errors() {
        assert!(
            run_command(
                &mut socket(vec![json!({"id":7,"result":{"unknown":true}})]),
                deletion()
            )
            .is_err()
        );
        let error = run_command(&mut socket(vec![json!({"id":7,"error":{"code":-32600,"message":format!("thread not found: {ROOT}")}})]), deletion()).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ManagementRpcError>().unwrap().code,
            Some(-32600)
        );
    }

    #[test]
    fn shared_session_response_frame_size_is_bounded() {
        let oversized = json!({"id":7,"result":"x".repeat(APP_SERVER_MAX_FRAME_BYTES)});
        assert!(run_command(&mut socket(vec![oversized]), deletion()).is_err());
    }

    #[test]
    fn shared_session_expired_receive_closes_queued_connection() {
        let (commands, _incoming) = mpsc::sync_channel(1);
        let (_outgoing, results) = mpsc::sync_channel(1);
        let identity = SharedEndpointIdentity {
            configured: PathBuf::new(),
            canonical: PathBuf::new(),
            modified: SystemTime::UNIX_EPOCH,
            #[cfg(unix)]
            file_identity: (0, 0),
            #[cfg(windows)]
            file_identity: (0, [0; 16]),
        };
        let mut transport = SharedSessionTransport {
            identity,
            child: None,
            commands: Some(commands),
            results: Some(results),
            worker: None,
            stderr_reader: None,
            reader_done: None,
            stderr: Arc::new(Mutex::new(String::new())),
        };
        assert!(
            transport
                .receive(Instant::now() - Duration::from_secs(1), "thread/delete")
                .is_err()
        );
        assert!(transport.commands.is_none() && transport.results.is_none());
    }

    #[test]
    fn shared_session_missing_socket_refuses_without_starting_a_server() {
        let directory = tempfile::tempdir().unwrap();
        let config = CollectConfig {
            codex_home: directory.path().into(),
            codex_bin: Some(directory.path().join("must-not-run")),
            ..CollectConfig::default()
        };
        let error =
            SharedSessionTransport::connect(&config, Instant::now() + Duration::from_secs(1))
                .err()
                .unwrap();
        assert!(error.to_string().contains("no standalone fallback"));
        assert!(!directory.path().join("app-server-control").exists());
    }

    #[cfg(unix)]
    #[test]
    fn shared_session_endpoint_identity_tracks_socket_replacement_and_rejects_public_socket() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.sock");
        let second = directory.path().join("second.sock");
        let _first_listener = UnixListener::bind(&first).unwrap();
        let _second_listener = UnixListener::bind(&second).unwrap();
        for path in [&first, &second] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let alias = directory.path().join("control.sock");
        symlink(&first, &alias).unwrap();
        let identity = SharedEndpointIdentity::resolve(&alias).unwrap();
        identity.validate_unchanged().unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&second, &alias).unwrap();
        assert!(identity.validate_unchanged().is_err());
        fs::set_permissions(&second, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(SharedEndpointIdentity::resolve(&alias).is_err());
        let regular = directory.path().join("regular");
        fs::write(&regular, "not a socket").unwrap();
        assert!(SharedEndpointIdentity::resolve(&regular).is_err());
    }
}
