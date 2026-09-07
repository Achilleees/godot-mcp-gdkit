//! Test-suite discovery and result shaping.
//!
//! Godot has no standard test runner, so gdkit stays framework-agnostic: it runs a *command* and
//! reads its output. The command comes from the tool call, then persisted settings, then
//! detection — and detection is **evidence-based**: a framework is only claimed when the exact
//! file that would be invoked exists on disk, so a wrong guess degrades to "no runner found"
//! instead of spawning nonsense and reporting a confusing failure.
//!
//! Result shaping is deliberately shallow. Counting assertions would mean parsing each
//! framework's format and silently rotting when one changes; instead the outcome carries the
//! process verdict, the error lines with their GDScript stack frames, and any lines that read
//! like a suite summary, verbatim.

use std::path::Path;

use crate::process::{is_stack_frame, OneshotResult};

/// Directories conventionally holding a Godot project's tests, most specific first.
const TEST_DIRS: [&str; 2] = ["test", "tests"];

/// Script names a hand-rolled harness conventionally uses as its entry point.
const RUNNER_SCRIPTS: [&str; 4] = ["run_tests.gd", "test_runner.gd", "tests.gd", "all_tests.gd"];

/// Where a test command came from — reported so the caller can see what actually ran.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanSource {
    /// Passed to the tool call.
    Argument,
    /// Read from persisted settings.
    Settings,
    /// Detected in the project; the string names the framework/convention.
    Detected(&'static str),
}

impl std::fmt::Display for PlanSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Argument => write!(f, "command argument"),
            Self::Settings => write!(f, "settings.test_command"),
            Self::Detected(what) => write!(f, "detected: {what}"),
        }
    }
}

/// A test command template plus where it came from. `argv` still holds `{godot}`/`{project}`
/// placeholders — expansion happens in one place ([`crate::settings::expand_command`]).
#[derive(Clone, Debug, PartialEq)]
pub struct TestPlan {
    pub source: PlanSource,
    pub argv: Vec<String>,
}

/// Detect a test command by looking for the file each convention would actually invoke. Returns
/// `None` when nothing conclusive is on disk — the caller then tells the operator how to
/// configure one, which is far better than guessing.
pub fn detect(project: &Path) -> Option<TestPlan> {
    let test_dir = TEST_DIRS.iter().find(|d| project.join(d).is_dir());

    // gdUnit4 — its documented headless entry point, run against a test directory.
    let gdunit = Path::new("addons/gdUnit4/bin/GdUnitCmdTool.gd");
    if project.join(gdunit).is_file() {
        if let Some(dir) = test_dir {
            return Some(TestPlan {
                source: PlanSource::Detected("gdUnit4"),
                argv: vec![
                    "{godot}".into(),
                    "--headless".into(),
                    "--path".into(),
                    "{project}".into(),
                    "-s".into(),
                    format!("res://{}", gdunit.display().to_string().replace('\\', "/")),
                    "-a".into(),
                    format!("res://{dir}"),
                ],
            });
        }
    }

    // GUT — its command-line runner, told to quit when the suite finishes.
    let gut = Path::new("addons/gut/gut_cmdln.gd");
    if project.join(gut).is_file() {
        if let Some(dir) = test_dir {
            return Some(TestPlan {
                source: PlanSource::Detected("GUT"),
                argv: vec![
                    "{godot}".into(),
                    "--headless".into(),
                    "--path".into(),
                    "{project}".into(),
                    "-s".into(),
                    format!("res://{}", gut.display().to_string().replace('\\', "/")),
                    format!("-gdir=res://{dir}"),
                    "-gexit".into(),
                ],
            });
        }
    }

    // A hand-rolled harness: a single script run headless.
    for dir in TEST_DIRS {
        for script in RUNNER_SCRIPTS {
            if project.join(dir).join(script).is_file() {
                return Some(TestPlan {
                    source: PlanSource::Detected("script harness"),
                    argv: vec![
                        "{godot}".into(),
                        "--headless".into(),
                        "--path".into(),
                        "{project}".into(),
                        "--script".into(),
                        format!("res://{dir}/{script}"),
                    ],
                });
            }
        }
    }

    None
}

/// How a suite run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TestStatus {
    /// Clean exit, nothing error-classified.
    Passed,
    /// Clean exit, but error-classified lines were printed — a suite that swallows failures, or
    /// tests that deliberately provoke engine errors. Surfaced rather than silently reported green.
    PassedWithErrors,
    /// Non-zero exit.
    Failed,
    /// Killed at the timeout.
    TimedOut,
}

impl TestStatus {
    /// True when the run should be reported to the model as a tool error.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::Failed | Self::TimedOut)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "PASSED",
            Self::PassedWithErrors => "PASSED (with engine errors)",
            Self::Failed => "FAILED",
            Self::TimedOut => "TIMED OUT",
        }
    }
}

/// A shaped test result: the verdict, the errors with their stack frames, and the suite's own
/// summary lines.
#[derive(Clone, Debug)]
pub struct TestOutcome {
    pub status: TestStatus,
    pub exit_code: Option<i32>,
    /// Error lines, each followed by the GDScript stack frames that trailed it.
    pub errors: Vec<String>,
    /// Lines that read like a framework summary, verbatim and in order.
    pub summary: Vec<String>,
    /// The last lines of output, for when nothing else is conclusive.
    pub tail: Vec<String>,
}

/// Substrings that mark a line as a suite summary in the common Godot test frameworks. Matched
/// case-insensitively against short lines only, so a stack trace mentioning "failed" is skipped.
const SUMMARY_MARKERS: [&str; 8] = [
    "passed",
    "failed",
    "failures",
    "assertions",
    "test suites",
    "tests run",
    "orphans",
    "totals",
];

/// Max characters a line may have and still count as a summary.
const SUMMARY_MAX_LEN: usize = 200;

/// Shape a raw one-shot result into a test outcome.
pub fn shape(res: &OneshotResult, tail: usize) -> TestOutcome {
    let status = if res.timed_out {
        TestStatus::TimedOut
    } else if res.exit_code != Some(0) || !res.capture_complete {
        TestStatus::Failed
    } else if res.error_count > 0 {
        TestStatus::PassedWithErrors
    } else {
        TestStatus::Passed
    };

    // Error lines carry their trailing stack frames — an error without its frames is not
    // actionable, and the frames are separate lines in Godot's output.
    let mut errors: Vec<String> = Vec::new();
    let mut collecting = false;
    for line in &res.lines {
        if line.is_error {
            errors.push(line.text.clone());
            collecting = true;
        } else if collecting && is_stack_frame(&line.text) {
            errors.push(line.text.clone());
        } else {
            collecting = false;
        }
    }

    let summary: Vec<String> = res
        .lines
        .iter()
        .filter(|l| !l.is_error && l.text.len() <= SUMMARY_MAX_LEN)
        .filter(|l| {
            let lower = l.text.to_lowercase();
            SUMMARY_MARKERS.iter().any(|m| lower.contains(m))
        })
        .map(|l| l.text.clone())
        .collect();

    let start = res.lines.len().saturating_sub(tail);
    let tail = res.lines[start..].iter().map(|l| l.text.clone()).collect();

    TestOutcome {
        status,
        exit_code: res.exit_code,
        errors,
        summary,
        tail,
    }
}

impl TestOutcome {
    /// Render the outcome for the model: verdict first, then whichever evidence exists.
    pub fn render(&self, plan_source: &PlanSource, argv: &[String]) -> String {
        let mut out = format!(
            "{} (exit={:?})\nran [{plan_source}]: {}\n",
            self.status.label(),
            self.exit_code,
            argv.join(" ")
        );
        if !self.summary.is_empty() {
            out.push_str("summary:\n");
            // The final lines are the suite totals; earlier matches are usually per-suite noise.
            let start = self.summary.len().saturating_sub(8);
            for l in &self.summary[start..] {
                out.push_str(&format!("  {l}\n"));
            }
        }
        if !self.errors.is_empty() {
            out.push_str(&format!("errors ({}):\n", self.errors.len()));
            for l in self.errors.iter().take(40) {
                out.push_str(&format!("  {l}\n"));
            }
        }
        if self.summary.is_empty() && self.errors.is_empty() {
            out.push_str("output tail:\n");
            for l in &self.tail {
                out.push_str(&format!("  {l}\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::LogLine;

    fn tmp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gdkit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "").unwrap();
    }

    fn lines(raw: &[(&str, bool)]) -> OneshotResult {
        OneshotResult {
            exit_code: Some(0),
            timed_out: false,
            error_count: raw.iter().filter(|(_, error)| *error).count(),
            capture_complete: true,
            lines: raw
                .iter()
                .map(|(t, e)| LogLine {
                    text: (*t).to_string(),
                    is_error: *e,
                })
                .collect(),
        }
    }

    #[test]
    fn detects_nothing_in_an_empty_project() {
        let p = tmp("detect-empty");
        assert_eq!(detect(&p), None);
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn a_test_dir_without_a_runner_is_not_enough() {
        let p = tmp("detect-baredir");
        std::fs::create_dir_all(p.join("tests")).unwrap();
        assert_eq!(detect(&p), None, "a bare tests/ dir must not be claimed");
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn detects_a_script_harness() {
        let p = tmp("detect-harness");
        touch(&p, "tests/run_tests.gd");
        let plan = detect(&p).expect("harness detected");
        assert_eq!(plan.source, PlanSource::Detected("script harness"));
        assert!(plan.argv.contains(&"res://tests/run_tests.gd".to_string()));
        assert!(plan.argv.contains(&"--headless".to_string()));
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn detects_gut_only_with_its_runner_present() {
        let p = tmp("detect-gut");
        std::fs::create_dir_all(p.join("addons/gut")).unwrap();
        std::fs::create_dir_all(p.join("test")).unwrap();
        assert_eq!(detect(&p), None, "the addon dir alone must not be claimed");

        touch(&p, "addons/gut/gut_cmdln.gd");
        let plan = detect(&p).expect("GUT detected");
        assert_eq!(plan.source, PlanSource::Detected("GUT"));
        assert!(plan.argv.iter().any(|a| a == "-gdir=res://test"));
        assert!(plan.argv.iter().any(|a| a == "-gexit"));
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn gdunit_wins_over_a_script_harness() {
        let p = tmp("detect-gdunit");
        touch(&p, "addons/gdUnit4/bin/GdUnitCmdTool.gd");
        touch(&p, "tests/run_tests.gd");
        let plan = detect(&p).expect("gdUnit4 detected");
        assert_eq!(plan.source, PlanSource::Detected("gdUnit4"));
        assert!(plan
            .argv
            .iter()
            .any(|a| a == "res://addons/gdUnit4/bin/GdUnitCmdTool.gd"));
        assert!(plan.argv.iter().any(|a| a == "res://tests"));
        let _ = std::fs::remove_dir_all(&p);
    }

    #[test]
    fn a_clean_run_passes() {
        let res = lines(&[("Tests run: 12, failures: 0", false)]);
        let out = shape(&res, 40);
        assert_eq!(out.status, TestStatus::Passed);
        assert!(!out.status.is_failure());
        assert_eq!(out.summary, vec!["Tests run: 12, failures: 0"]);
    }

    #[test]
    fn a_non_zero_exit_fails() {
        let mut res = lines(&[("boom", false)]);
        res.exit_code = Some(1);
        let out = shape(&res, 40);
        assert_eq!(out.status, TestStatus::Failed);
        assert!(out.status.is_failure());
    }

    #[test]
    fn a_timeout_is_its_own_status() {
        let mut res = lines(&[("hung", false)]);
        res.exit_code = None;
        res.timed_out = true;
        let out = shape(&res, 40);
        assert_eq!(out.status, TestStatus::TimedOut);
        assert!(out.status.is_failure());
    }

    #[test]
    fn a_clean_exit_with_engine_errors_is_flagged_not_hidden() {
        let res = lines(&[("SCRIPT ERROR: nope", true), ("done", false)]);
        let out = shape(&res, 40);
        assert_eq!(out.status, TestStatus::PassedWithErrors);
        assert!(!out.status.is_failure(), "a clean exit is not a tool error");
    }

    #[test]
    fn an_incomplete_capture_cannot_report_passing_tests() {
        let mut res = lines(&[("Tests run: 2, failures: 0", false)]);
        res.capture_complete = false;
        let outcome = shape(&res, 40);
        assert_eq!(outcome.status, TestStatus::Failed);
        assert!(outcome.status.is_failure());
        assert!(!res.ok());
    }

    #[test]
    fn an_error_evicted_from_the_tail_still_affects_the_verdict() {
        let mut res = lines(&[("all done", false)]);
        res.error_count = 1;
        assert_eq!(shape(&res, 40).status, TestStatus::PassedWithErrors);
        assert!(!res.ok());
    }

    #[test]
    fn errors_keep_their_stack_frames_and_drop_unrelated_lines() {
        let res = lines(&[
            ("noise", false),
            ("SCRIPT ERROR: Invalid call", true),
            ("   at: _ready (res://main.gd:12)", false),
            ("   at: run (res://tests/run_tests.gd:4)", false),
            ("unrelated output", false),
            ("   at: orphan frame with no error", false),
        ]);
        let out = shape(&res, 40);
        assert_eq!(
            out.errors.len(),
            3,
            "error + its two frames: {:?}",
            out.errors
        );
        assert!(out.errors[0].contains("Invalid call"));
        assert!(out.errors[2].contains("run_tests.gd:4"));
    }

    #[test]
    fn long_lines_are_not_mistaken_for_a_summary() {
        let long = format!("failed {}", "x".repeat(SUMMARY_MAX_LEN));
        let res = lines(&[(long.as_str(), false)]);
        assert!(shape(&res, 40).summary.is_empty());
    }

    /// A `SceneTree` script is how a hand-rolled harness runs headlessly; `quit(n)` is how it
    /// reports its verdict. Both halves are engine contracts this tool depends on, so the test
    /// drives the real engine end-to-end: detect -> expand -> run -> shape.
    #[tokio::test]
    async fn runs_a_detected_harness_against_the_real_engine() {
        let Some((godot, _engine_guard)) =
            crate::testutil::godot_or_skip("runs_a_detected_harness").await
        else {
            return;
        };
        let project = crate::testutil::temp_project(
            "suite",
            &[
                (
                    "tests/run_tests.gd",
                    "extends SceneTree\n\n\nfunc _initialize() -> void:\n\
                     \tprint(\"Tests run: 2, failures: 0\")\n\tquit(0)\n",
                ),
                (
                    "tests/fail_tests.gd",
                    "extends SceneTree\n\n\nfunc _initialize() -> void:\n\
                     \tprint(\"Tests run: 2, failures: 1\")\n\tquit(1)\n",
                ),
            ],
        );

        let plan = detect(&project).expect("harness detected");
        assert_eq!(plan.source, PlanSource::Detected("script harness"));
        let argv =
            crate::settings::expand_command(&plan.argv, Some(&godot), Some(&project)).unwrap();
        let (program, args) = argv.split_first().unwrap();
        let res = crate::process::run_oneshot(
            Path::new(program),
            args,
            std::time::Duration::from_secs(120),
            Some(&project),
        )
        .await
        .expect("run the suite");
        let outcome = shape(&res, 40);
        assert_eq!(
            outcome.status,
            TestStatus::Passed,
            "tail:\n{}",
            outcome.tail.join("\n")
        );
        assert!(
            outcome.summary.iter().any(|l| l.contains("failures: 0")),
            "summary: {:?}",
            outcome.summary
        );

        // The same harness quitting non-zero must come back FAILED, not green.
        let failing = vec![
            "{godot}".to_string(),
            "--headless".to_string(),
            "--path".to_string(),
            "{project}".to_string(),
            "--script".to_string(),
            "res://tests/fail_tests.gd".to_string(),
        ];
        let argv = crate::settings::expand_command(&failing, Some(&godot), Some(&project)).unwrap();
        let (program, args) = argv.split_first().unwrap();
        let res = crate::process::run_oneshot(
            Path::new(program),
            args,
            std::time::Duration::from_secs(120),
            Some(&project),
        )
        .await
        .expect("run the failing suite");
        let outcome = shape(&res, 40);
        assert_eq!(outcome.status, TestStatus::Failed);
        assert!(outcome.status.is_failure());

        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn render_names_the_command_and_its_source() {
        let res = lines(&[("Tests run: 3, failures: 0", false)]);
        let out = shape(&res, 40);
        let text = out.render(
            &PlanSource::Detected("script harness"),
            &["godot.exe".to_string(), "--headless".to_string()],
        );
        assert!(text.starts_with("PASSED"), "{text}");
        assert!(text.contains("detected: script harness"), "{text}");
        assert!(text.contains("godot.exe --headless"), "{text}");
    }
}
