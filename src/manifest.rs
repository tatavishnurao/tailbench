//! Run manifest: the facts needed to reproduce or compare a run (code
//! version, machine, toolchain, runner parameters), serialized to JSON.
//!
//! This file exists so every run records where its numbers came from. Every
//! probed value is either read from the system or reported as "UNKNOWN";
//! nothing is guessed.

use std::env::consts::{ARCH, OS};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Serialize, Serializer};

use crate::{RunConfig, RunnerKind};

const UNKNOWN: &str = "UNKNOWN";

/// A value read from the system, or `Unknown` if reading it failed.
/// Serializes as the value itself, or as the string "UNKNOWN".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe<T> {
    Known(T),
    Unknown,
}

impl<T> From<Option<T>> for Probe<T> {
    fn from(value: Option<T>) -> Self {
        value.map_or(Probe::Unknown, Probe::Known)
    }
}

impl<T: Serialize> Serialize for Probe<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Probe::Known(value) => value.serialize(serializer),
            Probe::Unknown => serializer.serialize_str(UNKNOWN),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Manifest {
    /// tailbench crate version.
    pub tool_version: &'static str,
    /// `git rev-parse HEAD` of the tailbench source tree (CARGO_MANIFEST_DIR
    /// at build time), read at run time.
    pub git_sha: Probe<String>,
    /// Whether `git status --porcelain` lists anything, untracked files included.
    pub git_dirty: Probe<bool>,
    /// `std::env::consts::OS` / `ARCH` of the running binary.
    pub os: &'static str,
    pub arch: &'static str,
    /// `uname -r`.
    pub kernel: Probe<String>,
    pub cpu_model: Probe<String>,
    /// TAILBENCH_MACHINE_LABEL if set, else "<os>-<arch>-<n>cpu". Never the hostname.
    pub machine_label: String,
    /// `rustc --version` of the compiler that built this binary (from build.rs).
    pub rust_version: &'static str,
    pub runner: RunnerKind,
    pub interval_ns: u128,
    pub count: u64,
    pub seed: Option<u64>,
    /// Wall-clock start, nanoseconds since the Unix epoch. For the record
    /// only: no measurement uses it.
    pub start_unix_ns: Probe<u128>,
    /// Whether tokio's clock was paused (test-util) when the manifest was taken.
    pub paused_clock: bool,
}

impl Manifest {
    /// Probes the system for a run of `config`. Blocking: runs `git` and
    /// `uname`. Call it inside the tokio runtime that will drive the run, so
    /// `paused_clock` describes that runtime's clock.
    pub fn collect(config: &RunConfig) -> Self {
        let source_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        Self {
            tool_version: env!("CARGO_PKG_VERSION"),
            git_sha: git_sha(source_dir),
            git_dirty: git_dirty(source_dir),
            os: OS,
            arch: ARCH,
            kernel: command_stdout("uname", &["-r"]).into(),
            cpu_model: cpu_model(),
            machine_label: machine_label(),
            rust_version: env!("TAILBENCH_RUSTC_VERSION"),
            runner: config.kind,
            interval_ns: config.interval.as_nanos(),
            count: config.count,
            seed: config.seed,
            start_unix_ns: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos())
                .into(),
            paused_clock: clock_is_paused(),
        }
    }
}

/// Trimmed stdout of a command that exited successfully, else `None`.
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_owned())
}

/// HEAD of the repository at `dir`. Anything but a full hex SHA (SHA-1 or
/// SHA-256) is reported as Unknown.
fn git_sha(dir: &Path) -> Probe<String> {
    let dir = dir.to_str();
    dir.and_then(|dir| command_stdout("git", &["-C", dir, "rev-parse", "HEAD"]))
        .filter(|sha| (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        .into()
}

/// Whether the working tree at `dir` has changes, untracked files included.
/// `--no-optional-locks` keeps `git status` from writing the index while the
/// user may be running git themselves.
fn git_dirty(dir: &Path) -> Probe<bool> {
    let dir = dir.to_str();
    dir.and_then(|dir| command_stdout("git", &["--no-optional-locks", "-C", dir, "status", "--porcelain"]))
        .map(|status| !status.is_empty())
        .into()
}

fn cpu_model() -> Probe<String> {
    let model = if cfg!(target_os = "linux") {
        std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, model)| model.trim().to_owned())
        })
    } else if cfg!(target_os = "macos") {
        command_stdout("sysctl", &["-n", "machdep.cpu.brand_string"])
    } else {
        None
    };
    model.filter(|m| !m.is_empty()).into()
}

fn machine_label() -> String {
    if let Ok(label) = std::env::var("TAILBENCH_MACHINE_LABEL")
        && !label.is_empty()
    {
        return label;
    }
    let cpus = std::thread::available_parallelism().map_or_else(|_| UNKNOWN.to_owned(), |n| n.to_string());
    format!("{OS}-{ARCH}-{cpus}cpu")
}

/// Detects tokio's paused clock: spin for 100 us of real time without
/// yielding; a real tokio clock moves, a paused one cannot (it only advances
/// when the runtime is idle). Outside a runtime tokio uses the real clock.
fn clock_is_paused() -> bool {
    let tokio_start = tokio::time::Instant::now();
    let real_start = std::time::Instant::now();
    while real_start.elapsed() < Duration::from_micros(100) {
        std::hint::spin_loop();
    }
    tokio::time::Instant::now() == tokio_start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_serializes_as_the_string_unknown() {
        assert_eq!(serde_json::to_value(Probe::<bool>::Unknown).unwrap(), "UNKNOWN");
        assert_eq!(serde_json::to_value(Probe::Known(true)).unwrap(), true);
    }

    #[test]
    fn git_probes_report_unknown_when_git_fails() {
        let missing = Path::new("/nonexistent/tailbench-manifest-test");
        assert_eq!(git_sha(missing), Probe::Unknown);
        assert_eq!(git_dirty(missing), Probe::Unknown);
    }

    #[test]
    fn git_sha_of_this_repo_is_a_full_hex_sha() {
        let Probe::Known(sha) = git_sha(Path::new(env!("CARGO_MANIFEST_DIR"))) else {
            panic!("expected a git SHA for the tailbench source tree");
        };
        assert!(sha.len() == 40 || sha.len() == 64, "unexpected SHA length: {sha}");
        assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()), "not hex: {sha}");
    }

    #[tokio::test(start_paused = true)]
    async fn detects_paused_clock() {
        assert!(clock_is_paused());
    }

    #[tokio::test]
    async fn detects_real_clock() {
        assert!(!clock_is_paused());
    }
}
