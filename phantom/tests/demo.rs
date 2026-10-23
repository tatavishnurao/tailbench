//! `phantom demo`, shortened by PHANTOM_DEMO_QUICK to 2 s runs with a
//! 500:300 freeze: the naive client's own p99 misses the freeze that the
//! truth from phantom's log shows, and each run's final panel shows the same
//! numbers and ratio as the summary.

use std::process::Command;

/// The two p99s (tool, truth) on the summary row starting with `name`, in ms.
fn p99s_ms(stdout: &str, name: &str) -> (f64, f64) {
    let line = stdout
        .lines()
        .find(|l| l.trim_start().starts_with(name))
        .unwrap_or_else(|| panic!("no {name:?} row in:\n{stdout}"));
    let words: Vec<&str> = line.split_whitespace().collect();
    let values: Vec<f64> = words
        .windows(2)
        .filter_map(|w| match w[1] {
            "ms" => w[0].parse().ok(),
            "s" => w[0].parse::<f64>().ok().map(|s| s * 1e3),
            _ => None,
        })
        .collect();
    assert_eq!(values.len(), 2, "{line}");
    (values[0], values[1])
}

/// The `N` in the first `by N×` after `from` in `text`.
fn by<'a>(text: &'a str, from: &str) -> &'a str {
    let text = &text[text.find(from).unwrap_or_else(|| panic!("no {from:?} in\n{text}"))..];
    let start = text.find(" by ").unwrap_or_else(|| panic!("no ratio in\n{text}")) + 4;
    let len = text[start..].find('×').unwrap();
    &text[start..start + len]
}

#[test]
fn demo_shows_the_naive_client_missing_the_freeze() {
    let out = Command::new(env!("CARGO_BIN_EXE_phantom"))
        .arg("demo")
        .env("PHANTOM_DEMO_QUICK", "1")
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(!stdout.contains('\x1b') && !stderr.contains('\x1b'), "{stdout}{stderr}");
    let (naive, truth) = p99s_ms(&stdout, "1/2 naive client");
    assert!(truth > 10.0 * naive, "truth {truth} ms vs naive {naive} ms:\n{stdout}");
    assert!(stdout.contains("✗ under by"), "{stdout}");
    assert!(stdout.contains("next: phantom report --rate 100 "), "{stdout}");

    // The naive run's final panel (on stderr, as no terminal is attached)
    // agrees with the summary: same ratio in the verdict.
    let panel = &stderr[stderr.find("phantom · 1/2 naive client").expect(&stderr)..];
    let panel = &panel[..panel.find("phantom · 2/2").expect(panel)];
    assert!(panel.contains("your tool reports"), "{panel}");
    assert_eq!(
        by(panel, "under-report"),
        by(&stdout, "1/2 naive client"),
        "{panel}\n{stdout}"
    );

    // The logs it names exist; remove them and their directory.
    let logs: Vec<&str> = stdout
        .lines()
        .filter_map(|l| l.strip_prefix("logs: ").or_else(|| l.strip_prefix("      ")))
        .map(str::trim)
        .collect();
    assert_eq!(logs.len(), 2, "{stdout}");
    for log in &logs {
        assert!(
            std::fs::read_to_string(log)
                .unwrap()
                .starts_with(r#"{"type":"manifest""#)
        );
    }
    std::fs::remove_dir_all(std::path::Path::new(logs[0]).parent().unwrap()).unwrap();
}
