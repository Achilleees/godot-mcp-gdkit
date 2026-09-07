//! Export-preset reading and export invocation.
//!
//! Godot's export CLI addresses presets *by name*, and the names live in the project's
//! `export_presets.cfg` — a file the engine writes and the operator rarely reads. Parsing it here
//! means the `export` tool can list what is available, pick the single preset when there is only
//! one, and default the output path to whatever the preset already declares.
//!
//! Two engine behaviours shape the code: the output's parent directory must exist before the
//! export runs (the engine refuses rather than creating it), and a failed export can still exit
//! zero — so the caller verifies the artifact appeared instead of trusting the exit code alone.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Snapshot the artifact before exporting so an older build cannot satisfy verification.
#[derive(PartialEq, Eq)]
pub struct ArtifactStamp {
    len: u64,
    modified: SystemTime,
    created: Option<SystemTime>,
}

impl ArtifactStamp {
    pub fn read(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => Ok(Some(Self {
                len: meta.len(),
                modified: meta.modified()?,
                created: meta.created().ok(),
            })),
            Ok(_) => Err(std::io::Error::other("export output is not a regular file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn verdict(before: Option<&Self>, after: Option<&Self>) -> (bool, &'static str) {
        match after {
            None => (false, "MISSING"),
            Some(stamp) if stamp.len == 0 => (false, "EMPTY"),
            Some(stamp) if before == Some(stamp) => (false, "UNCHANGED from before this export"),
            Some(_) => (true, "created or updated"),
        }
    }
}

/// One entry from `export_presets.cfg`.
#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub name: String,
    pub platform: String,
    /// The preset's configured output path — relative to the project dir unless absolute. Empty
    /// when the preset never had one set in the editor.
    pub export_path: String,
    pub runnable: bool,
}

/// Which of Godot's export modes to invoke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportMode {
    Release,
    Debug,
    /// Data only — a `.pck` or `.zip`, no executable.
    Pack,
}

impl ExportMode {
    pub fn flag(self) -> &'static str {
        match self {
            Self::Release => "--export-release",
            Self::Debug => "--export-debug",
            Self::Pack => "--export-pack",
        }
    }
}

/// Path of a project's export presets file.
pub fn presets_path(project: &Path) -> PathBuf {
    project.join("export_presets.cfg")
}

/// Read and parse a project's export presets. A missing file yields an empty list — a project
/// simply may not have configured any exports yet.
pub fn read_presets(project: &Path) -> Result<Vec<Preset>, String> {
    let path = presets_path(project);
    match std::fs::read_to_string(&path) {
        Ok(raw) => Ok(parse_presets(&raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// Parse the `[preset.N]` sections of an `export_presets.cfg`. The `[preset.N.options]` sections
/// hold per-platform knobs gdkit does not touch, so they are skipped.
pub fn parse_presets(raw: &str) -> Vec<Preset> {
    let mut out: Vec<Preset> = Vec::new();
    let mut in_preset = false;

    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            let section = &line[1..line.len() - 1];
            in_preset = section.starts_with("preset.") && !section.ends_with(".options");
            if in_preset {
                out.push(Preset {
                    name: String::new(),
                    platform: String::new(),
                    export_path: String::new(),
                    runnable: false,
                });
            }
            continue;
        }
        if !in_preset || line.is_empty() || line.starts_with(';') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let Some(preset) = out.last_mut() else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => preset.name = value,
            "platform" => preset.platform = value,
            "export_path" => preset.export_path = value,
            "runnable" => preset.runnable = value == "true",
            _ => {}
        }
    }

    // A section without a name is not addressable by the export CLI, so it is not a usable preset.
    out.retain(|p| !p.name.is_empty());
    out
}

/// Strip surrounding double quotes and unescape the two sequences Godot's config writer emits.
fn unquote(v: &str) -> String {
    let inner = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(v);
    inner.replace("\\\"", "\"").replace("\\\\", "\\")
}

/// Choose the preset to export: the requested name, else the only one, else an error listing the
/// candidates so the caller can pick.
pub fn choose<'a>(presets: &'a [Preset], requested: Option<&str>) -> Result<&'a Preset, String> {
    if presets.is_empty() {
        return Err(
            "no export presets — configure one in the Godot editor (Project > Export), which \
             writes export_presets.cfg"
                .to_string(),
        );
    }
    match requested {
        Some(name) => presets
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("no preset named {name:?}. Available: {}", names(presets))),
        None if presets.len() == 1 => Ok(&presets[0]),
        None => Err(format!(
            "several export presets exist — name one. Available: {}",
            names(presets)
        )),
    }
}

/// Comma-separated preset names, for error messages and listings.
pub fn names(presets: &[Preset]) -> String {
    presets
        .iter()
        .map(|p| format!("{:?} ({})", p.name, p.platform))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve the output path: the explicit request, else the configured default, else the preset's
/// own `export_path`. A relative path resolves against the project dir, matching the engine.
pub fn resolve_output(
    project: &Path,
    preset: &Preset,
    requested: Option<&str>,
    configured: Option<&str>,
) -> Result<PathBuf, String> {
    let chosen = requested
        .filter(|s| !s.trim().is_empty())
        .or(configured.filter(|s| !s.trim().is_empty()))
        .or(Some(preset.export_path.as_str()).filter(|s| !s.trim().is_empty()))
        .ok_or_else(|| {
            format!(
                "preset {:?} has no export_path — pass an output path",
                preset.name
            )
        })?;

    let p = PathBuf::from(chosen);
    Ok(if p.is_absolute() { p } else { project.join(p) })
}

/// Build the Godot argv for an export run.
pub fn export_args(
    project: &Path,
    preset: &Preset,
    output: &Path,
    mode: ExportMode,
) -> Vec<String> {
    vec![
        "--headless".to_string(),
        "--path".to_string(),
        project.display().to_string(),
        mode.flag().to_string(),
        preset.name.clone(),
        output.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"[preset.0]

name="Windows Desktop"
platform="Windows Desktop"
runnable=true
advanced_options=false
export_filter="all_resources"
export_path="builds/win/game.exe"
encryption_include_filters=""

[preset.0.options]

custom_template/debug=""
binary_format/embed_pck=false

[preset.1]

name="Linux"
platform="Linux"
runnable=false
export_path="builds/linux/game.x86_64"

[preset.1.options]

binary_format/architecture="x86_64"
"#;

    #[test]
    fn parses_presets_and_skips_option_sections() {
        let presets = parse_presets(SAMPLE);
        assert_eq!(presets.len(), 2, "{presets:?}");
        assert_eq!(presets[0].name, "Windows Desktop");
        assert_eq!(presets[0].platform, "Windows Desktop");
        assert_eq!(presets[0].export_path, "builds/win/game.exe");
        assert!(presets[0].runnable);
        assert_eq!(presets[1].name, "Linux");
        assert!(!presets[1].runnable);
        // A key that only exists inside [preset.N.options] must never leak into the preset.
        assert!(!presets[0].export_path.contains("x86_64"));
    }

    #[test]
    fn ignores_a_nameless_section() {
        let presets = parse_presets("[preset.0]\nplatform=\"Web\"\n");
        assert!(presets.is_empty());
    }

    #[test]
    fn unquotes_escapes() {
        assert_eq!(unquote(r#""a\"b""#), "a\"b");
        assert_eq!(unquote("true"), "true");
        assert_eq!(unquote(r#""C:\\games""#), r"C:\games");
    }

    #[test]
    fn missing_presets_file_is_empty_not_an_error() {
        let dir = std::env::temp_dir().join(format!("gdkit-nopresets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(read_presets(&dir).unwrap(), Vec::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn choose_takes_the_only_preset_but_not_one_of_two() {
        let presets = parse_presets(SAMPLE);
        assert_eq!(choose(&presets, Some("Linux")).unwrap().name, "Linux");

        let err = choose(&presets, None).unwrap_err();
        assert!(
            err.contains("Windows Desktop") && err.contains("Linux"),
            "{err}"
        );

        let one = vec![presets[0].clone()];
        assert_eq!(choose(&one, None).unwrap().name, "Windows Desktop");

        let err = choose(&presets, Some("Mac")).unwrap_err();
        assert!(err.contains("Available:"), "{err}");

        assert!(choose(&[], None).unwrap_err().contains("no export presets"));
    }

    #[test]
    fn output_precedence_is_request_then_setting_then_preset() {
        let project = Path::new("D:/game");
        let preset = &parse_presets(SAMPLE)[0];

        let p = resolve_output(project, preset, Some("out/a.exe"), Some("out/b.exe")).unwrap();
        assert_eq!(p, project.join("out/a.exe"));

        let p = resolve_output(project, preset, None, Some("out/b.exe")).unwrap();
        assert_eq!(p, project.join("out/b.exe"));

        let p = resolve_output(project, preset, None, None).unwrap();
        assert_eq!(p, project.join("builds/win/game.exe"));

        // Blank strings are treated as absent, not as a valid path.
        let p = resolve_output(project, preset, Some("  "), None).unwrap();
        assert_eq!(p, project.join("builds/win/game.exe"));
    }

    #[test]
    fn an_absolute_output_is_left_alone() {
        let preset = &parse_presets(SAMPLE)[0];
        let p = resolve_output(Path::new("D:/game"), preset, Some("C:/out/g.exe"), None).unwrap();
        assert_eq!(p, PathBuf::from("C:/out/g.exe"));
    }

    #[test]
    fn a_preset_without_an_export_path_needs_one_passed() {
        let preset = Preset {
            name: "Web".into(),
            platform: "Web".into(),
            export_path: String::new(),
            runnable: false,
        };
        let err = resolve_output(Path::new("D:/game"), &preset, None, None).unwrap_err();
        assert!(err.contains("no export_path"), "{err}");
    }

    #[test]
    fn export_args_carry_the_mode_flag_and_preset_name() {
        let preset = &parse_presets(SAMPLE)[0];
        let args = export_args(
            Path::new("D:/game"),
            preset,
            Path::new("D:/game/builds/win/game.exe"),
            ExportMode::Debug,
        );
        assert_eq!(args[0], "--headless");
        assert!(args.contains(&"--export-debug".to_string()));
        assert!(args.contains(&"Windows Desktop".to_string()));
        assert_eq!(ExportMode::Pack.flag(), "--export-pack");
        assert_eq!(ExportMode::Release.flag(), "--export-release");
    }
}
