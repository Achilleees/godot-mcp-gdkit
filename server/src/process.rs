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

pub type RunId = u64;

/// Max captured lines retained per run (oldest dropped past this).
const LOG_CAP: usize = 4000;

#[derive(Clone, Debug, serde::Serialize)]
pub struct LogLine {
    pub text: String,
    /// True when the line matches a Godot error pattern (SCRIPT/SHADER/parse/ERROR).
    pub is_error: bool,
}

/// One running (or exited) Godot process plus its captured output.
pub struct GodotProc {
    /// The resolved argv (binary + args), for status display.
    pub argv: Vec<String>,
    child: Child,
    logs: Arc<Mutex<VecDeque<LogLine>>>,
    /// Cached once the process has been observed to exit.
    exit_code: Option<i32>,
}

impl GodotProc {
    /// Spawn Godot with `args`; begin capturing stdout+stderr into the ring buffer.
    pub async fn spawn(godot_bin: &Path, args: &[String]) -> Result<Self> {
        let mut child = Command::new(godot_bin)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn Godot: {}", godot_bin.display()))?;

        let logs: Arc<Mutex<VecDeque<LogLine>>> = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(out) = child.stdout.take() {
            spawn_capture(BufReader::new(out), logs.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_capture(BufReader::new(err), logs.clone());
        }

        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(godot_bin.display().to_string());
        argv.extend(args.iter().cloned());

        Ok(Self {
            argv,
            child,
            logs,
            exit_code: None,
        })
    }

    /// Non-blocking exit check. Returns the exit code once the process has finished, else `None`.
    pub fn poll_exit(&mut self) -> Option<i32> {
        if self.exit_code.is_some() {
            return self.exit_code;
        }
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.exit_code = Some(status.code().unwrap_or(-1));
                self.exit_code
            }
            _ => None,
        }
    }

    /// Kill the process and wait for it to reap, caching the exit code.
    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
        if self.exit_code.is_none() {
            if let Ok(status) = self.child.wait().await {
                self.exit_code = Some(status.code().unwrap_or(-1));
            }
        }
    }

    /// The most recent `tail` captured lines (all of them if `tail` exceeds the buffer).
    pub async fn recent_logs(&self, tail: usize) -> Vec<LogLine> {
        let g = self.logs.lock().await;
        let start = g.len().saturating_sub(tail);
        g.iter().skip(start).cloned().collect()
    }

    /// Count of error-classified lines captured so far.
    pub async fn error_count(&self) -> usize {
        self.logs.lock().await.iter().filter(|l| l.is_error).count()
    }
}

/// Spawn a task that drains a reader line-by-line into the shared ring buffer.
fn spawn_capture<R>(reader: R, logs: Arc<Mutex<VecDeque<LogLine>>>)
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let is_error = classify_error(&line);
            let mut g = logs.lock().await;
            if g.len() >= LOG_CAP {
                g.pop_front();
            }
            g.push_back(LogLine {
                text: line,
                is_error,
            });
        }
    });
}

/// Godot prints errors with recognizable prefixes/substrings; flag those for structured surfacing.
fn classify_error(line: &str) -> bool {
    line.contains("SCRIPT ERROR")
        || line.contains("SHADER ERROR")
        || line.contains("Parse Error")
        || line.contains("USER ERROR")
        || line.starts_with("ERROR")
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
    /// All captured stdout+stderr lines, error-classified.
    pub lines: Vec<LogLine>,
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
        self.exit_code == Some(0) && !self.lines.iter().any(|l| l.is_error)
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
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn: {}", program.display()))?;

    let sink: Arc<Mutex<Vec<LogLine>>> = Arc::new(Mutex::new(Vec::new()));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_collect(BufReader::new(out), sink.clone()));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_collect(BufReader::new(err), sink.clone()));
    }

    let (exit_code, timed_out) = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(_)) => (None, false),
        Err(_) => {
            let _ = child.kill().await;
            (None, true)
        }
    };

    // Drain the reader tasks — the pipes are closed now the child has exited/been killed.
    for r in readers {
        let _ = r.await;
    }
    let lines = std::mem::take(&mut *sink.lock().await);
    Ok(OneshotResult {
        exit_code,
        timed_out,
        lines,
    })
}

/// Collect a reader's lines into a Vec (bounded output; no ring buffer). Returns its JoinHandle.
fn spawn_collect<R>(reader: R, sink: Arc<Mutex<Vec<LogLine>>>) -> tokio::task::JoinHandle<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let is_error = classify_error(&line);
            sink.lock().await.push(LogLine {
                text: line,
                is_error,
            });
        }
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
            if let Some(code) = proc.poll_exit() {
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
            exit.is_some(),
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
