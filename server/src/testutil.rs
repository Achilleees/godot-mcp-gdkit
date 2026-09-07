//! Shared helpers for the Godot-backed tests.
//!
//! The self-tests that actually drive the engine need two things: a Godot binary (absent on a CI
//! box or a fresh clone, where those tests skip unless `GDKIT_REQUIRE_GODOT=1`) and a throwaway project
//! to point it at. Projects are generated into the temp dir rather than committed, so every run
//! starts with no `.godot/` cache — which matters when the assertion is about what the engine
//! writes into that cache.

use std::path::PathBuf;
use tokio::sync::{Mutex, MutexGuard};

// Headless editor imports and API dumps share the engine's per-user doc cache.
// Concurrent writers can crash Godot even when the test projects are disjoint.
static ENGINE_LOCK: Mutex<()> = Mutex::const_new(());

/// Discover Godot and hold the engine-test lock until the caller drops the guard.
/// Missing Godot skips locally, but fails when `GDKIT_REQUIRE_GODOT=1`.
pub async fn godot_or_skip(test: &str) -> Option<(PathBuf, MutexGuard<'static, ()>)> {
    let guard = ENGINE_LOCK.lock().await;
    match crate::config::Config::resolve().godot_bin {
        Some(p) => Some((p, guard)),
        None => {
            assert_ne!(
                std::env::var("GDKIT_REQUIRE_GODOT").as_deref(),
                Ok("1"),
                "{test}: Godot is required but was not found (set GODOT_BIN)"
            );
            eprintln!("SKIP {test}: no Godot binary found (set GODOT_BIN)");
            None
        }
    }
}

/// Create an empty temp directory unique to this process and test name.
pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gdkit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Write a throwaway Godot project: a minimal `project.godot` plus the given relative files.
pub fn temp_project(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = temp_dir(name);
    std::fs::write(
        dir.join("project.godot"),
        format!("config_version=5\n\n[application]\n\nconfig/name=\"gdkit-{name}\"\n"),
    )
    .expect("write project.godot");
    for (rel, body) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("create subdir");
        std::fs::write(path, body).expect("write fixture file");
    }
    dir
}
