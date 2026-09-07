//! Godot child-process management.
//!
//! A [`GodotProc`] owns one running Godot instance: it spawns the binary, captures stdout and
//! stderr line-by-line into a bounded ring buffer (classifying error lines), polls for exit, and
//! kills on request. `kill_on_drop(true)` is the safety net — dropping the handle (server
//! shutdown, panic, registry eviction) terminates the child even if `kill()` was never called.
//!
//! The server keeps a registry of these keyed by [`RunId`] so `run`/`status`/`stop` can address a
//! specific instance.

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

#[cfg(windows)]
use crate::windows_job::Job;

#[cfg(not(windows))]
struct Job;

#[cfg(not(windows))]
impl Job {
    fn spawn(command: &mut Command) -> Result<(Child, Self)> {
        Ok((command.spawn()?, Self))
    }
}

pub type RunId = u64;

/// Max captured lines retained per run (oldest dropped past this).
const LOG_CAP: usize = 4000;
const LOG_BYTE_CAP: usize = 4 * 1024 * 1024;
const LINE_BYTE_CAP: usize = 64 * 1024;

#[derive(Clone, Debug, serde::Serialize)]
pub struct LogLine {
    pub text: String,
    /// True when the line matches a Godot error pattern (SCRIPT/SHADER/parse/ERROR).
    pub is_error: bool,
}

/// Retain a bounded tail while remembering errors that have scrolled out of it.
#[derive(Default)]
struct CapturedOutput {
    lines: VecDeque<LogLine>,
    bytes: usize,
    errors: usize,
    dropped: usize,
    truncated: usize,
    read_failed: bool,
}

impl CapturedOutput {
    fn push(&mut self, line: LogLine) {
        self.errors += usize::from(line.is_error);
        self.bytes += line.text.len();
        self.lines.push_back(line);
        while self.lines.len() > LOG_CAP || self.bytes > LOG_BYTE_CAP {
            if let Some(line) = self.lines.pop_front() {
                self.bytes -= line.text.len();
                self.dropped += 1;
            }
        }
    }

    fn complete(&self) -> bool {
        !self.read_failed && self.truncated == 0
    }

    fn tail(&self, n: usize) -> Vec<LogLine> {
        let mut lines = Vec::new();
        if self.dropped > 0 || !self.complete() {
            lines.push(LogLine {
                text: format!(
                    "[capture: {} earlier lines omitted, {} oversized lines truncated, {} total errors{}]",
                    self.dropped,
                    self.truncated,
                    self.errors,
                    if self.read_failed { ", output read failed" } else { "" },
                ),
                is_error: !self.complete() || self.errors > 0,
            });
        }
        lines.extend(
            self.lines
                .iter()
                .skip(self.lines.len().saturating_sub(n))
                .cloned(),
        );
        lines
    }
}

/// One running (or exited) Godot process plus its captured output.
pub struct GodotProc {
    /// The resolved argv (binary + args), for status display.
    pub argv: Vec<String>,
    child: Child,
    job: Option<Job>,
    logs: Arc<Mutex<CapturedOutput>>,
    readers: JoinSet<()>,
    /// Cached once the process has been observed to exit.
    exit_code: Option<i32>,
}

impl GodotProc {
    /// Spawn Godot with `args`; begin capturing stdout+stderr into the ring buffer.
    pub async fn spawn(godot_bin: &Path, args: &[String]) -> Result<Self> {
        let mut command = Command::new(godot_bin);
        command
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let (mut child, job) = Job::spawn(&mut command)
            .with_context(|| format!("failed to spawn Godot: {}", godot_bin.display()))?;

        let logs = Arc::new(Mutex::new(CapturedOutput::default()));
        let mut readers = JoinSet::new();
        if let Some(out) = child.stdout.take() {
            readers.spawn(capture(BufReader::new(out), logs.clone()));
        }
        if let Some(err) = child.stderr.take() {
            readers.spawn(capture(BufReader::new(err), logs.clone()));
        }

        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(godot_bin.display().to_string());
        argv.extend(args.iter().cloned());

        Ok(Self {
            argv,
            child,
            job: Some(job),
            logs,
            readers,
            exit_code: None,
        })
    }

    /// Poll for exit, allowing a short, bounded drain for the final output.
    pub async fn poll_exit(&mut self) -> Option<i32> {
        if self.exit_code.is_some() {
            return self.exit_code;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = Some(status.code().unwrap_or(-1));
                self.job.take();
                let _ = tokio::time::timeout(Duration::from_millis(200), async {
                    while self.readers.join_next().await.is_some() {}
                })
                .await;
                self.exit_code
            }
            _ => None,
        }
    }

    /// Kill the process and wait for it to reap, caching the exit code.
    pub async fn kill(&mut self) {
        self.job.take();
        let _ = self.child.kill().await;
        if self.exit_code.is_none() {
            if let Ok(status) = self.child.wait().await {
                self.exit_code = Some(status.code().unwrap_or(-1));
            }
        }
    }

    /// The most recent `tail` captured lines (all of them if `tail` exceeds the buffer).
    pub async fn recent_logs(&self, tail: usize) -> Vec<LogLine> {
        self.logs.lock().await.tail(tail)
    }

    /// Count of error-classified lines captured so far.
    pub async fn error_count(&self) -> usize {
        self.logs.lock().await.errors
    }
}

/// Drain both UTF-8 and non-UTF-8 output without allowing an unbroken line to grow indefinitely.
async fn capture<R>(mut reader: R, logs: Arc<Mutex<CapturedOutput>>)
where
    R: AsyncBufRead + Unpin,
{
    loop {
        match read_line(&mut reader).await {
            Ok(Some((text, truncated))) => {
                let is_error = classify_error(&text);
                let mut logs = logs.lock().await;
                logs.truncated += usize::from(truncated);
                logs.push(LogLine { text, is_error });
            }
            Ok(None) => return,
            Err(error) => {
                let mut logs = logs.lock().await;
                logs.read_failed = true;
                logs.push(LogLine {
                    text: format!("output capture failed: {error}"),
                    is_error: true,
                });
                return;
            }
        }
    }
}

async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<(String, bool)>> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if bytes.is_empty() && !truncated {
                return Ok(None);
            }
            break;
        }
        let newline = chunk.iter().position(|b| *b == b'\n');
        let length = newline.unwrap_or(chunk.len());
        let keep = length.min(LINE_BYTE_CAP.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..keep]);
        truncated |= keep < length;
        reader.consume(length + usize::from(newline.is_some()));
        if newline.is_some() {
            break;
        }
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str(" [line truncated]");
    }
    Ok(Some((text, truncated)))
}

/// Godot prints errors with recognizable prefixes/substrings; flag those for structured surfacing.
fn classify_error(line: &str) -> bool {
    line.contains("SCRIPT ERROR")
        || line.contains("SHADER ERROR")
        || line.contains("Parse Error")
        || line.contains("USER ERROR")
        || line.trim_start().starts_with("ERROR")
}

/// A GDScript stack frame — the indented `at: func (res://file.gd:12)` lines Godot prints beneath
/// an error. They arrive as separate lines, so anything reporting an error re-attaches them.
pub fn is_stack_frame(line: &str) -> bool {
    line.trim_start().starts_with("at: ")
}

/// Result of a one-shot Godot invocation (parse-check, import, export, tests).
pub struct OneshotResult {
    /// Process exit code, or `None` if it was killed (e.g. timed out).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Retained stdout+stderr tail, including a capture notice if output was omitted.
    pub lines: Vec<LogLine>,
    /// All observed errors, including those no longer retained in the tail.
    pub error_count: usize,
    /// False if a read failed or an oversized line could not be fully inspected.
    pub capture_complete: bool,
}

impl OneshotResult {
    /// Lines flagged as errors.
    pub fn error_lines(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|l| l.is_error)
            .map(|l| l.text.as_str())
            .collect()
    }

    /// True when the process exited 0 with no error-classified output.
    pub fn ok(&self) -> bool {
        self.exit_code == Some(0)
            && !self.timed_out
            && self.capture_complete
            && self.error_count == 0
    }
}

/// Run a program to completion with `args`, capturing combined output, bounded by `timeout`. For
/// one-shot commands (parse-check, import, export, tests, API dump) — NOT the long-running game
/// registry. `cwd` matters for the commands that write beside themselves rather than to a path
/// argument (the API dump does exactly that).
pub async fn run_oneshot(
    program: &Path,
    args: &[String],
    timeout: Duration,
    cwd: Option<&Path>,
) -> Result<OneshotResult> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let (mut child, job) =
        Job::spawn(&mut cmd).with_context(|| format!("failed to spawn: {}", program.display()))?;

    let sink = Arc::new(Mutex::new(CapturedOutput::default()));
    // JoinSet aborts its tasks on drop, including when the MCP call is cancelled.
    let mut readers = JoinSet::new();
    if let Some(out) = child.stdout.take() {
        readers.spawn(capture(BufReader::new(out), sink.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.spawn(capture(BufReader::new(err), sink.clone()));
    }

    let mut exit_code = None;
    let completion = async {
        exit_code = child
            .wait()
            .await
            .context("failed to wait for command")?
            .code();
        while let Some(result) = readers.join_next().await {
            result.context("output capture task failed")?;
        }
        Ok::<_, anyhow::Error>(())
    };
    // A descendant can keep inherited pipes open after the direct child exits. The same
    // deadline must cover draining those pipes, as well as waiting for the direct child.
    let timed_out = match tokio::time::timeout(timeout, completion).await {
        Ok(result) => {
            result?;
            false
        }
        Err(_) => {
            let _ = child.start_kill();
            true
        }
    };
    drop(job);
    drop(readers);
    let captured = sink.lock().await;
    Ok(OneshotResult {
        exit_code,
        timed_out,
        lines: captured.tail(LOG_CAP),
        error_count: captured.errors,
        capture_complete: captured.complete(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn classifies_godot_error_lines() {
        assert!(classify_error("SCRIPT ERROR: Invalid call"));
        assert!(classify_error("ERROR: Condition failed"));
        assert!(classify_error("  ERROR: Indented engine error"));
        assert!(classify_error("  Parse Error: Unexpected token"));
        assert!(!classify_error("Godot Engine v4.7.1.stable"));
        assert!(
            !classify_error("an error occurred"),
            "lowercase prose is not a Godot error line"
        );
    }

    #[test]
    fn recognizes_stack_frames() {
        assert!(is_stack_frame("   at: _ready (res://main.gd:12)"));
        assert!(is_stack_frame("at: run (res://tests/run.gd:4)"));
        assert!(!is_stack_frame("SCRIPT ERROR: boom"));
        assert!(!is_stack_frame("nothing at: all"));
    }

    #[tokio::test]
    async fn oneshot_reports_a_spawn_failure_rather_than_hanging() {
        let missing = std::path::Path::new("./definitely-not-a-real-binary-gdkit");
        let res = run_oneshot(missing, &[], Duration::from_secs(5), None).await;
        assert!(res.is_err(), "a missing program must surface as an error");
    }

    fn child_args(name: &str) -> Vec<String> {
        vec![
            "--exact".into(),
            format!("process::tests::{name}"),
            "--ignored".into(),
            "--nocapture".into(),
        ]
    }

    #[test]
    #[ignore = "subprocess fixture invoked by the output-capture regression"]
    fn child_writes_non_utf8() {
        use std::io::Write;
        std::io::stdout()
            .write_all(b"before\n\xff\nERROR: after non-UTF-8 output\n")
            .unwrap();
        std::process::exit(0);
    }

    #[test]
    #[ignore = "subprocess fixture invoked by the timeout regression"]
    fn child_holds_pipes_open() {
        std::thread::sleep(Duration::from_secs(3));
    }

    #[test]
    #[ignore = "subprocess fixture invoked by the timeout regression"]
    #[allow(
        clippy::zombie_processes,
        reason = "This fixture must exit before its descendant closes the inherited pipes"
    )]
    fn child_exits_with_inherited_pipes() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(child_args("child_holds_pipes_open"));
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        // The short-lived descendant deliberately outlives its parent with inherited pipes.
        let _child = command.spawn().unwrap();
    }

    #[tokio::test]
    async fn oneshot_keeps_reading_after_non_utf8_output() {
        let res = run_oneshot(
            &std::env::current_exe().unwrap(),
            &child_args("child_writes_non_utf8"),
            Duration::from_secs(5),
            None,
        )
        .await
        .unwrap();
        assert_eq!(res.exit_code, Some(0));
        assert!(
            !res.ok(),
            "an encoding error must not hide a later engine error"
        );
        assert!(res
            .error_lines()
            .iter()
            .any(|l| l.contains("after non-UTF-8")));
    }

    #[tokio::test]
    async fn oneshot_deadline_includes_inherited_output_pipes() {
        let res = run_oneshot(
            &std::env::current_exe().unwrap(),
            &child_args("child_exits_with_inherited_pipes"),
            Duration::from_secs(1),
            None,
        )
        .await
        .unwrap();
        assert!(
            res.timed_out,
            "open output pipes must respect the command deadline"
        );
        assert_eq!(res.exit_code, Some(0), "the direct child already exited");
        assert!(!res.ok());
    }

    #[tokio::test]
    async fn oneshot_deadline_also_kills_a_running_child() {
        let res = run_oneshot(
            &std::env::current_exe().unwrap(),
            &child_args("child_holds_pipes_open"),
            Duration::from_millis(100),
            None,
        )
        .await
        .unwrap();
        assert!(res.timed_out);
        assert_eq!(res.exit_code, None);
        assert!(!res.ok());
    }

    #[cfg(windows)]
    #[test]
    fn timed_out_descendants_cannot_keep_the_runtime_alive() {
        let started = std::time::Instant::now();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime
            .block_on(run_oneshot(
                &std::env::current_exe().unwrap(),
                &child_args("child_exits_with_inherited_pipes"),
                Duration::from_secs(1),
                None,
            ))
            .unwrap();
        assert!(result.timed_out);
        drop(runtime);
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "a timed-out descendant kept the pipe workers alive for {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn capture_bounds_the_tail_without_forgetting_earlier_errors() {
        let output = format!("ERROR: first\n{}", "progress\n".repeat(LOG_CAP + 10));
        let sink = Arc::new(Mutex::new(CapturedOutput::default()));
        capture(BufReader::new(output.as_bytes()), sink.clone()).await;
        let captured = sink.lock().await;
        assert_eq!(captured.lines.len(), LOG_CAP);
        assert_eq!(captured.errors, 1);
        assert_eq!(captured.dropped, 11);
        assert!(captured.complete());
        assert!(captured.tail(3)[0].text.contains("1 total errors"));
    }

    #[tokio::test]
    async fn capture_bounds_bytes_and_continues_after_oversized_lines() {
        let output = format!(
            "{}\nERROR: after long line\n",
            "x".repeat(LINE_BYTE_CAP * 2)
        );
        let sink = Arc::new(Mutex::new(CapturedOutput::default()));
        capture(BufReader::new(output.as_bytes()), sink.clone()).await;
        let mut captured = sink.lock().await;
        assert_eq!(captured.truncated, 1);
        assert!(!captured.complete());
        assert_eq!(captured.errors, 1);
        assert!(captured.lines[0].text.len() < LINE_BYTE_CAP + 100);
        for _ in 0..100 {
            captured.push(LogLine {
                text: "x".repeat(LINE_BYTE_CAP),
                is_error: false,
            });
        }
        assert!(captured.bytes <= LOG_BYTE_CAP);
        assert!(captured.lines.len() < LOG_CAP);
        assert_eq!(captured.errors, 1);
    }

    /// Spawn the in-repo `hello` fixture headless and confirm we capture its output and observe
    /// a clean exit. Skips gracefully when no Godot binary is available (e.g. CI without Godot).
    #[tokio::test]
    async fn spawns_fixture_and_captures_logs() {
        let Some((godot, _engine_guard)) =
            crate::testutil::godot_or_skip("spawns_fixture_and_captures_logs").await
        else {
            return;
        };
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/hello");
        let args = vec![
            "--path".to_string(),
            fixture.display().to_string(),
            "--headless".to_string(),
        ];

        let mut proc = GodotProc::spawn(&godot, &args).await.expect("spawn Godot");

        // Poll for exit up to ~15s (Godot boots in ~1-2s, then the fixture quits itself).
        let mut exit = None;
        for _ in 0..300 {
            if let Some(code) = proc.poll_exit().await {
                exit = Some(code);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let logs = proc.recent_logs(2000).await;
        let joined: String = logs
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            exit == Some(0),
            "fixture did not exit within 15s; captured logs:\n{joined}"
        );
        assert!(
            joined.contains("FIXTURE-READY"),
            "expected FIXTURE-READY in captured output; got:\n{joined}"
        );
    }

    /// `--check-only` on the broken fixture must be reported as a failure with a parse error —
    /// this is the "green tests, dead game" net.
    #[tokio::test]
    async fn check_only_detects_parse_error() {
        let Some((godot, _engine_guard)) =
            crate::testutil::godot_or_skip("check_only_detects_parse_error").await
        else {
            return;
        };
        let bad = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/bad");
        let args = vec![
            "--headless".to_string(),
            "--path".to_string(),
            bad.display().to_string(),
            "--check-only".to_string(),
            "--script".to_string(),
            "res://bad.gd".to_string(),
        ];
        let res = run_oneshot(&godot, &args, Duration::from_secs(30), None)
            .await
            .expect("run Godot");

        assert!(
            !res.ok(),
            "expected the broken script to be reported as not-ok"
        );
        let errs = res.error_lines().join("\n");
        assert!(
            errs.contains("Parse Error") || errs.contains("SCRIPT ERROR"),
            "expected a parse error in captured errors; got:\n{errs}"
        );
    }
}
