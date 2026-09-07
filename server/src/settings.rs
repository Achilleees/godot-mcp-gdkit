//! Persisted gdkit settings.
//!
//! Discovery (Godot binary, project dir) works with zero setup, but a project that keeps Godot
//! somewhere unusual — or that needs a specific test command or export preset — must be able to
//! say so once and have it stick. Those answers live in a JSON file under the plugin *data*
//! directory, which survives plugin updates (the install dir is replaced wholesale on update, so
//! nothing writable may live beside the binary).
//!
//! Precedence for the directory: `GDKIT_DATA_DIR` > `CLAUDE_PLUGIN_DATA` (supplied by the host
//! when the server runs as a plugin) > the OS per-user data dir > `.gdkit` in the current dir.
//! Nothing here hard-fails: an unreadable or malformed settings file logs and yields defaults, so
//! a bad edit can never stop the server from loading.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name inside the data dir.
const SETTINGS_FILE: &str = "settings.json";

/// Operator-set overrides. Every field is optional — absent means "use discovery".
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Settings {
    /// Absolute path to the Godot executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub godot_bin: Option<String>,
    /// Absolute path to the directory holding `project.godot`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_dir: Option<String>,
    /// Command that runs the project's test suite, as program + args. Supports the placeholders
    /// `{godot}` (resolved Godot binary) and `{project}` (resolved project dir); a bare `godot`
    /// as the program is treated as `{godot}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_command: Option<Vec<String>>,
    /// Default `export_presets.cfg` preset name for the `export` tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_preset: Option<String>,
    /// Default output path for the `export` tool (absolute, or relative to the project dir).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub export_output: Option<String>,
}

impl Settings {
    /// Read the settings file from `dir`. A missing file yields defaults; a malformed one logs a
    /// warning and also yields defaults (never fails the server).
    pub fn load(dir: &Path) -> Self {
        let path = settings_path(dir);
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                tracing::warn!("settings unreadable at {}: {e}", path.display());
                return Self::default();
            }
        };
        match serde_json::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "settings malformed at {} ({e}) — using defaults",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// Write the settings file into `dir`, creating the directory if needed.
    pub fn save(&self, dir: &Path) -> std::io::Result<PathBuf> {
        let path = settings_path(dir);
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        let scratch = crate::storage::ScratchDir::new(dir)?;
        let pending = scratch.path().join(SETTINGS_FILE);
        std::fs::write(&pending, format!("{json}\n"))?;
        std::fs::rename(pending, &path)?;
        Ok(path)
    }

    /// True when no override is set — i.e. everything comes from discovery.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// The settings file's path inside a data dir.
pub fn settings_path(dir: &Path) -> PathBuf {
    dir.join(SETTINGS_FILE)
}

/// Resolve the data directory. Reads the environment through `env` so tests can drive it.
pub fn resolve_data_dir<F>(env: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    for key in ["GDKIT_DATA_DIR", "CLAUDE_PLUGIN_DATA"] {
        if let Some(v) = env(key).filter(|v| !v.trim().is_empty()) {
            return PathBuf::from(v);
        }
    }
    // Per-user data dir. LOCALAPPDATA on Windows; the XDG pair elsewhere.
    if let Some(v) = env("LOCALAPPDATA").filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(v).join("gdkit");
    }
    if let Some(v) = env("XDG_DATA_HOME").filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(v).join("gdkit");
    }
    if let Some(v) = env("HOME").filter(|v| !v.trim().is_empty()) {
        return PathBuf::from(v).join(".local").join("share").join("gdkit");
    }
    PathBuf::from(".gdkit")
}

/// Read the process environment — the production input to [`resolve_data_dir`].
pub fn process_env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// Substitute the `{godot}` / `{project}` placeholders in a configured command, and treat a bare
/// `godot` program name as `{godot}`. Returns an error naming the placeholder that could not be
/// filled, so the caller can report *why* the command is unusable rather than spawning nonsense.
pub fn expand_command(
    cmd: &[String],
    godot: Option<&Path>,
    project: Option<&Path>,
) -> Result<Vec<String>, String> {
    if cmd.is_empty() {
        return Err("test_command is empty".to_string());
    }
    let mut out = Vec::with_capacity(cmd.len());
    for (i, part) in cmd.iter().enumerate() {
        // A bare `godot` in program position means "whatever discovery resolved".
        let part = if i == 0 && part.eq_ignore_ascii_case("godot") {
            "{godot}"
        } else {
            part.as_str()
        };
        let mut s = part.to_string();
        if s.contains("{godot}") {
            let g = godot.ok_or_else(|| {
                "test_command uses {godot} but no Godot binary is configured".to_string()
            })?;
            s = s.replace("{godot}", &g.display().to_string());
        }
        if s.contains("{project}") {
            let p = project.ok_or_else(|| {
                "test_command uses {project} but no Godot project was found".to_string()
            })?;
            s = s.replace("{project}", &p.display().to_string());
        }
        out.push(s);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| {
            owned
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn data_dir_prefers_the_test_override() {
        let dir = resolve_data_dir(env_from(&[
            ("GDKIT_DATA_DIR", "D:/override"),
            ("CLAUDE_PLUGIN_DATA", "D:/plugin"),
            ("LOCALAPPDATA", "D:/appdata"),
        ]));
        assert_eq!(dir, PathBuf::from("D:/override"));
    }

    #[test]
    fn data_dir_uses_the_plugin_data_dir_when_hosted() {
        let dir = resolve_data_dir(env_from(&[
            ("CLAUDE_PLUGIN_DATA", "D:/plugin"),
            ("LOCALAPPDATA", "D:/appdata"),
        ]));
        assert_eq!(dir, PathBuf::from("D:/plugin"));
    }

    #[test]
    fn data_dir_falls_back_to_the_per_user_dir() {
        let dir = resolve_data_dir(env_from(&[("LOCALAPPDATA", "D:/appdata")]));
        assert_eq!(dir, PathBuf::from("D:/appdata").join("gdkit"));

        let dir = resolve_data_dir(env_from(&[("HOME", "/home/dev")]));
        assert_eq!(dir, PathBuf::from("/home/dev/.local/share/gdkit"));
    }

    #[test]
    fn blank_env_values_are_ignored() {
        let dir = resolve_data_dir(env_from(&[
            ("GDKIT_DATA_DIR", "   "),
            ("LOCALAPPDATA", "D:/appdata"),
        ]));
        assert_eq!(dir, PathBuf::from("D:/appdata").join("gdkit"));
    }

    #[test]
    fn settings_round_trip_through_disk() {
        let dir = std::env::temp_dir().join(format!("gdkit-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        // A missing file is not an error.
        assert!(Settings::load(&dir).is_empty());

        let s = Settings {
            godot_bin: Some("D:/godot/godot.exe".into()),
            test_command: Some(vec![
                "godot".into(),
                "-s".into(),
                "res://tests/run.gd".into(),
            ]),
            ..Default::default()
        };
        let path = s.save(&dir).expect("save settings");
        assert!(path.is_file());
        assert_eq!(Settings::load(&dir), s);

        // Absent fields stay absent in the file rather than serializing as nulls.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("export_preset"),
            "unset fields must be omitted: {raw}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_settings_degrade_to_defaults() {
        let dir = std::env::temp_dir().join(format!("gdkit-bad-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(settings_path(&dir), "{ not json").unwrap();
        assert!(Settings::load(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expand_fills_both_placeholders() {
        let cmd = vec![
            "godot".to_string(),
            "--path".to_string(),
            "{project}".to_string(),
            "-s".to_string(),
            "res://tests/run.gd".to_string(),
        ];
        let out = expand_command(
            &cmd,
            Some(Path::new("D:/godot/godot.exe")),
            Some(Path::new("D:/game")),
        )
        .unwrap();
        assert_eq!(out[0], "D:/godot/godot.exe");
        assert_eq!(out[2], "D:/game");
    }

    #[test]
    fn expand_leaves_an_external_runner_alone() {
        let cmd = vec!["./addons/gut/runtest.cmd".to_string(), "-gexit".to_string()];
        let out = expand_command(&cmd, None, None).unwrap();
        assert_eq!(out, cmd);
    }

    #[test]
    fn expand_reports_the_missing_placeholder() {
        let cmd = vec!["godot".to_string(), "{project}".to_string()];
        let err = expand_command(&cmd, None, Some(Path::new("D:/game"))).unwrap_err();
        assert!(err.contains("{godot}"), "got: {err}");

        let err = expand_command(&cmd, Some(Path::new("g.exe")), None).unwrap_err();
        assert!(err.contains("{project}"), "got: {err}");

        assert!(expand_command(&[], None, None).is_err());
    }
}
