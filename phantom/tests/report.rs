//! `phantom report` on a fixture: a closed-loop client (one connection) asked
//! for 20 requests/s while phantom stalled from 1.0 s to 1.5 s, so the
//! requests it could not send during the stall are late by up to the stall.

use std::process::Command;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/report_stall.jsonl");

#[test]
fn report_prints_the_chart_and_percentiles_and_writes_html() {
    let dir = tempfile::tempdir().unwrap();
    let html = dir.path().join("report.html");
    let out = Command::new(env!("CARGO_BIN_EXE_phantom"))
        .args(["report", "--rate", "20", FIXTURE, "--html"])
        .arg(&html)
        .env("COLUMNS", "100")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    for line in [
        "rate 20 /s (period 50.0 ms)",
        "answered 59   abandoned 1   stalls 1.0 s–1.5 s",
        "latency vs intended send time (max per column)",
        "█ server time (arrival → sent)",
        "░ open-loop truth (intended send → sent)",
    ] {
        assert!(stdout.contains(line), "no {line:?} in:\n{stdout}");
    }
    let line = |name: &str| {
        stdout
            .lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} line in:\n{stdout}"))
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    };
    // 59 answered requests: p50 is the 30th, p99 the 59th (the stalled one).
    assert_eq!(line("truth"), "1.00 ms 501 ms 501 ms 501 ms");
    assert_eq!(line("server"), "1.00 ms 501 ms 501 ms 501 ms");
    let page = std::fs::read_to_string(&html).unwrap();
    assert!(page.contains("<svg") && page.contains("p99 501 ms"), "{page}");
    assert!(!page.contains("<script"));
}

#[test]
fn report_refuses_a_bad_rate_or_log() {
    let run = |args: &[&str]| Command::new(env!("CARGO_BIN_EXE_phantom")).args(args).output().unwrap();
    let out = run(&["report", "--rate", "0", FIXTURE]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("invalid rate"));
    let out = run(&["report", "--rate", "20", "/nonexistent/gt.jsonl"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read"));
}
