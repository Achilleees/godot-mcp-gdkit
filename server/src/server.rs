//! The gdkit MCP server: tool definitions + shared session state.
//!
//! The handler is cloned by rmcp per request, so all mutable state lives behind `Arc`. `cfg` holds
//! resolved discovery and is swappable — the `config` tool persists a setting and re-resolves in
//! place, so a change takes effect without restarting the server. `session` holds the registry of
//! running Godot processes keyed by `RunId`; `kill_on_drop` on each `GodotProc` means clearing the
//! registry (or dropping the server) terminates the children. `api` caches the parsed API
//! reference for the life of the process, loaded lazily on the first `docs` call.
//!
//! Tool bodies stay thin: argument checking, then a call into the module that owns the logic
//! (`testrun`, `export`, `docs`, `settings`), then rendering. Anything worth a unit test lives in
//! those modules, where it can be tested without an MCP client.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ContentBlock, ErrorData as McpError, Implementation, ServerCapabilities,
        ServerInfo,
    },
    schemars, tool, tool_handler, tool_router, ServerHandler,
};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::docs::{self, ApiIndex};
use crate::export::{self, ExportMode};
use crate::process::{run_oneshot, GodotProc, RunId};
use crate::settings::{self, Settings};
use crate::storage::ScratchDir;
use crate::testrun::{self, PlanSource};

/// Default ceilings for one-shot tools, leaving room within the 10-minute client timeout.
const CHECK_TIMEOUT: u64 = 30;
const VERSION_TIMEOUT: u64 = 30;
const TESTS_TIMEOUT: u64 = 300;
const IMPORT_TIMEOUT: u64 = 540;
const EXPORT_TIMEOUT: u64 = 540;
const DUMP_TIMEOUT: u64 = 300;

/// Live session state shared across tool calls.
#[derive(Default)]
pub struct Session {
    pub runs: HashMap<RunId, GodotProc>,
    pub next_id: RunId,
}

#[derive(PartialEq, Eq)]
struct EngineKey {
    path: PathBuf,
    data_dir: PathBuf,
    modified: std::time::SystemTime,
    len: u64,
}

struct CachedApi {
    engine: EngineKey,
    index: Arc<ApiIndex>,
}

#[derive(Clone)]
pub struct GdkitServer {
    tool_router: ToolRouter<Self>,
    cfg: Arc<Mutex<Arc<Config>>>,
    session: Arc<Mutex<Session>>,
    api: Arc<Mutex<Option<CachedApi>>>,
    /// Editor operations share Godot's per-user cache and may write the same project files.
    editor: Arc<Mutex<()>>,
}

// ---- tool argument schemas ---------------------------------------------------------------

#[derive(Deserialize, schemars::JsonSchema)]
struct PingArgs {
    /// Optional text echoed back in the reply, to confirm argument passing works.
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RunArgs {
    /// Scene to run (a `res://` path or project-relative `.tscn`). Omit to run the project's
    /// configured main scene.
    #[serde(default)]
    scene: Option<String>,
    /// Run without a window: no rendering, so screenshots are impossible. Default false.
    #[serde(default)]
    headless: bool,
    /// Extra CLI arguments passed through to Godot verbatim.
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct StatusArgs {
    /// Which run to report. Omit to report every known run.
    #[serde(default)]
    run_id: Option<RunId>,
    /// Max recent log lines to include per run (default 40).
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct StopArgs {
    /// The run to stop. Omit to stop ALL runs.
    #[serde(default)]
    run_id: Option<RunId>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct CheckScriptArgs {
    /// Script to parse-check — a `res://` path or a path relative to the project root.
    script: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RunTestsArgs {
    /// Explicit command (program + args) to run instead of the configured/detected one. Supports
    /// the `{godot}` and `{project}` placeholders.
    #[serde(default)]
    command: Option<Vec<String>>,
    /// Kill the suite after this many seconds (default 300).
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Max output lines to show when the suite prints no summary and no errors (default 40).
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ReimportArgs {
    /// Kill the import after this many seconds (default 540 — a first import of a large project
    /// is slow).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct ExportArgs {
    /// Preset name from `export_presets.cfg`. Omit when the project has exactly one preset, or
    /// when a default is persisted.
    #[serde(default)]
    preset: Option<String>,
    /// Output path (absolute, or relative to the project dir). Defaults to the persisted setting,
    /// then to the preset's own export path.
    #[serde(default)]
    output: Option<String>,
    /// `release` (default), `debug`, or `pack` (data only — a .pck/.zip, no executable).
    #[serde(default)]
    mode: Option<String>,
    /// Only list the available presets; export nothing.
    #[serde(default)]
    list: bool,
    /// Kill the export after this many seconds (default 540).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
struct DocsArgs {
    /// A class (`Sprite2D`), a member (`Sprite2D.frame_coords`), or any substring to search for.
    /// Omit for a summary of what the reference contains.
    #[serde(default)]
    query: Option<String>,
    /// Re-dump the reference from the engine even if a cached copy exists.
    #[serde(default)]
    refresh: bool,
}

#[derive(Default, Deserialize, schemars::JsonSchema)]
struct ConfigArgs {
    /// Absolute path to the Godot executable. Pass an empty string to clear it.
    #[serde(default)]
    godot_bin: Option<String>,
    /// Absolute path to the directory holding `project.godot`. Empty string clears it.
    #[serde(default)]
    project_dir: Option<String>,
    /// Command that runs the test suite (program + args), supporting `{godot}` and `{project}`.
    /// Pass an empty list to clear it.
    #[serde(default)]
    test_command: Option<Vec<String>>,
    /// Default export preset name. Empty string clears it.
    #[serde(default)]
    export_preset: Option<String>,
    /// Default export output path. Empty string clears it.
    #[serde(default)]
    export_output: Option<String>,
    /// Wipe every persisted setting, returning to pure discovery.
    #[serde(default)]
    clear: bool,
}

// ---- tools -------------------------------------------------------------------------------

#[tool_router]
impl GdkitServer {
    pub fn new(cfg: Config) -> Self {
        Self {
            tool_router: Self::tool_router(),
            cfg: Arc::new(Mutex::new(Arc::new(cfg))),
            session: Arc::new(Mutex::new(Session {
                runs: HashMap::new(),
                next_id: 1,
            })),
            api: Arc::new(Mutex::new(None)),
            editor: Arc::new(Mutex::new(())),
        }
    }

    /// Current configuration. Cheap to clone, and never held across an await that could see it
    /// swapped by the `config` tool.
    async fn cfg(&self) -> Arc<Config> {
        self.cfg.lock().await.clone()
    }

    /// Kill any still-running Godot processes. Called on server shutdown.
    pub async fn shutdown(&self) {
        let mut sess = self.session.lock().await;
        for (id, mut proc) in sess.runs.drain() {
            tracing::info!("killing run_id={id} on shutdown");
            proc.kill().await;
        }
    }

    /// The parsed API reference, dumping it from the engine on first use (or on `refresh`).
    async fn api_index(&self, refresh: bool) -> Result<Arc<ApiIndex>, McpError> {
        let cfg = self.cfg().await;
        let godot = need_godot(&cfg)?;
        let meta = std::fs::metadata(&godot).map_err(|e| {
            McpError::internal_error(format!("cannot inspect {}: {e}", godot.display()), None)
        })?;
        let engine = EngineKey {
            path: godot.clone(),
            data_dir: cfg.data_dir.clone(),
            modified: meta
                .modified()
                .map_err(|e| McpError::internal_error(e.to_string(), None))?,
            len: meta.len(),
        };
        // Serialize the load, not just access to the final pointer: concurrent callers must
        // share a dump. Match the resolved binary so a config change cannot serve stale docs.
        let mut cached = self.api.lock().await;
        if !refresh {
            if let Some(api) = cached.as_ref().filter(|api| api.engine == engine) {
                return Ok(api.index.clone());
            }
        }

        // Key the cache by the engine's own version string: an engine upgrade must not serve the
        // previous version's API.
        let ver = run_oneshot(
            &godot,
            &["--version".to_string()],
            Duration::from_secs(VERSION_TIMEOUT),
            None,
        )
        .await
        .map_err(|e| McpError::internal_error(format!("godot --version failed: {e}"), None))?;
        if !ver.ok() {
            return Err(McpError::internal_error(
                format!(
                    "godot --version failed (exit={:?}, timed_out={}):\n{}",
                    ver.exit_code,
                    ver.timed_out,
                    lines_tail(&ver, 12)
                ),
                None,
            ));
        }
        let lines: Vec<String> = ver.lines.iter().map(|l| l.text.clone()).collect();
        let version = docs::parse_version_output(&lines).ok_or_else(|| {
            McpError::internal_error(
                format!(
                    "could not read the Godot version from: {}",
                    lines.join(" | ")
                ),
                None,
            )
        })?;

        let dir = docs::cache_dir(&cfg.data_dir, &version);
        let path = docs::cache_path(&cfg.data_dir, &version);
        let idx = if refresh || !path.is_file() {
            let _editor = self.editor.lock().await;
            let scratch = ScratchDir::new(&dir).map_err(|e| {
                McpError::internal_error(format!("cannot create {}: {e}", dir.display()), None)
            })?;
            // The dump has no output-path flag: it writes into the working directory.
            let res = run_oneshot(
                &godot,
                &[
                    "--headless".to_string(),
                    "--dump-extension-api-with-docs".to_string(),
                ],
                Duration::from_secs(DUMP_TIMEOUT),
                Some(scratch.path()),
            )
            .await
            .map_err(|e| McpError::internal_error(format!("API dump failed to run: {e}"), None))?;
            let pending = scratch.path().join(docs::DUMP_FILE);
            if !res.ok() || !pending.is_file() {
                return Err(McpError::internal_error(
                    format!(
                        "the engine produced no valid {} (exit={:?}{}). Output:\n{}",
                        docs::DUMP_FILE,
                        res.exit_code,
                        if res.timed_out { ", timed out" } else { "" },
                        lines_tail(&res, 12)
                    ),
                    None,
                ));
            }
            let idx = load_api(pending.clone()).await?;
            std::fs::rename(pending, &path).map_err(|e| {
                McpError::internal_error(format!("cannot save {}: {e}", path.display()), None)
            })?;
            tracing::info!("cached Godot {version} API reference at {}", path.display());
            idx
        } else {
            load_api(path).await?
        };

        let idx = Arc::new(idx);
        *cached = Some(CachedApi {
            engine,
            index: idx.clone(),
        });
        Ok(idx)
    }

    #[tool(
        description = "Liveness check for the gdkit MCP server. Returns \"pong\" (optionally echoing a supplied message). Use it to confirm the server loaded and is reachable."
    )]
    async fn ping(&self, Parameters(a): Parameters<PingArgs>) -> Result<CallToolResult, McpError> {
        let reply = match a.message {
            Some(m) if !m.is_empty() => format!("pong: {m}"),
            _ => "pong".to_string(),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(reply)]))
    }

    #[tool(
        description = "Launch the Godot project (or a specific scene) as a child process; returns a run_id. Non-blocking: poll `status` for logs/exit and `stop` to kill it. headless=true disables rendering (no screenshots possible)."
    )]
    async fn run(&self, Parameters(a): Parameters<RunArgs>) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg().await;
        let godot = need_godot(&cfg)?;
        let project = need_project(&cfg)?;

        let mut args: Vec<String> = vec!["--path".into(), project.display().to_string()];
        if a.headless {
            args.push("--headless".into());
        }
        if let Some(scene) = &a.scene {
            args.push(scene.clone());
        }
        args.extend(a.args.iter().cloned());

        let mut sess = self.session.lock().await;
        let run_id = sess.next_id;
        sess.next_id += 1;
        let proc = GodotProc::spawn(&godot, &args)
            .await
            .map_err(|e| McpError::internal_error(format!("spawn failed: {e}"), None))?;
        let argv = proc.argv.join(" ");
        sess.runs.insert(run_id, proc);

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "started run_id={run_id} (headless={})\nargv: {argv}\nPoll `status` for logs/exit; `stop` to kill.",
            a.headless
        ))]))
    }

    #[tool(
        description = "Report the state (running/exited), exit code, error count, and recent log tail of a run (or all runs). Error-classified lines are prefixed [ERR]."
    )]
    async fn status(
        &self,
        Parameters(a): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tail = a.tail.unwrap_or(40);
        let mut sess = self.session.lock().await;

        let ids: Vec<RunId> = match a.run_id {
            Some(id) => vec![id],
            None => {
                let mut v: Vec<RunId> = sess.runs.keys().copied().collect();
                v.sort_unstable();
                v
            }
        };
        if ids.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                "no runs".to_string(),
            )]));
        }

        let mut out = String::new();
        for id in ids {
            match sess.runs.get_mut(&id) {
                Some(proc) => {
                    let exit = proc.poll_exit().await;
                    let state = if exit.is_none() { "running" } else { "exited" };
                    let errors = proc.error_count().await;
                    let logs = proc.recent_logs(tail).await;
                    out.push_str(&format!(
                        "run_id={id} state={state} exit={exit:?} errors={errors}\n"
                    ));
                    for l in logs {
                        out.push_str(&format!(
                            "  {}{}\n",
                            if l.is_error { "[ERR] " } else { "" },
                            l.text
                        ));
                    }
                }
                None => out.push_str(&format!("run_id={id}: not found\n")),
            }
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
    }

    #[tool(
        description = "Stop (kill) a running Godot process by run_id, or all runs if run_id is omitted."
    )]
    async fn stop(&self, Parameters(a): Parameters<StopArgs>) -> Result<CallToolResult, McpError> {
        let mut sess = self.session.lock().await;
        let ids: Vec<RunId> = match a.run_id {
            Some(id) => vec![id],
            None => sess.runs.keys().copied().collect(),
        };
        let mut killed: Vec<RunId> = Vec::new();
        for id in ids {
            if let Some(mut proc) = sess.runs.remove(&id) {
                proc.kill().await;
                killed.push(id);
            }
        }
        killed.sort_unstable();
        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "stopped: {killed:?}"
        ))]))
    }

    #[tool(
        description = "Headless parse-check of a single GDScript file (godot --check-only). Catches syntax/parse errors WITHOUT running the game — the net for 'green tests, dead game' Variant-inference bugs. Returns OK, or a tool error with the parse errors."
    )]
    async fn check_script(
        &self,
        Parameters(a): Parameters<CheckScriptArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg().await;
        let godot = need_godot(&cfg)?;
        let project = need_project(&cfg)?;

        let args = vec![
            "--headless".to_string(),
            "--path".to_string(),
            project.display().to_string(),
            "--check-only".to_string(),
            "--script".to_string(),
            a.script.clone(),
        ];
        let res = run_oneshot(&godot, &args, Duration::from_secs(CHECK_TIMEOUT), None)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("check_script failed to run: {e}"), None)
            })?;

        if res.ok() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "OK — {} parsed with no errors",
                a.script
            ))]));
        }

        let mut out = format!(
            "PARSE FAILED — {} (exit={:?}{})\n",
            a.script,
            res.exit_code,
            if res.timed_out { ", timed out" } else { "" }
        );
        let errors = res.error_lines();
        if errors.is_empty() {
            out.push_str("output tail:\n");
            out.push_str(&lines_tail(&res, 12));
        } else {
            out.push_str("errors:\n");
            for e in &errors {
                out.push_str(&format!("  {e}\n"));
            }
        }
        Ok(CallToolResult::error(vec![ContentBlock::text(out)]))
    }

    #[tool(
        description = "Run the project's test suite and return a structured result: verdict, the suite's own summary lines, and every engine error with its GDScript stack frames. Framework-agnostic — uses the `command` argument, else the persisted test_command, else detects gdUnit4 / GUT / a tests/run_tests.gd-style harness by the runner file actually present in the project."
    )]
    async fn run_tests(
        &self,
        Parameters(a): Parameters<RunTestsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg().await;
        let project = need_project(&cfg)?;

        let plan = match (&a.command, &cfg.settings.test_command) {
            (Some(cmd), _) => testrun::TestPlan {
                source: PlanSource::Argument,
                argv: cmd.clone(),
            },
            (None, Some(cmd)) => testrun::TestPlan {
                source: PlanSource::Settings,
                argv: cmd.clone(),
            },
            (None, None) => testrun::detect(&project).ok_or_else(|| {
                McpError::invalid_params(
                    format!(
                        "no test runner found in {}. gdkit looks for \
                         addons/gdUnit4/bin/GdUnitCmdTool.gd, addons/gut/gut_cmdln.gd, or a \
                         test(s)/run_tests.gd-style script. Pass `command`, or persist one with \
                         the `config` tool.",
                        project.display()
                    ),
                    None,
                )
            })?,
        };

        let argv = settings::expand_command(&plan.argv, cfg.godot_bin.as_deref(), Some(&project))
            .map_err(|e| McpError::invalid_params(e, None))?;
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| McpError::invalid_params("empty test command", None))?;

        let timeout = Duration::from_secs(a.timeout_secs.unwrap_or(TESTS_TIMEOUT));
        let res = run_oneshot(Path::new(program), args, timeout, Some(&project))
            .await
            .map_err(|e| {
                McpError::internal_error(format!("test command failed to run: {e}"), None)
            })?;

        let outcome = testrun::shape(&res, a.tail.unwrap_or(40));
        let text = outcome.render(&plan.source, &argv);
        Ok(if outcome.status.is_failure() {
            CallToolResult::error(vec![ContentBlock::text(text)])
        } else {
            CallToolResult::success(vec![ContentBlock::text(text)])
        })
    }

    #[tool(
        description = "Reimport the project's assets headlessly (godot --import) and rebuild the global script-class cache. Run this after adding assets or after adding/renaming a `class_name`. Godot updates project import metadata and caches; engine errors are reported as a failed import."
    )]
    async fn reimport(
        &self,
        Parameters(a): Parameters<ReimportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg().await;
        let godot = need_godot(&cfg)?;
        let project = need_project(&cfg)?;

        // `--import` runs a real editor pass, which is what regenerates
        // .godot/global_script_class_cache.cfg — no script needs to be injected into the project
        // to refresh the class cache.
        let args = vec![
            "--headless".to_string(),
            "--path".to_string(),
            project.display().to_string(),
            "--import".to_string(),
        ];
        let _editor = self.editor.lock().await;
        let timeout = Duration::from_secs(a.timeout_secs.unwrap_or(IMPORT_TIMEOUT));
        let res = run_oneshot(&godot, &args, timeout, None)
            .await
            .map_err(|e| McpError::internal_error(format!("reimport failed to run: {e}"), None))?;

        let cache = project.join(".godot").join("global_script_class_cache.cfg");
        let mut out = format!(
            "reimport {} (exit={:?}{})\nproject: {}\nclass cache: {}\n",
            if res.ok() { "OK" } else { "FAILED" },
            res.exit_code,
            if res.timed_out { ", timed out" } else { "" },
            project.display(),
            if cache.is_file() { "present" } else { "absent" }
        );
        let errors = res.error_lines();
        if !errors.is_empty() {
            out.push_str(&format!("errors ({}):\n", errors.len()));
            for e in errors.iter().take(40) {
                out.push_str(&format!("  {e}\n"));
            }
        }
        Ok(if res.ok() {
            CallToolResult::success(vec![ContentBlock::text(out)])
        } else {
            out.push_str("output tail:\n");
            out.push_str(&lines_tail(&res, 12));
            CallToolResult::error(vec![ContentBlock::text(out)])
        })
    }

    #[tool(
        description = "Export the project through a preset from export_presets.cfg (release, debug, or pack). list=true just names the available presets. Requires a clean engine result and a nonempty artifact created or updated by this export."
    )]
    async fn export(
        &self,
        Parameters(a): Parameters<ExportArgs>,
    ) -> Result<CallToolResult, McpError> {
        let cfg = self.cfg().await;
        let project = need_project(&cfg)?;

        let presets =
            export::read_presets(&project).map_err(|e| McpError::internal_error(e, None))?;
        if a.list {
            let text = if presets.is_empty() {
                format!(
                    "no export presets in {}",
                    export::presets_path(&project).display()
                )
            } else {
                format!("presets: {}", export::names(&presets))
            };
            return Ok(CallToolResult::success(vec![ContentBlock::text(text)]));
        }

        // Argument validation before resolution, so a typo in `mode` is not masked by whatever
        // the preset lookup happens to say first.
        let mode = parse_mode(a.mode.as_deref())?;
        let godot = need_godot(&cfg)?;
        let requested = a
            .preset
            .as_deref()
            .or(cfg.settings.export_preset.as_deref());
        let preset =
            export::choose(&presets, requested).map_err(|e| McpError::invalid_params(e, None))?;
        let output = export::resolve_output(
            &project,
            preset,
            a.output.as_deref(),
            cfg.settings.export_output.as_deref(),
        )
        .map_err(|e| McpError::invalid_params(e, None))?;

        let _editor = self.editor.lock().await;
        let before = export::ArtifactStamp::read(&output).map_err(|e| {
            McpError::internal_error(format!("cannot inspect {}: {e}", output.display()), None)
        })?;
        // The engine refuses to export into a directory that does not exist yet.
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                McpError::internal_error(format!("cannot create {}: {e}", parent.display()), None)
            })?;
        }

        let args = export::export_args(&project, preset, &output, mode);
        let timeout = Duration::from_secs(a.timeout_secs.unwrap_or(EXPORT_TIMEOUT));
        let res = run_oneshot(&godot, &args, timeout, None)
            .await
            .map_err(|e| McpError::internal_error(format!("export failed to run: {e}"), None))?;

        // Trusting the exit code alone is not enough here: the artifact is the evidence.
        let after = export::ArtifactStamp::read(&output).map_err(|e| {
            McpError::internal_error(format!("cannot inspect {}: {e}", output.display()), None)
        })?;
        let (produced, artifact) = export::ArtifactStamp::verdict(before.as_ref(), after.as_ref());
        let ok = res.ok() && produced;
        let mut out = format!(
            "export {} — preset {:?} ({}) -> {}\nexit={:?}{}, artifact {}\n",
            if ok { "OK" } else { "FAILED" },
            preset.name,
            mode.flag(),
            output.display(),
            res.exit_code,
            if res.timed_out { ", timed out" } else { "" },
            artifact
        );
        let errors = res.error_lines();
        if !errors.is_empty() {
            out.push_str(&format!("errors ({}):\n", errors.len()));
            for e in errors.iter().take(40) {
                out.push_str(&format!("  {e}\n"));
            }
        }
        if !ok {
            out.push_str("output tail:\n");
            out.push_str(&lines_tail(&res, 16));
            return Ok(CallToolResult::error(vec![ContentBlock::text(out)]));
        }
        Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
    }

    #[tool(
        description = "Look up the Godot API: a class (\"Sprite2D\"), a member (\"Sprite2D.frame_coords\", found through the inheritance chain), or any substring to search. Answers come from the INSTALLED engine's own API dump — offline and exactly the version the project runs, including descriptions. The first call dumps and caches it (a few seconds); refresh=true re-dumps."
    )]
    async fn docs(&self, Parameters(a): Parameters<DocsArgs>) -> Result<CallToolResult, McpError> {
        let idx = self.api_index(a.refresh).await?;
        let text = idx.query(a.query.as_deref().unwrap_or(""));
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Show or change gdkit's persisted settings (Godot binary, project dir, test command, export defaults). Settings live in the plugin data dir so they survive plugin updates, and take effect immediately. Called with no arguments it reports what is resolved and where from; an empty string (or empty list) clears one setting, clear=true wipes them all."
    )]
    async fn config(
        &self,
        Parameters(a): Parameters<ConfigArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Serialize the complete read/modify/save/replace operation. Cloning under the lock
        // and reacquiring it only to replace would lose concurrent clients' changes.
        let mut current = self.cfg.lock().await;
        let cfg = current.clone();
        let mut settings = cfg.settings.clone();
        let mut changed = a.clear;
        if a.clear {
            settings = Settings::default();
        }

        set_opt(&mut settings.godot_bin, a.godot_bin, &mut changed);
        set_opt(&mut settings.project_dir, a.project_dir, &mut changed);
        set_opt(&mut settings.export_preset, a.export_preset, &mut changed);
        set_opt(&mut settings.export_output, a.export_output, &mut changed);
        if let Some(cmd) = a.test_command {
            settings.test_command = if cmd.is_empty() { None } else { Some(cmd) };
            changed = true;
        }

        let mut out = String::new();
        let cfg = if changed {
            let path = settings.save(&cfg.data_dir).map_err(|e| {
                McpError::internal_error(
                    format!("cannot write settings to {}: {e}", cfg.data_dir.display()),
                    None,
                )
            })?;
            out.push_str(&format!("saved {}\n", path.display()));
            // Re-resolve so the change applies to the very next tool call.
            let fresh = Arc::new(Config::from_settings(cfg.data_dir.clone(), settings));
            *current = fresh.clone();
            fresh
        } else {
            cfg
        };

        drop(current);
        out.push_str(&render_config(&cfg));
        Ok(CallToolResult::success(vec![ContentBlock::text(out)]))
    }
}

async fn load_api(path: PathBuf) -> Result<ApiIndex, McpError> {
    tokio::task::spawn_blocking(move || ApiIndex::load(&path))
        .await
        .map_err(|e| McpError::internal_error(format!("API load panicked: {e}"), None))?
        .map_err(|e| McpError::internal_error(e, None))
}

/// Apply one optional string setting; an empty string means "clear".
fn set_opt(slot: &mut Option<String>, value: Option<String>, changed: &mut bool) {
    if let Some(v) = value {
        *slot = if v.trim().is_empty() { None } else { Some(v) };
        *changed = true;
    }
}

fn parse_mode(mode: Option<&str>) -> Result<ExportMode, McpError> {
    match mode.unwrap_or("release").trim().to_lowercase().as_str() {
        "release" => Ok(ExportMode::Release),
        "debug" => Ok(ExportMode::Debug),
        "pack" => Ok(ExportMode::Pack),
        other => Err(McpError::invalid_params(
            format!("unknown export mode {other:?} — use release, debug, or pack"),
            None,
        )),
    }
}

fn need_godot(cfg: &Config) -> Result<PathBuf, McpError> {
    cfg.godot_bin.clone().ok_or_else(|| {
        McpError::invalid_params(
            "no Godot binary configured (set GODOT_BIN, or persist one with the `config` tool)",
            None,
        )
    })
}

fn need_project(cfg: &Config) -> Result<PathBuf, McpError> {
    cfg.project_dir.clone().ok_or_else(|| {
        McpError::invalid_params(
            "no Godot project found (set GODOT_PROJECT, persist project_dir with the `config` \
             tool, or rely on CLAUDE_PROJECT_DIR)",
            None,
        )
    })
}

/// The last `n` captured lines, indented for inclusion in a report.
fn lines_tail(res: &crate::process::OneshotResult, n: usize) -> String {
    let start = res.lines.len().saturating_sub(n);
    res.lines[start..]
        .iter()
        .map(|l| format!("  {}\n", l.text))
        .collect()
}

/// What is resolved, and from where — the answer to "why is it using that Godot?".
fn render_config(cfg: &Config) -> String {
    let s = &cfg.settings;
    let mut out = format!(
        "resolved:\n  godot_bin: {}\n  project_dir: {}\n  data_dir: {}\n",
        show(cfg.godot_bin.as_deref().map(|p| p.display().to_string())),
        show(cfg.project_dir.as_deref().map(|p| p.display().to_string())),
        cfg.data_dir.display()
    );
    out.push_str(&format!(
        "persisted ({}):\n",
        settings::settings_path(&cfg.data_dir).display()
    ));
    if s.is_empty() {
        out.push_str("  (nothing — everything comes from discovery)\n");
    } else {
        out.push_str(&format!("  godot_bin: {}\n", show(s.godot_bin.clone())));
        out.push_str(&format!("  project_dir: {}\n", show(s.project_dir.clone())));
        out.push_str(&format!(
            "  test_command: {}\n",
            show(s.test_command.as_ref().map(|c| c.join(" ")))
        ));
        out.push_str(&format!(
            "  export_preset: {}\n",
            show(s.export_preset.clone())
        ));
        out.push_str(&format!(
            "  export_output: {}\n",
            show(s.export_output.clone())
        ));
    }
    if let Some(project) = &cfg.project_dir {
        let detected = match (&s.test_command, testrun::detect(project)) {
            (Some(_), _) => "persisted test_command".to_string(),
            (None, Some(plan)) => plan.source.to_string(),
            (None, None) => "none — pass `command` to run_tests or persist one".to_string(),
        };
        out.push_str(&format!("test runner: {detected}\n"));
    }
    out
}

fn show(v: Option<String>) -> String {
    v.filter(|s| !s.is_empty()).unwrap_or_else(|| "—".into())
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GdkitServer {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo (= InitializeResult) is #[non_exhaustive] in rmcp 2.2 — build via the
        // constructor. Implementation::from_build_env() reports rmcp's own crate identity, so
        // override name/version with this crate's.
        let mut server_info = Implementation::from_build_env();
        server_info.name = env!("CARGO_PKG_NAME").to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_instructions(
                "gdkit — Godot dev kit MCP server. Project tooling is live: run/status/stop a \
                 Godot process with classified logs, parse-check a script, run the test suite, \
                 reimport assets, export a build, look up the installed engine's API, and \
                 persist settings. Call `config` with no arguments to see what is resolved.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command_server(name: &str, windows: &str, unix: &str) -> (GdkitServer, PathBuf) {
        let project = crate::testutil::temp_project(name, &[]);
        let godot = crate::testutil::command_fixture(&project, windows, unix);
        let server = GdkitServer::new(Config {
            godot_bin: Some(godot),
            project_dir: Some(project.clone()),
            data_dir: project.join("data"),
            settings: Settings::default(),
        });
        (server, project)
    }

    fn export_request() -> Parameters<ExportArgs> {
        Parameters(ExportArgs {
            preset: None,
            output: Some("build/game.pck".into()),
            mode: Some("pack".into()),
            list: false,
            timeout_secs: Some(10),
        })
    }

    fn write_export_preset(project: &Path) {
        std::fs::write(
            export::presets_path(project),
            "[preset.0]\nname=\"Test\"\nplatform=\"Windows Desktop\"\nexport_filter=\"all_resources\"\ninclude_filter=\"\"\nexclude_filter=\"\"\n[preset.0.options]\nbinary_format/architecture=\"x86_64\"\n",
        )
        .unwrap();
    }

    fn docs_server(name: &str, version: &str) -> (GdkitServer, PathBuf) {
        let (server, project) = command_server(
            name,
            &format!("if \"%~1\"==\"--version\" (\r\necho {version}\r\nexit /b 0\r\n)\r\necho dump>>\"%~dp0dumps.txt\"\r\ncopy /y \"%~dp0api.json\" \"extension_api.json\" >nul\r\nexit /b 0"),
            &format!("if [ \"$1\" = '--version' ]; then echo '{version}'; exit 0; fi\necho dump >> \"$(dirname \"$0\")/dumps.txt\"\ncp \"$(dirname \"$0\")/api.json\" extension_api.json\nexit 0"),
        );
        std::fs::write(
            project.join("api.json"),
            serde_json::json!({
                "header": {"version_full_name": version},
                "classes": [{"name": "Node", "description": version}]
            })
            .to_string(),
        )
        .unwrap();
        (server, project)
    }

    fn docs_request(refresh: bool) -> Parameters<DocsArgs> {
        Parameters(DocsArgs {
            query: None,
            refresh,
        })
    }

    #[tokio::test]
    async fn docs_follow_a_change_of_resolved_engine() {
        let (server, first) = docs_server("docs-first", "4.6.fixture-a");
        let (other, second) = docs_server("docs-second", "4.7.fixture-b");
        let initial = server.docs(docs_request(false)).await.unwrap();
        assert!(format!("{initial:?}").contains("4.6.fixture-a"));
        // Model the resolved configuration swap made by the config tool, independent of env.
        *server.cfg.lock().await = other.cfg().await;
        let changed = server.docs(docs_request(false)).await.unwrap();
        assert!(
            format!("{changed:?}").contains("4.7.fixture-b"),
            "{changed:?}"
        );
        let _ = std::fs::remove_dir_all(first);
        let _ = std::fs::remove_dir_all(second);
    }

    #[tokio::test]
    async fn concurrent_docs_queries_share_one_dump() {
        let (server, project) = docs_server("docs-concurrent", "4.7.fixture-concurrent");
        let (first, second) = tokio::join!(
            server.docs(docs_request(false)),
            server.docs(docs_request(false))
        );
        first.unwrap();
        second.unwrap();
        assert_eq!(
            std::fs::read_to_string(project.join("dumps.txt"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let _ = std::fs::remove_dir_all(project);
    }

    #[tokio::test]
    async fn failed_docs_refresh_preserves_cache_and_reports_failure() {
        let version = "4.7.fixture-refresh";
        let (server, project) = docs_server("docs-refresh", version);
        server.docs(docs_request(false)).await.unwrap();
        let cache = docs::cache_path(&project.join("data"), version);
        let original = std::fs::read(&cache).unwrap();
        crate::testutil::command_fixture(
            &project,
            &format!("if \"%~1\"==\"--version\" (\r\necho {version}\r\nexit /b 0\r\n)\r\necho ERROR: dump failed\r\nexit /b 0"),
            &format!("if [ \"$1\" = '--version' ]; then echo '{version}'; exit 0; fi\necho 'ERROR: dump failed'\nexit 0"),
        );
        assert!(server.docs(docs_request(true)).await.is_err());
        assert_eq!(std::fs::read(cache).unwrap(), original);
        let _ = std::fs::remove_dir_all(project);
    }

    #[tokio::test]
    async fn malformed_docs_refresh_preserves_the_previous_dump() {
        let version = "4.7.fixture-malformed";
        let (server, project) = docs_server("docs-malformed", version);
        server.docs(docs_request(false)).await.unwrap();
        let cache = docs::cache_path(&project.join("data"), version);
        let original = std::fs::read(&cache).unwrap();
        std::fs::write(project.join("api.json"), "{incomplete").unwrap();
        assert!(server.docs(docs_request(true)).await.is_err());
        assert_eq!(std::fs::read(cache).unwrap(), original);
        let _ = std::fs::remove_dir_all(project);
    }

    #[tokio::test]
    async fn import_errors_cannot_report_success_on_exit_zero() {
        let (server, project) = command_server(
            "import-errors",
            "echo ERROR: import failed\r\nexit /b 0",
            "echo 'ERROR: import failed'\nexit 0",
        );
        let result = server
            .reimport(Parameters(ReimportArgs {
                timeout_secs: Some(10),
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{result:?}");
        let _ = std::fs::remove_dir_all(project);
    }

    #[tokio::test]
    async fn export_rejects_an_unchanged_previous_artifact() {
        let (server, project) = command_server("export-stale", "exit /b 0", "exit 0");
        write_export_preset(&project);
        std::fs::create_dir_all(project.join("build")).unwrap();
        let artifact = project.join("build/game.pck");
        std::fs::write(&artifact, "previous build").unwrap();
        let result = server.export(export_request()).await.unwrap();
        assert_eq!(result.is_error, Some(true), "{result:?}");
        assert_eq!(std::fs::read_to_string(artifact).unwrap(), "previous build");
        let _ = std::fs::remove_dir_all(project);
    }

    #[tokio::test]
    async fn export_errors_cannot_report_success_with_an_artifact() {
        let (server, project) = command_server(
            "export-errors",
            "echo output>\"%~dp0build\\game.pck\"\r\necho ERROR: export failed\r\nexit /b 0",
            "echo output > \"$(dirname \"$0\")/build/game.pck\"\necho 'ERROR: export failed'\nexit 0",
        );
        write_export_preset(&project);
        let result = server.export(export_request()).await.unwrap();
        assert_eq!(result.is_error, Some(true), "{result:?}");
        let _ = std::fs::remove_dir_all(project);
    }

    #[tokio::test]
    async fn concurrent_config_updates_preserve_both_settings() {
        let (server, project) = command_server("config-concurrent", "exit /b 0", "exit 0");
        let guard = server.cfg.lock().await;
        let mut first = Box::pin(server.config(Parameters(ConfigArgs {
            export_preset: Some("Test".into()),
            ..Default::default()
        })));
        let mut second = Box::pin(server.config(Parameters(ConfigArgs {
            export_output: Some("build/game.pck".into()),
            ..Default::default()
        })));
        // Queue both calls on the same initial state before allowing either to run.
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        drop(guard);
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();
        let cfg = server.cfg().await;
        assert_eq!(cfg.settings.export_preset.as_deref(), Some("Test"));
        assert_eq!(
            cfg.settings.export_output.as_deref(),
            Some("build/game.pck")
        );
        assert_eq!(Settings::load(&cfg.data_dir), cfg.settings);
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn export_modes_parse_and_default_to_release() {
        assert_eq!(parse_mode(None).unwrap(), ExportMode::Release);
        assert_eq!(parse_mode(Some(" Debug ")).unwrap(), ExportMode::Debug);
        assert_eq!(parse_mode(Some("pack")).unwrap(), ExportMode::Pack);
        let err = parse_mode(Some("sideways")).unwrap_err();
        assert!(format!("{err:?}").contains("sideways"));
    }

    #[test]
    fn an_empty_value_clears_a_setting() {
        let mut slot = Some("old".to_string());
        let mut changed = false;

        set_opt(&mut slot, None, &mut changed);
        assert_eq!(slot.as_deref(), Some("old"), "absent means leave alone");
        assert!(!changed);

        set_opt(&mut slot, Some("new".into()), &mut changed);
        assert_eq!(slot.as_deref(), Some("new"));
        assert!(changed);

        set_opt(&mut slot, Some("   ".into()), &mut changed);
        assert_eq!(slot, None, "a blank value clears");
    }

    #[test]
    fn missing_godot_and_project_produce_actionable_errors() {
        let cfg = Config::default();
        let err = format!("{:?}", need_godot(&cfg).unwrap_err());
        assert!(err.contains("GODOT_BIN"), "{err}");
        let err = format!("{:?}", need_project(&cfg).unwrap_err());
        assert!(err.contains("CLAUDE_PROJECT_DIR"), "{err}");
    }

    /// `reimport` claims a headless `--import` rebuilds the global script-class cache, which is
    /// why gdkit injects no EditorScript into the user's project to do it. That claim is an
    /// engine contract, so it is tested against the engine: a fresh project with a `class_name`
    /// script and no `.godot/` must come out of `--import` with the class in the cache.
    #[tokio::test]
    async fn import_rebuilds_the_global_class_cache() {
        let Some((godot, _engine_guard)) =
            crate::testutil::godot_or_skip("import_rebuilds_class_cache").await
        else {
            return;
        };
        let project = crate::testutil::temp_project(
            "classy",
            &[("thing.gd", "class_name GdkitFixtureThing\nextends Node\n")],
        );
        let cache = project.join(".godot").join("global_script_class_cache.cfg");
        assert!(!cache.exists(), "the fixture must start with no cache");

        let args = vec![
            "--headless".to_string(),
            "--path".to_string(),
            project.display().to_string(),
            "--import".to_string(),
        ];
        let res = run_oneshot(&godot, &args, Duration::from_secs(300), None)
            .await
            .expect("run the import");
        assert_eq!(
            res.exit_code,
            Some(0),
            "import failed: {:?}",
            res.error_lines()
        );

        let body = std::fs::read_to_string(&cache).expect("class cache written");
        assert!(
            body.contains("GdkitFixtureThing"),
            "class cache missing the class_name: {body}"
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    #[tokio::test]
    async fn exports_a_resource_pack_with_the_real_engine() {
        let Some((godot, _engine_guard)) =
            crate::testutil::godot_or_skip("exports_a_resource_pack").await
        else {
            return;
        };
        let project = crate::testutil::temp_project(
            "pack-export",
            &[("resource.tres", "[gd_resource type=\"Resource\" format=3]\n[resource]\nresource_name=\"export fixture\"\n")],
        );
        write_export_preset(&project);
        let server = GdkitServer::new(Config {
            godot_bin: Some(godot),
            project_dir: Some(project.clone()),
            data_dir: project.join("data"),
            settings: Settings::default(),
        });
        let result = server
            .export(Parameters(ExportArgs {
                timeout_secs: Some(120),
                ..export_request().0
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let pack = std::fs::read(project.join("build/game.pck")).unwrap();
        assert!(pack.starts_with(b"GDPC"), "expected a Godot resource pack");
        let repeated = server
            .export(Parameters(ExportArgs {
                timeout_secs: Some(120),
                ..export_request().0
            }))
            .await
            .unwrap();
        assert_eq!(
            repeated.is_error,
            Some(false),
            "re-export must update the artifact: {repeated:?}"
        );
        let _ = std::fs::remove_dir_all(project);
    }

    #[test]
    fn config_report_names_the_settings_file_and_flags_pure_discovery() {
        let cfg = Config {
            godot_bin: Some(PathBuf::from("D:/godot.exe")),
            project_dir: None,
            data_dir: PathBuf::from("D:/data"),
            settings: Settings::default(),
        };
        let out = render_config(&cfg);
        assert!(out.contains("D:/godot.exe"), "{out}");
        assert!(out.contains("settings.json"), "{out}");
        assert!(out.contains("everything comes from discovery"), "{out}");
        // No project resolved, so there is nothing to say about a test runner.
        assert!(!out.contains("test runner:"), "{out}");
    }
}
