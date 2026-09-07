//! Configuration + discovery for the gdkit MCP server.
//!
//! Resolves the Godot binary, the target project directory, and the data directory holding
//! persisted settings. Nothing here hard-fails: the server must still load (and answer `ping`)
//! even when Godot isn't found — tools that need Godot check `godot_bin` and return a clear error
//! if it is missing.
//!
//! Precedence — Godot binary: `GODOT_BIN` env > persisted `godot_bin` > `PATH` > common install
//! locations. Precedence — project dir: `GODOT_PROJECT` env > persisted `project_dir` >
//! `CLAUDE_PROJECT_DIR` env > current dir. Env beats the settings file so a one-off session can
//! override without editing anything; the settings file beats discovery so a deliberate choice
//! survives a machine with several Godot builds lying around.

use std::path::{Path, PathBuf};

use crate::settings::{self, Settings};

#[derive(Clone, Debug, Default)]
pub struct Config {
    /// Godot executable. On Windows the *console* build is preferred so the child's stdout/
    /// stderr can be captured for log classification (the plain build detaches from the console).
    pub godot_bin: Option<PathBuf>,
    /// Project directory containing `project.godot`. Optional — some tools (e.g. `docs`) don't
    /// need it.
    pub project_dir: Option<PathBuf>,
    /// Writable directory that survives plugin updates: settings file + API-reference cache.
    pub data_dir: PathBuf,
    /// The persisted overrides that fed this resolution.
    pub settings: Settings,
}

impl Config {
    /// Resolve from environment + persisted settings + on-disk discovery. Never fails.
    pub fn resolve() -> Self {
        let data_dir = settings::resolve_data_dir(settings::process_env);
        let settings = Settings::load(&data_dir);
        Self::from_settings(data_dir, settings)
    }

    /// Resolve with an already-loaded settings file — the path `config` takes after a write, so a
    /// new setting takes effect without restarting the server.
    pub fn from_settings(data_dir: PathBuf, settings: Settings) -> Self {
        let godot_bin = first_existing_file(godot_overrides(
            std::env::var("GODOT_BIN").ok().as_deref(),
            &settings,
        ))
        .or_else(discover_godot)
        .and_then(|path| std::path::absolute(path).ok());

        let project_dir = first_project_dir(project_candidates(
            std::env::var("GODOT_PROJECT").ok().as_deref(),
            &settings,
            std::env::var("CLAUDE_PROJECT_DIR").ok().as_deref(),
            std::env::current_dir().ok(),
        ));

        Self {
            godot_bin,
            project_dir,
            data_dir: std::path::absolute(&data_dir).unwrap_or(data_dir),
            settings,
        }
    }

    /// Log what discovery resolved (info) or failed to find (warn).
    pub fn log_summary(&self) {
        match &self.godot_bin {
            Some(p) => tracing::info!("godot binary: {}", p.display()),
            None => tracing::warn!(
                "no Godot binary found — set GODOT_BIN or persist one with the `config` tool; \
                 run/test/export/reimport will error until it is configured"
            ),
        }
        match &self.project_dir {
            Some(p) => tracing::info!("project dir: {}", p.display()),
            None => tracing::warn!(
                "no Godot project found (no project.godot) — set GODOT_PROJECT or rely on \
                 CLAUDE_PROJECT_DIR"
            ),
        }
        tracing::info!("data dir: {}", self.data_dir.display());
    }
}

/// Explicit Godot-binary candidates, highest precedence first. Checked before any discovery so a
/// deliberate choice always wins over whatever happens to be installed.
pub fn godot_overrides(env_godot: Option<&str>, settings: &Settings) -> Vec<PathBuf> {
    [env_godot, settings.godot_bin.as_deref()]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Project-directory candidates, highest precedence first.
pub fn project_candidates(
    env_project: Option<&str>,
    settings: &Settings,
    env_claude: Option<&str>,
    cwd: Option<PathBuf>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [env_project, settings.project_dir.as_deref(), env_claude]
        .into_iter()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .collect();
    out.extend(cwd);
    out
}

/// First candidate that exists as a file. A candidate that does not exist is logged rather than
/// skipped silently — a stale override is the likeliest cause of "why is it using the wrong Godot".
pub fn first_existing_file(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    for c in candidates {
        if c.is_file() {
            return std::path::absolute(c).ok();
        }
        tracing::warn!("configured Godot binary is not a file: {}", c.display());
    }
    None
}

/// First candidate directory that actually holds a `project.godot`.
pub fn first_project_dir(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find(|dir| dir.join("project.godot").is_file())
        .and_then(|dir| std::path::absolute(dir).ok())
}

/// `PATH` (godot/godot.exe) > common install locations. Console build preferred.
fn discover_godot() -> Option<PathBuf> {
    for name in ["godot.exe", "godot"] {
        if let Some(p) = which_on_path(name) {
            return Some(p);
        }
    }

    // Common install locations. Prefer a "console" build (writes to stdout for log capture),
    // then the lexically-latest name (roughly the latest version).
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in common_godot_dirs() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                let name = file_name_lower(&p);
                if name.starts_with("godot") && name.ends_with(".exe") {
                    found.push(p);
                }
            }
        }
    }
    found.into_iter().max_by_key(|p| {
        let n = file_name_lower(p);
        (n.contains("console"), n)
    })
}

fn file_name_lower(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
}

/// Look up an executable name on `PATH`.
fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|cand| cand.is_file())
}

/// Windows locations worth probing for a manually-installed Godot.
fn common_godot_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(up) = std::env::var_os("USERPROFILE") {
        let up = PathBuf::from(up);
        dirs.push(up.join("Desktop"));
        dirs.push(up.join("Downloads"));
    }
    for env_key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        if let Some(v) = std::env::var_os(env_key) {
            let v = PathBuf::from(v);
            dirs.push(v.join("Godot"));
            dirs.push(v);
        }
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with(godot: Option<&str>, project: Option<&str>) -> Settings {
        Settings {
            godot_bin: godot.map(str::to_string),
            project_dir: project.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn the_env_override_outranks_the_settings_file() {
        let s = settings_with(Some("D:/from-settings.exe"), None);
        let c = godot_overrides(Some("D:/from-env.exe"), &s);
        assert_eq!(
            c,
            vec![
                PathBuf::from("D:/from-env.exe"),
                PathBuf::from("D:/from-settings.exe")
            ]
        );
    }

    #[test]
    fn a_blank_override_is_ignored() {
        let s = settings_with(Some("  "), None);
        assert!(godot_overrides(Some(""), &s).is_empty());
    }

    #[test]
    fn project_precedence_is_env_then_settings_then_host_then_cwd() {
        let s = settings_with(None, Some("D:/settings-proj"));
        let c = project_candidates(
            Some("D:/env-proj"),
            &s,
            Some("D:/claude-proj"),
            Some(PathBuf::from("D:/cwd")),
        );
        assert_eq!(
            c,
            vec![
                PathBuf::from("D:/env-proj"),
                PathBuf::from("D:/settings-proj"),
                PathBuf::from("D:/claude-proj"),
                PathBuf::from("D:/cwd"),
            ]
        );
    }

    #[test]
    fn only_a_directory_holding_project_godot_is_picked() {
        let root = std::env::temp_dir().join(format!("gdkit-projpick-{}", std::process::id()));
        let empty = root.join("empty");
        let real = root.join("real");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("project.godot"), "config_version=5\n").unwrap();

        assert_eq!(
            first_project_dir(vec![empty.clone(), real.clone()]),
            Some(real)
        );
        assert_eq!(first_project_dir(vec![empty]), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_binary_override_falls_through_instead_of_being_used() {
        let dir = std::env::temp_dir().join(format!("gdkit-binpick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("godot-stub.exe");
        std::fs::write(&real, "").unwrap();

        let picked = first_existing_file(vec![dir.join("missing.exe"), real.clone()]);
        assert_eq!(picked, Some(real));
        assert_eq!(first_existing_file(vec![dir.join("nope.exe")]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_discovery_is_anchored_before_a_child_changes_directory() {
        let relative = PathBuf::from(format!("gdkit-relative-{}", std::process::id()));
        let directory = std::env::current_dir().unwrap().join(&relative);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("project.godot"), "config_version=5\n").unwrap();
        std::fs::write(directory.join("engine.exe"), "fixture").unwrap();
        assert_eq!(
            first_project_dir(vec![relative.clone()]),
            Some(directory.clone())
        );
        assert_eq!(
            first_existing_file(vec![relative.join("engine.exe")]),
            Some(directory.join("engine.exe"))
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
