//! Shared process manager for Codex-style command execution.
//!
//! This intentionally mirrors the important unified-exec contract from Codex:
//! numeric session ids, model-visible text results, live output events, PTY
//! stdin support, interrupt-persistent sessions, bounded head/tail retained
//! output, and explicit polling via `write_stdin`.

use std::collections::HashMap;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child as StdChild, Command as StdCommand, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtySize};
use rand::Rng;
use serde_json::json;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;

use crate::events::{EventSink, PendingEvent};
use crate::tools::runtime::ToolError;

pub const MIN_YIELD_TIME_MS: u64 = 250;
pub const MIN_EMPTY_YIELD_TIME_MS: u64 = 5_000;
pub const MAX_YIELD_TIME_MS: u64 = 30_000;
pub const DEFAULT_EXEC_YIELD_TIME_MS: u64 = 10_000;
pub const DEFAULT_WRITE_STDIN_YIELD_TIME_MS: u64 = 250;
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
pub const MAX_RETAINED_OUTPUT_BYTES: usize = 1024 * 1024;

const READ_CHUNK_SIZE: usize = 8192;
const TRAILING_OUTPUT_GRACE_MS: u64 = 100;
const WAIT_FOR_READERS_MS: u64 = 2_000;
const EXIT_WATCH_INTERVAL_MS: u64 = 100;
const MAX_UNIFIED_EXEC_PROCESSES: usize = 64;
const AGENT_TOOLS_DIR_ENV: &str = "BUT_AGENT_TOOLS_DIR";
const PATH_ENV: &str = "PATH";
const UNIFIED_EXEC_ENV: [(&str, &str); 10] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("LANG", "C.UTF-8"),
    ("LC_CTYPE", "C.UTF-8"),
    ("LC_ALL", "C.UTF-8"),
    ("COLORTERM", ""),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("CODEX_CI", "1"),
];

/// Emits process events into the session event log.
#[derive(Clone)]
pub struct UnifiedExecEventEmitter {
    sink: Arc<dyn EventSink>,
    session_id: String,
}

impl std::fmt::Debug for UnifiedExecEventEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecEventEmitter")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl UnifiedExecEventEmitter {
    pub fn new(sink: Arc<dyn EventSink>, session_id: impl Into<String>) -> Self {
        Self {
            sink,
            session_id: session_id.into(),
        }
    }

    pub fn emit_exec_begin(&self, event: ExecBeginEvent<'_>) {
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            "exec_command.begin",
            json!({
                "name": event.tool_name,
                "tool_call_id": event.call_id,
                "process_id": event.session_id.to_string(),
                "session_id": event.session_id,
                "command": event.argv,
                "cwd": event.cwd.display().to_string(),
            }),
        ));
    }

    fn emit_output_delta(&self, event: OutputDeltaEvent<'_>) {
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            "tool.output_delta",
            json!({
                "name": event.tool_name,
                "tool_call_id": event.call_id,
                "process_id": event.session_id.to_string(),
                "session_id": event.session_id,
                "stream": event.stream,
                "text": event.text,
            }),
        ));
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            "exec_command.output_delta",
            json!({
                "call_id": event.call_id,
                "process_id": event.session_id.to_string(),
                "session_id": event.session_id,
                "stream": event.stream,
                "chunk": event.text,
            }),
        ));
    }

    fn emit_exec_end(&self, event: ExecEndEvent<'_>) {
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            "exec_command.end",
            json!({
                "name": event.tool_name,
                "tool_call_id": event.call_id,
                "process_id": event.session_id.to_string(),
                "session_id": event.session_id,
                "exit_code": event.exit_code,
                "wall_time_seconds": event.wall_time.as_secs_f64(),
                "output": event.output,
            }),
        ));
    }

    pub fn emit_command_waiting(&self, tool_name: &str, call_id: &str, session_id: i32) {
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            "command.waiting",
            json!({
                "name": tool_name,
                "tool_call_id": call_id,
                "process_id": session_id.to_string(),
                "session_id": session_id,
            }),
        ));
    }

    pub fn emit_terminal_interaction(&self, call_id: &str, session_id: i32, stdin: &str) {
        self.sink.emit(PendingEvent::new(
            self.session_id.clone(),
            "terminal.interaction",
            json!({
                "call_id": call_id,
                "process_id": session_id.to_string(),
                "session_id": session_id,
                "stdin": stdin,
            }),
        ));
    }
}

pub struct ExecBeginEvent<'a> {
    pub tool_name: &'a str,
    pub call_id: &'a str,
    pub session_id: i32,
    pub argv: &'a [String],
    pub cwd: &'a PathBuf,
}

struct OutputDeltaEvent<'a> {
    tool_name: &'a str,
    call_id: &'a str,
    session_id: i32,
    stream: &'a str,
    text: &'a str,
}

struct ExecEndEvent<'a> {
    tool_name: &'a str,
    call_id: &'a str,
    session_id: i32,
    exit_code: i32,
    wall_time: Duration,
    output: &'a str,
}

/// Shared process manager. Cheap to clone.
#[derive(Clone, Default)]
pub struct UnifiedExecManager {
    inner: Arc<UnifiedExecManagerInner>,
}

impl std::fmt::Debug for UnifiedExecManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnifiedExecManager").finish_non_exhaustive()
    }
}

struct UnifiedExecManagerInner {
    processes: Mutex<HashMap<i32, Arc<ManagedProcess>>>,
    next_session_id: Mutex<i32>,
    deterministic_session_ids: bool,
}

impl Default for UnifiedExecManagerInner {
    fn default() -> Self {
        Self {
            processes: Mutex::new(HashMap::new()),
            next_session_id: Mutex::new(1000),
            deterministic_session_ids: false,
        }
    }
}

pub struct SpawnProcessRequest {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub tty: bool,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub timeout_ms: Option<u64>,
    pub kill_on_cancel: bool,
    pub call_id: String,
    pub tool_name: String,
    pub emitter: Option<Arc<UnifiedExecEventEmitter>>,
    pub cancel: Option<CancellationToken>,
}

pub struct WriteStdinRequest {
    pub session_id: i32,
    pub chars: String,
    pub yield_time_ms: u64,
    pub max_output_tokens: Option<usize>,
    pub call_id: String,
    pub tool_name: String,
    pub emitter: Option<Arc<UnifiedExecEventEmitter>>,
}

#[derive(Clone, Debug)]
pub struct UnifiedExecSnapshot {
    pub session_id: i32,
    pub output: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub running: bool,
    pub timed_out: bool,
    pub output_truncated: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub wall_time: Duration,
    pub chunk_id: String,
    pub original_token_count: usize,
    pub max_output_tokens: Option<usize>,
}

impl UnifiedExecSnapshot {
    pub fn to_model_text(&self) -> String {
        let mut sections = Vec::new();
        if !self.chunk_id.is_empty() {
            sections.push(format!("Chunk ID: {}", self.chunk_id));
        }
        sections.push(format!(
            "Wall time: {:.4} seconds",
            self.wall_time.as_secs_f64()
        ));
        if let Some(exit_code) = self.exit_code {
            sections.push(format!("Process exited with code {exit_code}"));
        }
        if self.running {
            sections.push(format!(
                "Process running with session ID {}",
                self.session_id
            ));
        }
        sections.push(format!(
            "Original token count: {}",
            self.original_token_count
        ));
        sections.push("Output:".to_string());
        sections.push(formatted_truncate_text(
            &self.output,
            self.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
        ));
        sections.join("\n")
    }
}

impl UnifiedExecManager {
    #[cfg(test)]
    pub fn deterministic_for_tests() -> Self {
        Self {
            inner: Arc::new(UnifiedExecManagerInner {
                processes: Mutex::new(HashMap::new()),
                next_session_id: Mutex::new(1000),
                deterministic_session_ids: true,
            }),
        }
    }

    pub async fn spawn_process(
        &self,
        req: SpawnProcessRequest,
    ) -> Result<UnifiedExecSnapshot, ToolError> {
        self.spawn_process_inner(req, true).await
    }

    async fn spawn_process_inner(
        &self,
        req: SpawnProcessRequest,
        advance_cursor: bool,
    ) -> Result<UnifiedExecSnapshot, ToolError> {
        if req.argv.is_empty() {
            return Err(ToolError::Other(anyhow::anyhow!("empty command")));
        }

        let session_id = self.allocate_session_id().await;
        let env = apply_unified_exec_env(req.env.clone());
        let spawned = spawn_managed_backend(&req.argv, &req.cwd, &env, req.tty).await?;
        let process = Arc::new(ManagedProcess::new(
            session_id,
            spawned.child,
            spawned.stdin,
            spawned.pty_master,
            spawned.reader_count,
            req.call_id,
            req.tool_name,
            req.argv.clone(),
            req.cwd.clone(),
            req.tty,
            req.emitter,
        ));

        self.store_process(process.clone()).await;
        process.emit_begin();

        for reader in spawned.blocking_readers {
            spawn_blocking_reader(process.clone(), reader.stream, reader.reader);
        }
        spawn_exit_watcher(process.clone());
        let return_on_cancel = if req.kill_on_cancel {
            None
        } else {
            req.cancel.clone()
        };
        if req.kill_on_cancel {
            if let Some(cancel) = req.cancel.clone() {
                spawn_cancel_watcher(process.clone(), cancel);
            }
        }
        if let Some(timeout_ms) = req.timeout_ms {
            spawn_timeout_watcher(process.clone(), timeout_ms);
        }

        let mut snapshot = self
            .wait_for_snapshot(
                &process,
                req.yield_time_ms,
                advance_cursor,
                false,
                return_on_cancel,
            )
            .await?;
        snapshot.max_output_tokens = req.max_output_tokens;
        if !snapshot.running {
            self.inner
                .processes
                .lock()
                .await
                .remove(&snapshot.session_id);
        }
        Ok(snapshot)
    }

    pub async fn write_stdin(
        &self,
        req: WriteStdinRequest,
    ) -> Result<UnifiedExecSnapshot, ToolError> {
        let process = self
            .inner
            .processes
            .lock()
            .await
            .get(&req.session_id)
            .cloned()
            .ok_or_else(|| {
                ToolError::Other(anyhow::anyhow!("unknown session `{}`", req.session_id))
            })?;

        process
            .update_call_context(req.call_id, req.tool_name, req.emitter)
            .await;
        if !req.chars.is_empty() {
            process.write_stdin(&req.chars).await?;
            process.emit_terminal_interaction(&req.chars);
            tokio::time::sleep(Duration::from_millis(TRAILING_OUTPUT_GRACE_MS)).await;
        }
        let mut snapshot = self
            .wait_for_snapshot(
                &process,
                req.yield_time_ms,
                true,
                req.chars.is_empty(),
                None,
            )
            .await?;
        snapshot.max_output_tokens = req.max_output_tokens;
        if !snapshot.running {
            self.inner
                .processes
                .lock()
                .await
                .remove(&snapshot.session_id);
        } else if req.chars.is_empty() {
            process.emit_terminal_interaction("");
        }
        Ok(snapshot)
    }

    pub async fn run_to_completion(
        &self,
        req: SpawnProcessRequest,
    ) -> Result<UnifiedExecSnapshot, ToolError> {
        let timeout_ms = req.timeout_ms;
        let snapshot = self.spawn_process_inner(req, false).await?;
        let Some(process) = self
            .inner
            .processes
            .lock()
            .await
            .get(&snapshot.session_id)
            .cloned()
        else {
            return Ok(snapshot);
        };

        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        loop {
            let snap = self
                .wait_for_snapshot(
                    &process,
                    DEFAULT_WRITE_STDIN_YIELD_TIME_MS,
                    false,
                    false,
                    None,
                )
                .await?;
            if !snap.running {
                self.inner.processes.lock().await.remove(&snap.session_id);
                return Ok(snap);
            }
            if let Some(deadline) = deadline {
                if Instant::now() >= deadline {
                    process.terminate_as_timeout().await;
                    process
                        .wait_for_readers(Duration::from_millis(WAIT_FOR_READERS_MS))
                        .await;
                    process.emit_end_if_needed().await;
                    let snap = process.snapshot_since_cursor(false, Duration::ZERO).await;
                    self.inner.processes.lock().await.remove(&snap.session_id);
                    return Ok(snap);
                }
            }
        }
    }

    pub async fn terminate_all(&self) -> usize {
        let processes = {
            let mut locked = self.inner.processes.lock().await;
            locked
                .drain()
                .map(|(_, process)| process)
                .collect::<Vec<_>>()
        };
        let count = processes.len();
        for process in processes {
            process.terminate(130).await;
        }
        count
    }

    pub fn terminate_all_best_effort(&self) -> usize {
        if tokio::runtime::Handle::try_current().is_ok() {
            let manager = self.clone();
            return thread::spawn(move || manager.terminate_all_blocking_inner())
                .join()
                .unwrap_or(0);
        }
        self.terminate_all_blocking_inner()
    }

    fn terminate_all_blocking_inner(&self) -> usize {
        let processes = {
            let mut locked = self.inner.processes.blocking_lock();
            locked
                .drain()
                .map(|(_, process)| process)
                .collect::<Vec<_>>()
        };
        let count = processes.len();
        for process in processes {
            process.terminate_blocking(130);
        }
        count
    }

    async fn allocate_session_id(&self) -> i32 {
        loop {
            let candidate = if self.inner.deterministic_session_ids {
                let mut next = self.inner.next_session_id.lock().await;
                let candidate = *next;
                *next = next.saturating_add(1);
                candidate
            } else {
                rand::rng().random_range(1_000..100_000)
            };
            if !self.inner.processes.lock().await.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    async fn store_process(&self, process: Arc<ManagedProcess>) {
        let pruned = {
            let mut processes = self.inner.processes.lock().await;
            let pruned_id = if processes.len() >= MAX_UNIFIED_EXEC_PROCESSES {
                processes.keys().copied().min()
            } else {
                None
            };
            let pruned = pruned_id.and_then(|session_id| processes.remove(&session_id));
            processes.insert(process.session_id, process);
            pruned
        };
        if let Some(process) = pruned {
            process.terminate(130).await;
        }
    }

    async fn wait_for_snapshot(
        &self,
        process: &Arc<ManagedProcess>,
        yield_time_ms: u64,
        advance_cursor: bool,
        empty_poll: bool,
        return_on_cancel: Option<CancellationToken>,
    ) -> Result<UnifiedExecSnapshot, ToolError> {
        let start = Instant::now();
        let yield_time_ms = clamp_yield_time(yield_time_ms, empty_poll);
        let deadline = Instant::now() + Duration::from_millis(yield_time_ms);
        loop {
            process.refresh_exit_status().await?;
            if !process.is_running().await {
                process
                    .wait_for_readers(Duration::from_millis(WAIT_FOR_READERS_MS))
                    .await;
                process.emit_end_if_needed().await;
                return Ok(process
                    .snapshot_since_cursor(advance_cursor, start.elapsed())
                    .await);
            }

            let now = Instant::now();
            if now >= deadline {
                let snap = process
                    .snapshot_since_cursor(advance_cursor, start.elapsed())
                    .await;
                if snap.running {
                    process.emit_waiting();
                }
                return Ok(snap);
            }

            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
            tokio::pin!(sleep);
            tokio::select! {
                _ = process.notify.notified() => {}
                _ = async {
                    if let Some(cancel) = return_on_cancel.as_ref() {
                        cancel.cancelled().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    // Cancellation returns a live snapshot without killing the
                    // process. Give the blocking reader one small grace window
                    // to copy bytes the child already wrote before the cancel
                    // signal won this select branch.
                    tokio::time::sleep(Duration::from_millis(TRAILING_OUTPUT_GRACE_MS)).await;
                    let snap = process.snapshot_since_cursor(advance_cursor, start.elapsed()).await;
                    if snap.running {
                        process.emit_waiting();
                    }
                    return Ok(snap);
                }
                _ = &mut sleep => {
                    let snap = process.snapshot_since_cursor(advance_cursor, start.elapsed()).await;
                    if snap.running {
                        process.emit_waiting();
                    }
                    return Ok(snap);
                }
            }
        }
    }
}

struct SpawnedProcess {
    child: ManagedChild,
    stdin: Option<ManagedStdin>,
    pty_master: Option<Box<dyn MasterPty + Send>>,
    reader_count: usize,
    blocking_readers: Vec<BlockingReader>,
}

struct BlockingReader {
    stream: &'static str,
    reader: Box<dyn Read + Send>,
}

enum ManagedChild {
    Pipe {
        child: StdChild,
        process_group: Option<ProcessGroup>,
    },
    Pty(Box<dyn portable_pty::Child + Send + Sync>),
}

enum ManagedStdin {
    Pty(Box<dyn Write + Send>),
}

#[derive(Clone, Copy, Debug)]
struct ProcessGroup {
    #[cfg(unix)]
    pgid: libc::pid_t,
}

async fn spawn_managed_backend(
    argv: &[String],
    cwd: &PathBuf,
    env: &HashMap<String, String>,
    tty: bool,
) -> Result<SpawnedProcess, ToolError> {
    if tty {
        spawn_pty_backend(argv, cwd, env).await
    } else {
        spawn_pipe_backend(argv, cwd, env).await
    }
}

async fn spawn_pipe_backend(
    argv: &[String],
    cwd: &PathBuf,
    env: &HashMap<String, String>,
) -> Result<SpawnedProcess, ToolError> {
    let program = &argv[0];
    let mut command = StdCommand::new(program);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_pipe_child_process_group(&mut command);
    for (key, value) in env {
        command.env(key, value);
    }

    let mut child = command.spawn().map_err(|source| {
        ToolError::Other(anyhow::anyhow!("failed to spawn `{program}`: {source}"))
    })?;
    let process_group = process_group_for_child(&child);
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut blocking_readers = Vec::new();
    if let Some(reader) = stdout {
        blocking_readers.push(BlockingReader {
            stream: "stdout",
            reader: Box::new(reader),
        });
    }
    if let Some(reader) = stderr {
        blocking_readers.push(BlockingReader {
            stream: "stderr",
            reader: Box::new(reader),
        });
    }
    Ok(SpawnedProcess {
        child: ManagedChild::Pipe {
            child,
            process_group,
        },
        stdin: None,
        pty_master: None,
        reader_count: blocking_readers.len(),
        blocking_readers,
    })
}

fn configure_pipe_child_process_group(command: &mut StdCommand) {
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    #[cfg(not(unix))]
    let _ = command;
}

fn process_group_for_child(child: &StdChild) -> Option<ProcessGroup> {
    #[cfg(unix)]
    {
        Some(ProcessGroup {
            pgid: child.id() as libc::pid_t,
        })
    }
    #[cfg(not(unix))]
    {
        let _ = child;
        None
    }
}

async fn spawn_pty_backend(
    argv: &[String],
    cwd: &PathBuf,
    env: &HashMap<String, String>,
) -> Result<SpawnedProcess, ToolError> {
    let argv = argv.to_vec();
    let cwd = cwd.clone();
    let env = env.clone();
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize::default())
        .map_err(|source| ToolError::Other(anyhow::anyhow!("failed to open pty: {source}")))?;
    let mut builder = CommandBuilder::new(&argv[0]);
    builder.args(&argv[1..]);
    builder.cwd(cwd.as_os_str());
    for (key, value) in &env {
        builder.env(key, value);
    }
    let child = pair.slave.spawn_command(builder).map_err(|source| {
        ToolError::Other(anyhow::anyhow!("failed to spawn `{}`: {source}", argv[0]))
    })?;
    let reader = pair.master.try_clone_reader().map_err(|source| {
        ToolError::Other(anyhow::anyhow!("failed to open pty reader: {source}"))
    })?;
    let writer = pair.master.take_writer().map_err(|source| {
        ToolError::Other(anyhow::anyhow!("failed to open pty writer: {source}"))
    })?;

    Ok(SpawnedProcess {
        child: ManagedChild::Pty(child),
        stdin: Some(ManagedStdin::Pty(writer)),
        pty_master: Some(pair.master),
        reader_count: 1,
        blocking_readers: vec![BlockingReader {
            stream: "stdout",
            reader,
        }],
    })
}

struct ManagedProcess {
    session_id: i32,
    child: Mutex<ManagedChild>,
    stdin: Mutex<Option<ManagedStdin>>,
    _pty_master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    state: Mutex<ManagedProcessState>,
    notify: Notify,
    started_at: Instant,
}

struct ManagedProcessState {
    output: HeadTailStream,
    stdout: HeadTailStream,
    stderr: HeadTailStream,
    output_cursor: usize,
    stdout_cursor: usize,
    stderr_cursor: usize,
    exit_code: Option<i32>,
    running: bool,
    timed_out: bool,
    open_readers: usize,
    end_emitted: bool,
    call_id: String,
    tool_name: String,
    argv: Vec<String>,
    cwd: PathBuf,
    tty: bool,
    emitter: Option<Arc<UnifiedExecEventEmitter>>,
}

impl ManagedProcess {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: i32,
        child: ManagedChild,
        stdin: Option<ManagedStdin>,
        pty_master: Option<Box<dyn MasterPty + Send>>,
        open_readers: usize,
        call_id: String,
        tool_name: String,
        argv: Vec<String>,
        cwd: PathBuf,
        tty: bool,
        emitter: Option<Arc<UnifiedExecEventEmitter>>,
    ) -> Self {
        Self {
            session_id,
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            _pty_master: Mutex::new(pty_master),
            state: Mutex::new(ManagedProcessState {
                output: HeadTailStream::new(MAX_RETAINED_OUTPUT_BYTES),
                stdout: HeadTailStream::new(MAX_RETAINED_OUTPUT_BYTES),
                stderr: HeadTailStream::new(MAX_RETAINED_OUTPUT_BYTES),
                output_cursor: 0,
                stdout_cursor: 0,
                stderr_cursor: 0,
                exit_code: None,
                running: true,
                timed_out: false,
                open_readers,
                end_emitted: false,
                call_id,
                tool_name,
                argv,
                cwd,
                tty,
                emitter,
            }),
            notify: Notify::new(),
            started_at: Instant::now(),
        }
    }

    async fn update_call_context(
        &self,
        call_id: String,
        tool_name: String,
        emitter: Option<Arc<UnifiedExecEventEmitter>>,
    ) {
        let mut state = self.state.lock().await;
        state.call_id = call_id;
        state.tool_name = tool_name;
        if emitter.is_some() {
            state.emitter = emitter;
        }
    }

    fn emit_begin(&self) {
        if let Ok(state) = self.state.try_lock() {
            if let Some(emitter) = state.emitter.as_ref() {
                emitter.emit_exec_begin(ExecBeginEvent {
                    tool_name: &state.tool_name,
                    call_id: &state.call_id,
                    session_id: self.session_id,
                    argv: &state.argv,
                    cwd: &state.cwd,
                });
            }
        }
    }

    fn append_output_blocking(&self, stream: &'static str, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes).to_string();
        let mut state = self.state.blocking_lock();
        state.output.push(&text);
        match stream {
            "stdout" => state.stdout.push(&text),
            "stderr" => state.stderr.push(&text),
            _ => {}
        }
        if let Some(emitter) = state.emitter.clone() {
            emitter.emit_output_delta(OutputDeltaEvent {
                tool_name: &state.tool_name,
                call_id: &state.call_id,
                session_id: self.session_id,
                stream,
                text: &text,
            });
        }
        drop(state);
        self.notify.notify_waiters();
    }

    fn reader_done_blocking(&self) {
        let mut state = self.state.blocking_lock();
        state.open_readers = state.open_readers.saturating_sub(1);
        drop(state);
        self.notify.notify_waiters();
    }

    async fn write_stdin(&self, chars: &str) -> Result<(), ToolError> {
        if !self.state.lock().await.tty {
            return Err(ToolError::Other(anyhow::anyhow!("stdin is closed")));
        }
        let mut stdin = self.stdin.lock().await;
        let Some(stdin) = stdin.as_mut() else {
            return Err(ToolError::Other(anyhow::anyhow!("stdin is closed")));
        };
        match stdin {
            ManagedStdin::Pty(writer) => {
                writer.write_all(chars.as_bytes()).map_err(|source| {
                    ToolError::Other(anyhow::anyhow!("writing stdin: {source}"))
                })?;
                writer.flush().map_err(|source| {
                    ToolError::Other(anyhow::anyhow!("flushing stdin: {source}"))
                })?;
            }
        }
        Ok(())
    }

    async fn refresh_exit_status(&self) -> Result<(), ToolError> {
        if !self.is_running().await {
            return Ok(());
        }
        let status = {
            let mut child = self.child.lock().await;
            match &mut *child {
                ManagedChild::Pipe { child, .. } => child
                    .try_wait()
                    .map_err(|source| {
                        ToolError::Other(anyhow::anyhow!("polling process: {source}"))
                    })?
                    .map(|status| status.code().unwrap_or_else(|| signal_exit_code(&status))),
                ManagedChild::Pty(child) => child
                    .try_wait()
                    .map_err(|source| {
                        ToolError::Other(anyhow::anyhow!("polling process: {source}"))
                    })?
                    .map(|status| status.exit_code() as i32),
            }
        };
        if let Some(exit_code) = status {
            let mut state = self.state.lock().await;
            if state.running {
                state.exit_code.get_or_insert(exit_code);
                state.running = false;
            }
            drop(state);
            self.notify.notify_waiters();
        }
        Ok(())
    }

    async fn is_running(&self) -> bool {
        self.state.lock().await.running
    }

    async fn readers_done(&self) -> bool {
        self.state.lock().await.open_readers == 0
    }

    async fn wait_for_readers(&self, max_wait: Duration) {
        let deadline = Instant::now() + max_wait;
        loop {
            if self.readers_done().await {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
            tokio::pin!(sleep);
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = &mut sleep => return,
            }
        }
    }

    async fn snapshot_since_cursor(
        &self,
        advance_cursor: bool,
        wall_time: Duration,
    ) -> UnifiedExecSnapshot {
        let mut state = self.state.lock().await;
        let output = state.output.since(state.output_cursor);
        let stdout = state.stdout.since(state.stdout_cursor);
        let stderr = state.stderr.since(state.stderr_cursor);
        if advance_cursor {
            state.output_cursor = state.output.total_len();
            state.stdout_cursor = state.stdout.total_len();
            state.stderr_cursor = state.stderr.total_len();
        }
        let original_token_count = approx_token_count(&output);
        UnifiedExecSnapshot {
            session_id: self.session_id,
            output,
            stdout,
            stderr,
            exit_code: state.exit_code,
            running: state.running,
            timed_out: state.timed_out,
            output_truncated: state.output.truncated(),
            stdout_truncated: state.stdout.truncated(),
            stderr_truncated: state.stderr.truncated(),
            wall_time,
            chunk_id: generate_chunk_id(),
            original_token_count,
            max_output_tokens: None,
        }
    }

    async fn terminate(&self, exit_code: i32) {
        {
            let mut child = self.child.lock().await;
            match &mut *child {
                ManagedChild::Pipe {
                    child,
                    process_group,
                } => {
                    terminate_std_child_tree(child, *process_group, Duration::from_secs(2)).await;
                }
                ManagedChild::Pty(child) => {
                    let _ = child.kill();
                }
            }
        }
        let mut state = self.state.lock().await;
        if state.running {
            state.running = false;
            state.exit_code = Some(exit_code);
        }
        drop(state);
        self.notify.notify_waiters();
    }

    async fn terminate_as_timeout(&self) {
        {
            let mut child = self.child.lock().await;
            match &mut *child {
                ManagedChild::Pipe {
                    child,
                    process_group,
                } => {
                    terminate_std_child_tree(child, *process_group, Duration::from_secs(2)).await;
                }
                ManagedChild::Pty(child) => {
                    let _ = child.kill();
                }
            }
        }
        let mut state = self.state.lock().await;
        state.running = false;
        state.timed_out = true;
        state.exit_code = Some(124);
        drop(state);
        self.notify.notify_waiters();
    }

    fn terminate_blocking(&self, exit_code: i32) {
        {
            let mut child = self.child.blocking_lock();
            match &mut *child {
                ManagedChild::Pipe {
                    child,
                    process_group,
                } => {
                    terminate_std_child_tree_blocking(
                        child,
                        *process_group,
                        Duration::from_secs(2),
                    );
                }
                ManagedChild::Pty(child) => {
                    let _ = child.kill();
                }
            }
        }
        let mut state = self.state.blocking_lock();
        if state.running {
            state.running = false;
            state.exit_code = Some(exit_code);
        }
        drop(state);
        self.notify.notify_waiters();
    }

    fn emit_waiting(&self) {
        if let Ok(state) = self.state.try_lock() {
            if let Some(emitter) = state.emitter.as_ref() {
                emitter.emit_command_waiting(&state.tool_name, &state.call_id, self.session_id);
            }
        }
    }

    fn emit_terminal_interaction(&self, stdin: &str) {
        if let Ok(state) = self.state.try_lock() {
            if let Some(emitter) = state.emitter.as_ref() {
                emitter.emit_terminal_interaction(&state.call_id, self.session_id, stdin);
            }
        }
    }

    async fn emit_end_if_needed(&self) {
        let mut state = self.state.lock().await;
        if state.running || state.end_emitted {
            return;
        }
        state.end_emitted = true;
        let emitter = state.emitter.clone();
        let tool_name = state.tool_name.clone();
        let call_id = state.call_id.clone();
        let output = state.output.to_text();
        let exit_code = state.exit_code.unwrap_or(-1);
        let wall_time = self.started_at.elapsed();
        drop(state);
        if let Some(emitter) = emitter {
            emitter.emit_exec_end(ExecEndEvent {
                tool_name: &tool_name,
                call_id: &call_id,
                session_id: self.session_id,
                exit_code,
                wall_time,
                output: &output,
            });
        }
    }
}

#[derive(Debug)]
struct HeadTailStream {
    max_bytes: usize,
    head_budget: usize,
    tail_budget: usize,
    head: String,
    tail: String,
    total_bytes: usize,
    omitted_bytes: usize,
}

impl HeadTailStream {
    fn new(max_bytes: usize) -> Self {
        let head_budget = max_bytes / 2;
        let tail_budget = max_bytes.saturating_sub(head_budget);
        Self {
            max_bytes,
            head_budget,
            tail_budget,
            head: String::new(),
            tail: String::new(),
            total_bytes: 0,
            omitted_bytes: 0,
        }
    }

    fn push(&mut self, text: &str) {
        self.total_bytes = self.total_bytes.saturating_add(text.len());
        if self.max_bytes == 0 {
            self.omitted_bytes = self.omitted_bytes.saturating_add(text.len());
            return;
        }

        let mut rest = text;
        if self.head.len() < self.head_budget {
            let remaining_head = self.head_budget.saturating_sub(self.head.len());
            let (head_part, tail_part) = split_prefix_at_boundary(rest, remaining_head);
            self.head.push_str(head_part);
            rest = tail_part;
        }
        if !rest.is_empty() {
            self.push_tail(rest);
        }
    }

    fn push_tail(&mut self, text: &str) {
        if self.tail_budget == 0 {
            self.omitted_bytes = self.omitted_bytes.saturating_add(text.len());
            return;
        }
        self.tail.push_str(text);
        if self.tail.len() > self.tail_budget {
            let excess = self.tail.len().saturating_sub(self.tail_budget);
            let drop_len = ceil_char_boundary(&self.tail, excess);
            self.tail.drain(..drop_len);
            self.omitted_bytes = self.omitted_bytes.saturating_add(drop_len);
        }
    }

    fn since(&self, cursor: usize) -> String {
        if cursor >= self.total_bytes {
            return String::new();
        }
        if self.omitted_bytes == 0 {
            let mut full = self.head.clone();
            full.push_str(&self.tail);
            return slice_from_boundary(&full, cursor);
        }

        let tail_start = self.total_bytes.saturating_sub(self.tail.len());
        if cursor < self.head.len() {
            let mut out = slice_from_boundary(&self.head, cursor);
            out.push_str(&self.omission_marker());
            out.push_str(&self.tail);
            return out;
        }
        if cursor < tail_start {
            let mut out = self.omission_marker();
            out.push_str(&self.tail);
            return out;
        }
        slice_from_boundary(&self.tail, cursor.saturating_sub(tail_start))
    }

    fn to_text(&self) -> String {
        if self.omitted_bytes == 0 {
            let mut out = self.head.clone();
            out.push_str(&self.tail);
            return out;
        }
        let mut out = self.head.clone();
        out.push_str(&self.omission_marker());
        out.push_str(&self.tail);
        out
    }

    fn omission_marker(&self) -> String {
        format!("\n[... omitted {} bytes ...]\n", self.omitted_bytes)
    }

    fn total_len(&self) -> usize {
        self.total_bytes
    }

    fn truncated(&self) -> bool {
        self.omitted_bytes > 0
    }
}

fn spawn_blocking_reader(
    process: Arc<ManagedProcess>,
    stream: &'static str,
    mut reader: Box<dyn Read + Send>,
) {
    thread::spawn(move || {
        let mut buf = [0u8; READ_CHUNK_SIZE];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => process.append_output_blocking(stream, &buf[..n]),
                Err(_) => break,
            }
        }
        process.reader_done_blocking();
    });
}

fn spawn_exit_watcher(process: Arc<ManagedProcess>) {
    tokio::spawn(async move {
        loop {
            let _ = process.refresh_exit_status().await;
            if !process.is_running().await {
                process
                    .wait_for_readers(Duration::from_millis(WAIT_FOR_READERS_MS))
                    .await;
                process.emit_end_if_needed().await;
                break;
            }
            tokio::time::sleep(Duration::from_millis(EXIT_WATCH_INTERVAL_MS)).await;
        }
    });
}

fn spawn_cancel_watcher(process: Arc<ManagedProcess>, cancel: CancellationToken) {
    tokio::spawn(async move {
        cancel.cancelled().await;
        let _ = process.refresh_exit_status().await;
        if process.is_running().await {
            process.terminate(130).await;
        }
    });
}

fn spawn_timeout_watcher(process: Arc<ManagedProcess>, timeout_ms: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(timeout_ms)).await;
        let _ = process.refresh_exit_status().await;
        if process.is_running().await {
            process.terminate_as_timeout().await;
        }
    });
}

fn apply_unified_exec_env(mut env: HashMap<String, String>) -> HashMap<String, String> {
    for (key, value) in UNIFIED_EXEC_ENV {
        env.insert(key.to_string(), value.to_string());
    }
    prepend_managed_agent_tools_dir(&mut env);
    env
}

fn prepend_managed_agent_tools_dir(env: &mut HashMap<String, String>) {
    let Ok(agent_tools_dir) = std::env::var(AGENT_TOOLS_DIR_ENV) else {
        return;
    };
    let agent_tools_dir = PathBuf::from(agent_tools_dir);
    if !agent_tools_dir_contains_ripgrep(&agent_tools_dir) {
        return;
    }
    prepend_path_dir(env, &agent_tools_dir);
}

fn agent_tools_dir_contains_ripgrep(dir: &Path) -> bool {
    let binary_name = if cfg!(windows) { "rg.exe" } else { "rg" };
    dir.join(binary_name).is_file()
}

fn prepend_path_dir(env: &mut HashMap<String, String>, dir: &Path) {
    let dir = dir.to_string_lossy().to_string();
    if dir.is_empty() {
        return;
    }

    let separator = if cfg!(windows) { ';' } else { ':' };
    let existing_path = env
        .get(PATH_ENV)
        .cloned()
        .or_else(|| std::env::var(PATH_ENV).ok())
        .unwrap_or_default();

    let mut entries = Vec::with_capacity(existing_path.split(separator).count() + 1);
    entries.push(dir.clone());
    entries.extend(
        existing_path
            .split(separator)
            .filter(|entry| !entry.is_empty() && *entry != dir)
            .map(str::to_string),
    );
    env.insert(PATH_ENV.to_string(), entries.join(&separator.to_string()));
}

async fn wait_for_std_child_exit(child: &mut StdChild, max_wait: Duration) {
    let deadline = Instant::now() + max_wait;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => return,
            Ok(None) => tokio::time::sleep(Duration::from_millis(EXIT_WATCH_INTERVAL_MS)).await,
        }
    }
}

async fn terminate_std_child_tree(
    child: &mut StdChild,
    process_group: Option<ProcessGroup>,
    max_wait: Duration,
) {
    signal_process_group(process_group, ProcessSignal::Terminate);
    let _ = child.kill();
    wait_for_std_child_exit(child, max_wait).await;
    signal_process_group(process_group, ProcessSignal::Kill);
    wait_for_std_child_exit(child, Duration::from_millis(EXIT_WATCH_INTERVAL_MS)).await;
}

fn wait_for_std_child_exit_blocking(child: &mut StdChild, max_wait: Duration) {
    let deadline = Instant::now() + max_wait;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => return,
            Ok(None) => thread::sleep(Duration::from_millis(EXIT_WATCH_INTERVAL_MS)),
        }
    }
}

fn terminate_std_child_tree_blocking(
    child: &mut StdChild,
    process_group: Option<ProcessGroup>,
    max_wait: Duration,
) {
    signal_process_group(process_group, ProcessSignal::Terminate);
    let _ = child.kill();
    wait_for_std_child_exit_blocking(child, max_wait);
    signal_process_group(process_group, ProcessSignal::Kill);
    wait_for_std_child_exit_blocking(child, Duration::from_millis(EXIT_WATCH_INTERVAL_MS));
}

#[derive(Clone, Copy)]
enum ProcessSignal {
    Terminate,
    Kill,
}

fn signal_process_group(process_group: Option<ProcessGroup>, signal: ProcessSignal) {
    #[cfg(unix)]
    {
        let Some(process_group) = process_group else {
            return;
        };
        let signal = match signal {
            ProcessSignal::Terminate => libc::SIGTERM,
            ProcessSignal::Kill => libc::SIGKILL,
        };
        unsafe {
            let _ = libc::kill(-process_group.pgid, signal);
        }
    }
    #[cfg(not(unix))]
    let _ = (process_group, signal);
}

fn clamp_yield_time(yield_time_ms: u64, empty_poll: bool) -> u64 {
    if empty_poll {
        yield_time_ms
            .max(MIN_EMPTY_YIELD_TIME_MS)
            .min(MAX_YIELD_TIME_MS)
    } else {
        yield_time_ms.clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
    }
}

fn generate_chunk_id() -> String {
    let mut rng = rand::rng();
    (0..6)
        .map(|_| format!("{:x}", rng.random_range(0..16)))
        .collect()
}

fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4).max(1)
}

fn formatted_truncate_text(text: &str, token_budget: usize) -> String {
    let byte_budget = token_budget.saturating_mul(4);
    if text.len() <= byte_budget {
        return text.to_string();
    }
    if byte_budget == 0 {
        return String::new();
    }
    let end = floor_char_boundary(text, byte_budget);
    format!("{}\n…\n", &text[..end])
}

fn split_prefix_at_boundary(text: &str, max_bytes: usize) -> (&str, &str) {
    if text.len() <= max_bytes {
        return (text, "");
    }
    let end = floor_char_boundary(text, max_bytes);
    text.split_at(end)
}

fn floor_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn slice_from_boundary(text: &str, start: usize) -> String {
    if start >= text.len() {
        return String::new();
    }
    let start = ceil_char_boundary(text, start);
    text[start..].to_string()
}

fn signal_exit_code(status: &std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    let _ = status;
    -1
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::{SpawnProcessRequest, UnifiedExecManager};

    fn request(argv: Vec<String>, cwd: std::path::PathBuf, timeout_ms: u64) -> SpawnProcessRequest {
        SpawnProcessRequest {
            argv,
            cwd,
            env: HashMap::new(),
            tty: false,
            yield_time_ms: 1_000,
            max_output_tokens: None,
            timeout_ms: Some(timeout_ms),
            kill_on_cancel: true,
            call_id: "test-call".to_string(),
            tool_name: "shell".to_string(),
            emitter: None,
            cancel: None,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_background_children_before_they_can_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let late_file = dir.path().join("late.txt");
        let manager = UnifiedExecManager::deterministic_for_tests();

        let snapshot = manager
            .run_to_completion(request(
                vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "(sleep 1; printf bad > late.txt) & sleep 5".to_string(),
                ],
                dir.path().to_path_buf(),
                200,
            ))
            .await
            .expect("run command");

        assert!(snapshot.timed_out, "command should time out");
        tokio::time::sleep(Duration::from_millis(1_400)).await;
        assert!(
            !late_file.exists(),
            "timed-out background child must not survive to write artifacts"
        );
    }
}
