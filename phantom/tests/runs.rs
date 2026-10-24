//! `phantom report` on a log from a phantom that served two runs 6 s apart:
//! truth is scored per run, so the idle gap is not lateness.

use std::fmt::Write as _;
use std::process::Command;

/// Two 3 s runs at 100 req/s, 6 s apart, 4 ms service.
fn two_runs() -> String {
    let mut log = String::from(r#"{"type":"manifest","stalls":[]}"#);
    log.push('\n');
    let mut seq = 0;
    for from_ms in [0u64, 9_000] {
        for t in (from_ms..from_ms + 3_000).step_by(10) {
            let arrival = t * 1_000_000;
            let _ = writeln!(
                log,
                r#"{{"type":"req","seq":{seq},"arrival_ns":{arrival},"sent_ns":{}}}"#,
                arrival + 4_000_000
            );
            seq += 1;
        }
    }
    log
}

fn report(log: &str, rate: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.jsonl");
    std::fs::write(&path, log).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_phantom"))
        .args(["report", "--rate", rate])
        .arg(&path)
        .env("COLUMNS", "300")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn two_runs_on_one_phantom_are_not_six_seconds_late() {
    let out = report(&two_runs(), "100");
    let row = |name: &str| {
        out.lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} row in\n{out}"))
            .split_whitespace()
            .skip(1)
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(row("truth"), "4.00 ms 4.00 ms 4.00 ms 4.00 ms", "{out}");
    assert_eq!(row("server"), "4.00 ms 4.00 ms 4.00 ms 4.00 ms", "{out}");
    assert!(out.contains("runs 2"), "{out}");
    assert!(
        out.replace("\n  ", " ").contains(
            "2 separate runs found (gaps of 6.0 s); truth is computed per run. \
For a clean result use a fresh phantom per run."
        ),
        "{out}"
    );
    assert!(out.lines().all(|l| l.chars().count() <= 100), "{out}");
}

#[test]
fn a_rate_far_from_the_logs_is_warned_about_with_both_numbers() {
    let out = report(&two_runs(), "50");
    assert!(
        out.contains("warning: the log's arrival rate is 100 /s but --rate is 50 /s; truth assumes --rate"),
        "{out}"
    );
    assert!(!report(&two_runs(), "100").contains("warning"));
}

#[test]
fn a_log_still_being_written_is_read_up_to_its_last_whole_line() {
    let mut log = two_runs();
    log.push_str(r#"{"type":"req","seq":600,"arrival_ns":12"#);
    let out = report(&log, "100");
    assert!(out.contains("ignored 1 incomplete final line"), "{out}");
    assert!(out.contains("answered 600"), "{out}");
}
