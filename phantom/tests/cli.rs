//! CLI tests: an invalid script is refused with a message naming the problem,
//! a non-zero exit, and no log file, before anything is bound or written.

use std::process::{Command, Output};

fn phantom(args: &[&str]) -> (Output, bool) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("gt.jsonl");
    let out = Command::new(env!("CARGO_BIN_EXE_phantom"))
        .args(["--listen", "127.0.0.1:0", "--log"])
        .arg(&log)
        .args(args)
        .output()
        .unwrap();
    (out, log.exists())
}

fn assert_refused(args: &[&str], mentions: &str) {
    let (out, log_written) = phantom(args);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{args:?} was accepted; stderr: {stderr}");
    assert!(
        stderr.contains(mentions),
        "{args:?}: stderr does not mention {mentions:?}: {stderr}"
    );
    assert!(!log_written, "{args:?}: a log was written for a refused config");
}

#[test]
fn invalid_specs_are_refused() {
    assert_refused(&["--service-ms", "0"], "--service-ms");
    assert_refused(&["--service-ms", "-5"], "--service-ms");
    assert_refused(&["--service-ms", "fast"], "fast");
    assert_refused(&["--lognormal", "20"], "20");
    assert_refused(&["--lognormal", "20:0"], "20:0");
    assert_refused(&["--lognormal", "-20:0.5"], "-20:0.5");
    assert_refused(&["--service-ms", "20", "--stall", "100:0"], "100:0");
    assert_refused(&["--service-ms", "20", "--stall", "100"], "100");
    assert_refused(&["--service-ms", "20", "--seed", "-1"], "--seed");
}

#[test]
fn exactly_one_service_spec_is_required() {
    assert_refused(&[], "--service-ms");
    assert_refused(&["--service-ms", "20", "--lognormal", "20:0.5"], "--lognormal");
}

#[test]
fn overlapping_stalls_are_refused() {
    assert_refused(
        &["--service-ms", "20", "--stall", "100:300", "--stall", "200:50"],
        "overlap",
    );
}
