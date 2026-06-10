use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct PythonWorker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    launch: PythonWorkerLaunch,
}

#[derive(Clone, Debug)]
struct PythonWorkerLaunch {
    program: PathBuf,
    args: Vec<String>,
    pythonpath: OsString,
    browser_mode: Option<String>,
    extra_env: Vec<(OsString, OsString)>,
}

#[derive(Debug, Serialize)]
struct RunPythonRequest {
    id: String,
    session_id: String,
    cwd: String,
    artifact_dir: String,
    code: String,
    cancel_requested: bool,
    timeout_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    control: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct RunPythonResponse {
    pub id: String,
    pub ok: bool,
    pub text: String,
    pub error: Option<String>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub outputs: Vec<Value>,
    #[serde(default)]
    pub artifacts: Vec<Value>,
    #[serde(default)]
    pub images: Vec<Value>,
    #[serde(default)]
    pub browser_events: Vec<Value>,
    #[serde(default)]
    pub browser_harness_available: bool,
    #[serde(default)]
    pub browser_harness_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct PythonWorkerEvent {
    pub id: String,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

impl PythonWorker {
    pub fn start() -> Result<Self> {
        Self::start_with_browser_mode(None)
    }

    pub fn start_with_browser_mode(browser_mode: Option<&str>) -> Result<Self> {
        Self::start_with_browser_mode_and_env(browser_mode, std::iter::empty::<(&str, &str)>())
    }

    pub fn start_with_browser_mode_and_env<I, K, V>(
        browser_mode: Option<&str>,
        extra_env: I,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let cwd = std::env::current_dir()?;
        let mut paths = Vec::new();
        if let Some(path) = installed_python_path() {
            paths.push(path);
        }
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?;
        let workspace_python = repo_root.join("python");
        if workspace_python.exists() {
            paths.push(workspace_python);
        }
        let workspace_src = repo_root.join("src");
        if workspace_src.exists() {
            paths.push(workspace_src);
        }
        let cwd_src = cwd.join("src");
        if cwd_src.exists() {
            paths.push(cwd_src);
        }
        if let Some(path) = std::env::var_os("BROWSER_HARNESS_SRC") {
            paths.push(path.into());
        }
        if let Some(path) = std::env::var_os("PYTHONPATH") {
            paths.extend(std::env::split_paths(&path));
        }
        let pythonpath = std::env::join_paths(paths)?;
        let extra_env = extra_env
            .into_iter()
            .map(|(key, value)| (key.as_ref().to_os_string(), value.as_ref().to_os_string()))
            .collect::<Vec<_>>();
        Self::start_with_default_runtime(pythonpath, browser_mode, &extra_env)
    }

    fn start_with_default_runtime(
        pythonpath: impl AsRef<OsStr>,
        browser_mode: Option<&str>,
        extra_env: &[(OsString, OsString)],
    ) -> Result<Self> {
        if let Some(python) =
            std::env::var_os("BROWSER_USE_PYTHON").filter(|value| !value.is_empty())
        {
            return Self::start_with_program_args(
                Path::new(&python),
                &[],
                pythonpath.as_ref(),
                browser_mode,
                extra_env,
            )
            .with_context(|| {
                format!(
                    "start python worker with BROWSER_USE_PYTHON={}",
                    python.to_string_lossy()
                )
            });
        }
        if std::env::var_os("LLM_BROWSER_PYTHON_WORKER_DIRECT").is_none() {
            let uv = PathBuf::from("uv");
            let args = [
                "run",
                "--quiet",
                "--with",
                "cdp-use==1.4.5",
                "--with",
                "fetch-use==0.4.0",
                "--with",
                "pillow==12.2.0",
                "python",
            ];
            if let Ok(worker) = Self::start_with_program_args(
                &uv,
                &args,
                pythonpath.as_ref(),
                browser_mode,
                extra_env,
            ) {
                return Ok(worker);
            }
        }
        if let Ok(worker) = Self::start_with_program_args(
            Path::new("python3"),
            &[],
            pythonpath.as_ref(),
            browser_mode,
            extra_env,
        ) {
            return Ok(worker);
        }
        Self::start_with_program_args(
            Path::new("python"),
            &[],
            pythonpath.as_ref(),
            browser_mode,
            extra_env,
        )
    }

    pub fn start_with_pythonpath(
        python: impl AsRef<Path>,
        pythonpath: impl AsRef<OsStr>,
    ) -> Result<Self> {
        Self::start_with_program_args(python.as_ref(), &[], pythonpath.as_ref(), None, &[])
    }

    fn start_with_program_args(
        program: &Path,
        args: &[&str],
        pythonpath: &OsStr,
        browser_mode: Option<&str>,
        extra_env: &[(OsString, OsString)],
    ) -> Result<Self> {
        let launch = PythonWorkerLaunch {
            program: program.to_path_buf(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            pythonpath: pythonpath.to_os_string(),
            browser_mode: browser_mode.map(str::to_string),
            extra_env: extra_env.to_vec(),
        };
        let (child, stdin, stdout) = spawn_python_worker(&launch)?;
        Ok(Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            launch,
        })
    }

    fn restart(&mut self) -> Result<()> {
        kill_worker_child(&mut self.child);
        let (child, stdin, stdout) = spawn_python_worker(&self.launch)?;
        self.child = child;
        self.stdin = stdin;
        self.stdout = stdout;
        Ok(())
    }

    fn read_response_line(
        &mut self,
        request_id: &str,
        timeout_seconds: Option<f64>,
        deadline: Option<Instant>,
    ) -> Result<Option<String>> {
        let Some(deadline) = deadline else {
            let mut response = String::new();
            let bytes = self.stdout.read_line(&mut response)?;
            if bytes == 0 {
                bail!("python worker exited before responding");
            }
            return Ok(Some(response));
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }

        let (tx, rx) = mpsc::channel();
        let child = &mut self.child;
        let stdout = &mut self.stdout;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let mut response = String::new();
                let read = stdout
                    .read_line(&mut response)
                    .map(|bytes| (bytes, response));
                let _ = tx.send(read);
            });

            match rx.recv_timeout(remaining) {
                Ok(Ok((0, _))) => bail!("python worker exited before responding"),
                Ok(Ok((_, response))) => Ok(Some(response)),
                Ok(Err(err)) => Err(err.into()),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    kill_worker_child(child);
                    let _ = rx.recv_timeout(Duration::from_secs(5));
                    Ok(None)
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("python worker reader disconnected")
                }
            }
        })
        .with_context(|| {
            format!(
                "read python worker response for {request_id} with timeout {:?}",
                timeout_seconds
            )
        })
    }

    pub fn run(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
    ) -> Result<RunPythonResponse> {
        self.run_with_events(session_id, cwd, artifact_dir, code, |_| {})
    }

    pub fn run_with_timeout(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
        timeout_seconds: Option<f64>,
    ) -> Result<RunPythonResponse> {
        self.run_with_events_and_timeout(
            session_id,
            cwd,
            artifact_dir,
            code,
            timeout_seconds,
            |_| {},
        )
    }

    pub fn run_with_events(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
        on_event: impl FnMut(PythonWorkerEvent),
    ) -> Result<RunPythonResponse> {
        self.run_with_events_and_timeout(session_id, cwd, artifact_dir, code, None, on_event)
    }

    pub fn run_with_events_and_timeout(
        &mut self,
        session_id: &str,
        cwd: impl AsRef<Path>,
        artifact_dir: impl AsRef<Path>,
        code: &str,
        timeout_seconds: Option<f64>,
        mut on_event: impl FnMut(PythonWorkerEvent),
    ) -> Result<RunPythonResponse> {
        let request = RunPythonRequest {
            id: format!("py-{}", self.next_id),
            session_id: session_id.to_string(),
            cwd: cwd.as_ref().display().to_string(),
            artifact_dir: artifact_dir.as_ref().display().to_string(),
            code: code.to_string(),
            cancel_requested: false,
            timeout_seconds,
            control: None,
        };
        self.next_id += 1;
        let line = serde_json::to_string(&request)?;
        writeln!(self.stdin, "{line}")?;
        self.stdin.flush()?;

        let deadline = timeout_seconds.map(|seconds| {
            let seconds = seconds.max(0.0);
            let grace = (seconds * 0.1).clamp(1.0, 2.0);
            Instant::now() + Duration::from_secs_f64(seconds + grace)
        });

        loop {
            let Some(response) = self.read_response_line(&request.id, timeout_seconds, deadline)?
            else {
                self.restart()?;
                return Ok(RunPythonResponse {
                    id: request.id.clone(),
                    ok: false,
                    text: String::new(),
                    error: Some(format!(
                        "python tool timed out after {} seconds",
                        timeout_seconds.unwrap_or_default()
                    )),
                    data: Value::Null,
                    outputs: Vec::new(),
                    artifacts: Vec::new(),
                    images: Vec::new(),
                    browser_events: Vec::new(),
                    browser_harness_available: false,
                    browser_harness_error: None,
                });
            };
            let trimmed = response.trim();
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(_) => {
                    if !trimmed.is_empty() {
                        on_event(PythonWorkerEvent {
                            id: request.id.clone(),
                            event: "worker.stdout".to_string(),
                            payload: serde_json::json!({ "text": trimmed }),
                        });
                    }
                    continue;
                }
            };
            if value.get("event").is_some() {
                let event: PythonWorkerEvent =
                    serde_json::from_value(value).context("parse python worker event")?;
                if event.id == request.id {
                    on_event(event);
                }
                continue;
            }
            return serde_json::from_value(value).context("parse python worker response");
        }
    }

    pub fn shutdown_owned_cloud_browser(&mut self) -> Result<Option<Value>> {
        let cwd = std::env::current_dir()?;
        let request = RunPythonRequest {
            id: format!("py-{}", self.next_id),
            session_id: "__worker_control__".to_string(),
            cwd: cwd.display().to_string(),
            artifact_dir: cwd
                .join(".browser-use")
                .join("control-artifacts")
                .display()
                .to_string(),
            code: String::new(),
            cancel_requested: false,
            timeout_seconds: Some(5.0),
            control: Some("shutdown_owned_cloud_browser".to_string()),
        };
        self.next_id += 1;
        let line = serde_json::to_string(&request)?;
        writeln!(self.stdin, "{line}")?;
        self.stdin.flush()?;

        loop {
            let mut response = String::new();
            let bytes = self.stdout.read_line(&mut response)?;
            if bytes == 0 {
                return Ok(None);
            }
            let trimmed = response.trim();
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if value.get("event").is_some() {
                continue;
            }
            return Ok(value.get("data").cloned());
        }
    }
}

fn installed_python_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let release_python = exe_dir.parent()?.join("python");
    if release_python.exists() {
        return Some(release_python);
    }
    let sibling_python = exe_dir.join("python");
    sibling_python.exists().then_some(sibling_python)
}

fn spawn_python_worker(
    launch: &PythonWorkerLaunch,
) -> Result<(Child, ChildStdin, BufReader<ChildStdout>)> {
    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .arg("-m")
        .arg("llm_browser_worker.worker")
        .env("PYTHONUNBUFFERED", "1")
        .env("PYTHONPATH", &launch.pythonpath);
    command.envs(launch.extra_env.iter().cloned());
    if let Some(browser_mode) = launch
        .browser_mode
        .as_deref()
        .filter(|mode| !mode.trim().is_empty())
    {
        command.env("LLM_BROWSER_BROWSER_MODE", browser_mode);
    }
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "start python worker via {} with PYTHONPATH={}",
                launch.program.display(),
                launch.pythonpath.to_string_lossy()
            )
        })?;
    let stdin = child.stdin.take().context("python worker stdin missing")?;
    let stdout = child
        .stdout
        .take()
        .context("python worker stdout missing")?;
    Ok((child, stdin, BufReader::new(stdout)))
}

fn kill_worker_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        let pid = child.id() as libc::pid_t;
        if pid > 0 {
            let _ = libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                eprintln!("timed out waiting for python worker process to exit after kill");
                break;
            }
            Err(_) => break,
        }
    }
}

impl Drop for PythonWorker {
    fn drop(&mut self) {
        kill_worker_child(&mut self.child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn worker_keeps_a_persistent_namespace_per_session() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let first = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "x = 41\nprint('ready')",
        )?;
        assert!(first.ok, "{first:?}");
        assert!(first.text.contains("ready"));

        let second = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "print(x + 1)\nresult = {'value': x + 1}",
        )?;
        assert!(second.ok, "{second:?}");
        assert!(second.text.contains("42"));
        assert_eq!(second.data["value"], 42);
        Ok(())
    }

    #[test]
    fn worker_times_out_snippets_without_losing_namespace() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let response = worker.run_with_timeout(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "x = 7\nimport time\ntime.sleep(5)",
            Some(0.2),
        )?;
        assert!(!response.ok, "{response:?}");
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("timed out"),
            "{response:?}"
        );

        let second = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "print(x)\nresult = {'value': x}",
        )?;
        assert!(second.ok, "{second:?}");
        assert_eq!(second.data["value"], 7);
        Ok(())
    }

    #[test]
    fn worker_hard_times_out_threadpool_shutdown_hang_and_recovers() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;

        let started = Instant::now();
        let response = worker.run_with_timeout(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "import concurrent.futures, time\nwith concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:\n    executor.submit(time.sleep, 5).result()",
            Some(0.2),
        )?;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "hard timeout should not wait for executor shutdown"
        );
        assert!(!response.ok, "{response:?}");
        assert!(
            response
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("timed out"),
            "{response:?}"
        );

        let recovered = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = {'ok': True}",
        )?;
        assert!(recovered.ok, "{recovered:?}");
        assert_eq!(recovered.data["ok"], true);
        Ok(())
    }

    #[test]
    fn worker_host_helpers_collect_outputs_artifacts_and_images() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let input = temp.path().join("input.txt");
        std::fs::write(&input, "hello")?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let response = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "emit_output('chunk')\ncopy_artifact('input.txt', kind='file')\nemit_browser_live_url('https://live.example')\nresult = {'ok': True}",
        )?;
        assert!(response.ok, "{response:?}");
        assert_eq!(response.outputs[0]["text"], "chunk");
        assert_eq!(response.artifacts[0]["kind"], "file");
        assert_eq!(response.browser_events[0]["type"], "browser.live_url");
        Ok(())
    }

    #[test]
    fn worker_tolerates_non_json_stdout_lines() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let package = temp.path().join("llm_browser_worker");
        std::fs::create_dir_all(&package)?;
        std::fs::write(package.join("__init__.py"), "")?;
        std::fs::write(
            package.join("worker.py"),
            r#"
import json
import sys

for line in sys.stdin:
    request = json.loads(line)
    print("cloud startup chatter", flush=True)
    print(json.dumps({
        "id": request["id"],
        "ok": True,
        "text": "done",
        "error": None,
        "data": None,
        "outputs": [],
        "artifacts": [],
        "images": [],
        "browser_events": [],
        "browser_harness_available": True,
        "browser_harness_error": None,
    }), flush=True)
"#,
        )?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", temp.path())?;
        let mut events = Vec::new();
        let response = worker.run_with_events(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = None",
            |event| events.push(event),
        )?;

        assert!(response.ok, "{response:?}");
        assert_eq!(response.text, "done");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, "worker.stdout");
        assert_eq!(events[0].payload["text"], "cloud startup chatter");
        Ok(())
    }

    #[test]
    fn worker_exposes_session_metadata_and_artifact_root_helpers() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let artifact_dir = temp.path().join("artifacts");
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let response = worker.run(
            "s1",
            temp.path(),
            &artifact_dir,
            "result = {'root': artifact_root(), 'metadata': session_metadata()}",
        )?;
        assert!(response.ok, "{response:?}");
        let artifact_dir = artifact_dir.canonicalize()?;
        assert_eq!(response.data["root"], artifact_dir.display().to_string());
        assert_eq!(response.data["metadata"]["session_id"], "s1");
        assert_eq!(
            response.data["metadata"]["artifact_root"],
            artifact_dir.display().to_string()
        );
        Ok(())
    }

    #[test]
    fn worker_streams_host_helper_events_before_final_response() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", repo_root.join("python"))?;
        let mut events = Vec::new();
        let response = worker.run_with_events(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "emit_output('first')\nemit_browser_state(url='https://example.com')\nresult = 'done'",
            |event| events.push(event),
        )?;
        assert!(response.ok, "{response:?}");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "output");
        assert_eq!(events[0].payload["text"], "first");
        assert_eq!(events[1].event, "browser");
        assert_eq!(events[1].payload["type"], "browser.state");
        Ok(())
    }

    #[test]
    fn worker_lazily_ensures_browser_harness_and_emits_current_tab_state() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let fake_harness = temp.path().join("browser_harness");
        std::fs::create_dir_all(&fake_harness)?;
        std::fs::write(fake_harness.join("__init__.py"), "")?;
        std::fs::write(
            fake_harness.join("admin.py"),
            r#"
started = False

def ensure_daemon():
    global started
    started = True

def daemon_alive():
    return started
"#,
        )?;
        std::fs::write(
            fake_harness.join("helpers.py"),
            r#"
__all__ = ["cdp", "goto_url", "current_tab"]

def cdp(method, session_id=None, **params):
    from browser_harness import admin
    if not admin.started:
        raise RuntimeError("daemon was not ensured")
    return {"method": method, "params": params, "session_id": session_id}

def goto_url(url):
    return cdp("Page.navigate", url=url)

def current_tab():
    return {"targetId": "target-1", "url": "https://example.com", "title": "Example Domain"}
"#,
        )?;
        let pythonpath =
            std::env::join_paths([repo_root.join("python"), temp.path().to_path_buf()])?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", pythonpath)?;
        let mut events = Vec::new();
        let response = worker.run_with_events(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = goto_url('https://example.com')",
            |event| events.push(event),
        )?;
        assert!(response.ok, "{response:?}");
        assert!(response.browser_harness_available);
        assert_eq!(response.data["method"], "Page.navigate");
        assert_eq!(response.browser_events[0]["type"], "browser.state");
        assert_eq!(
            response.browser_events[0]["payload"]["url"],
            "https://example.com"
        );
        assert!(events.iter().any(|event| event.event == "browser"
            && event.payload["payload"]["title"] == "Example Domain"));
        Ok(())
    }

    #[test]
    fn worker_can_index_browser_harness_download_outputs() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let fake_harness = temp.path().join("browser_harness");
        std::fs::create_dir_all(&fake_harness)?;
        std::fs::write(fake_harness.join("__init__.py"), "")?;
        std::fs::write(
            fake_harness.join("admin.py"),
            r#"
started = False

def ensure_daemon():
    global started
    started = True

def daemon_alive():
    return started
"#,
        )?;
        std::fs::write(
            fake_harness.join("helpers.py"),
            r#"
import os
from pathlib import Path

__all__ = ["cdp", "download_file", "current_tab"]

def cdp(method, session_id=None, **params):
    from browser_harness import admin
    if not admin.started:
        raise RuntimeError("daemon was not ensured")
    return {"method": method, "params": params, "session_id": session_id}

def download_file(name):
    cdp("Browser.downloadWillBegin", suggestedFilename=name)
    path = Path(os.getcwd()) / name
    path.write_text("downloaded", encoding="utf-8")
    return str(path)

def current_tab():
    return {"targetId": "target-download", "url": "https://example.com/download", "title": "Download"}
"#,
        )?;
        let pythonpath =
            std::env::join_paths([repo_root.join("python"), temp.path().to_path_buf()])?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", pythonpath)?;
        let response = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "path = download_file('report.csv')\ncopy_artifact(path, kind='file')\nresult = {'download': path}",
        )?;
        assert!(response.ok, "{response:?}");
        assert!(response.browser_harness_available);
        assert_eq!(response.artifacts[0]["kind"], "file");
        assert_eq!(response.artifacts[0]["bytes"], 10);
        assert_eq!(
            response.browser_events[0]["payload"]["target_id"],
            "target-download"
        );
        Ok(())
    }

    #[test]
    fn worker_refreshes_browser_identity_from_harness_each_call() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let fake_harness = temp.path().join("browser_harness");
        std::fs::create_dir_all(&fake_harness)?;
        std::fs::write(fake_harness.join("__init__.py"), "")?;
        std::fs::write(
            fake_harness.join("admin.py"),
            r#"
started = False

def ensure_daemon():
    global started
    started = True

def daemon_alive():
    return started
"#,
        )?;
        std::fs::write(
            fake_harness.join("helpers.py"),
            r#"
__all__ = ["cdp", "goto_url", "current_tab"]
count = 0

def cdp(method, session_id=None, **params):
    from browser_harness import admin
    if not admin.started:
        raise RuntimeError("daemon was not ensured")
    return {"method": method, "params": params, "session_id": session_id}

def goto_url(url):
    return cdp("Page.navigate", url=url)

def current_tab():
    global count
    count += 1
    return {"targetId": f"target-{count}", "url": f"https://example.com/{count}", "title": f"Page {count}"}
"#,
        )?;
        let pythonpath =
            std::env::join_paths([repo_root.join("python"), temp.path().to_path_buf()])?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", pythonpath)?;
        let first = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = goto_url('https://example.com/first')",
        )?;
        let second = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = goto_url('https://example.com/second')",
        )?;
        assert!(first.ok, "{first:?}");
        assert!(second.ok, "{second:?}");
        assert_eq!(first.browser_events[0]["payload"]["target_id"], "target-1");
        assert_eq!(second.browser_events[0]["payload"]["target_id"], "target-2");
        assert_eq!(
            second.browser_events[0]["payload"]["url"],
            "https://example.com/2"
        );
        Ok(())
    }

    #[test]
    fn worker_emits_explicit_browser_identity_changes() -> Result<()> {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("repo root")?
            .to_path_buf();
        let temp = tempfile::tempdir()?;
        let fake_harness = temp.path().join("browser_harness");
        std::fs::create_dir_all(&fake_harness)?;
        std::fs::write(fake_harness.join("__init__.py"), "")?;
        std::fs::write(
            fake_harness.join("admin.py"),
            r#"
started = False

def ensure_daemon():
    global started
    started = True

def daemon_alive():
    return started
"#,
        )?;
        std::fs::write(
            fake_harness.join("helpers.py"),
            r#"
__all__ = ["cdp", "goto_url", "current_tab", "set_identity"]
target_id = "target-1"
session_id = "session-1"
url = "https://example.com/one"
title = "One"

def _send(request):
    if request.get("meta") == "connection_status":
        return {
            "target_id": target_id,
            "session_id": session_id,
            "page": {"targetId": target_id, "url": url, "title": title},
        }
    return {}

def cdp(method, session_id=None, **params):
    from browser_harness import admin
    if not admin.started:
        raise RuntimeError("daemon was not ensured")
    return {"method": method, "params": params, "session_id": session_id}

def goto_url(next_url):
    cdp("Page.navigate", url=next_url)
    return {"ok": True}

def current_tab():
    return {"targetId": target_id, "url": url, "title": title}

def set_identity(next_target_id, next_session_id, next_url, next_title):
    global target_id, session_id, url, title
    target_id = next_target_id
    session_id = next_session_id
    url = next_url
    title = next_title
"#,
        )?;
        let pythonpath =
            std::env::join_paths([repo_root.join("python"), temp.path().to_path_buf()])?;
        let mut worker = PythonWorker::start_with_pythonpath("python3", pythonpath)?;

        let first = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "result = goto_url('https://example.com/one')",
        )?;
        assert!(first.ok, "{first:?}");
        assert!(first
            .browser_events
            .iter()
            .any(|event| event["type"] == "browser.connected"));

        let second = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "set_identity('target-1', 'session-2', 'https://example.com/two', 'Two')",
        )?;
        assert!(second.ok, "{second:?}");
        assert!(second
            .browser_events
            .iter()
            .any(|event| event["type"] == "browser.reconnected"
                && event["payload"]["previous_session_id"] == "session-1"
                && event["payload"]["stale_object_ids"] == true));

        let third = worker.run(
            "s1",
            temp.path(),
            temp.path().join("artifacts"),
            "set_identity('target-2', 'session-3', 'https://example.com/three', 'Three')",
        )?;
        assert!(third.ok, "{third:?}");
        assert!(third
            .browser_events
            .iter()
            .any(|event| event["type"] == "browser.target_changed"
                && event["payload"]["previous_target_id"] == "target-1"
                && event["payload"]["stale_object_ids"] == true));
        Ok(())
    }
}
