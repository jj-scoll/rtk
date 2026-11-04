#![cfg(unix)]
//! The Claude Code plugin in `claude-plugin/`: its SessionStart hook gives Claude the rtk
//! awareness text and tells the user when the rtk on PATH cannot condense output.
//! Each test runs the real script with a PATH that holds only a fake rtk and the tools
//! the script uses, so the rtk installed on this machine never answers.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn plugin(rel: &str) -> PathBuf {
    Path::new(ROOT).join("claude-plugin").join(rel)
}

/// The absolute path of `name` on the test's own PATH.
fn which(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH"))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} not on PATH"))
}

/// A bin dir with the tools the script needs and, when `rtk` is Some, a fake rtk with
/// that shell body.
fn bin_dir(rtk: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for tool in ["awk", "cat"] {
        std::os::unix::fs::symlink(which(tool), dir.path().join(tool)).expect("symlink");
    }
    if let Some(body) = rtk {
        let path = dir.path().join("rtk");
        fs::write(&path, format!("#!{}\n{body}\n", which("sh").display())).expect("write rtk");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    dir
}

/// Runs the hook with PATH set to `bin` and returns its parsed JSON reply.
fn run_hook(bin: &Path) -> serde_json::Value {
    let out = Command::new(which("sh"))
        .arg(plugin("scripts/session-start.sh"))
        .env_clear()
        .env("PATH", bin)
        .output()
        .expect("run hook");
    assert!(
        out.status.success(),
        "hook must exit 0, got {:?}",
        out.status
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn context(reply: &serde_json::Value) -> Option<&str> {
    reply["hookSpecificOutput"]["additionalContext"].as_str()
}

fn message(reply: &serde_json::Value) -> Option<&str> {
    reply["systemMessage"].as_str()
}

fn awareness() -> String {
    fs::read_to_string(Path::new(ROOT).join("hooks/rtk-awareness.md")).expect("awareness")
}

const FORK: &str = r#"case "$1" in gain) exit 0;; duckdb) exit 0;; esac; exit 1"#;
const UPSTREAM: &str = r#"case "$1" in gain) exit 0;; esac; exit 2"#;
const OTHER_RTK: &str = "exit 1";

// The plugin is fetched alone (git-subdir sparse checkout), so it carries its own copy
// of the awareness text; this keeps the copy equal to the one `rtk init` embeds.
#[test]
fn plugin_awareness_copy_matches_default_level() {
    let copy = fs::read_to_string(plugin("awareness.md")).expect("claude-plugin/awareness.md");
    assert_eq!(copy, awareness());
}

#[test]
fn fork_rtk_gets_awareness_and_no_message() {
    let bin = bin_dir(Some(FORK));
    let reply = run_hook(bin.path());
    assert_eq!(reply["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert_eq!(context(&reply), Some(awareness().trim_end()));
    assert_eq!(message(&reply), None);
}

#[test]
fn missing_rtk_gets_message_and_no_awareness() {
    let bin = bin_dir(None);
    let reply = run_hook(bin.path());
    assert_eq!(context(&reply), None);
    assert!(
        message(&reply).is_some_and(|m| m.contains("not on PATH")),
        "{reply}"
    );
}

#[test]
fn rtk_that_fails_gain_gets_message_and_no_awareness() {
    let bin = bin_dir(Some(OTHER_RTK));
    let reply = run_hook(bin.path());
    assert_eq!(context(&reply), None);
    assert!(
        message(&reply).is_some_and(|m| m.contains("rtk gain")),
        "{reply}"
    );
}

#[test]
fn upstream_rtk_gets_awareness_and_fork_message() {
    let bin = bin_dir(Some(UPSTREAM));
    let reply = run_hook(bin.path());
    assert_eq!(context(&reply), Some(awareness().trim_end()));
    assert!(
        message(&reply).is_some_and(|m| m.contains("jj-scoll/rtk")),
        "{reply}"
    );
}

#[test]
fn hooks_json_runs_the_script_at_session_start() {
    let hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(plugin("hooks/hooks.json")).expect("hooks.json"))
            .expect("hooks.json is JSON");
    let cmd = hooks["hooks"]["SessionStart"][0]["hooks"][0]["command"]
        .as_str()
        .expect("SessionStart command");
    assert_eq!(
        cmd,
        r#"sh "${CLAUDE_PLUGIN_ROOT}/scripts/session-start.sh""#
    );
}
