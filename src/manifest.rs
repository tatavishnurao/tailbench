//! Run manifest: the facts needed to reproduce or compare a run (code
//! version, machine, toolchain, runner parameters), serialized to JSON.
//!
//! This file exists so every run records where its numbers came from. Every
//! probed value is either read from the system or reported as "UNKNOWN";
//! nothing is guessed.

use std::collections::BTreeMap;
use std::env::consts::{ARCH, OS};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Serialize, Serializer};
use tokio::runtime::{Handle, RuntimeFlavor};

use crate::git::{self, is_full_hex_sha};
use crate::{RunConfig, RunnerKind};

const UNKNOWN: &str = "UNKNOWN";

/// A value read from the system, or `Unknown` if reading it failed.
/// Serializes as the value itself, or as the string "UNKNOWN".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe<T> {
    /// The value read.
    Known(T),
    /// Reading it failed.
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

/// The run manifest: the build, the machine and the run's parameters,
/// written as JSON before the first request.
#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    /// tailbench crate version.
    pub tool_version: &'static str,
    /// `git rev-parse HEAD` of the tailbench source tree when this binary was
    /// built (from build.rs): the commit the code came from. Unknown when, at
    /// build time, the crate dir was not the top level of its own repository
    /// (e.g. vendored inside another project's repository), git was missing,
    /// or the output was not a full hex object name.
    pub build_git_sha: Probe<String>,
    /// Whether, at build time, `git status --porcelain` listed anything
    /// (untracked files included) under the build-relevant paths only: src/,
    /// build.rs, Cargo.toml, Cargo.lock. true means this binary differs from
    /// `build_git_sha`. Unknown when, at build time, the crate dir was not
    /// the top level of its own repository or git was missing or failed.
    pub build_git_dirty: Probe<bool>,
    /// `git rev-parse HEAD` of the tailbench source tree (CARGO_MANIFEST_DIR
    /// at build time), read at run time. A cross-check: it describes the
    /// checkout now, which may have moved since the build. Unknown when the
    /// crate dir is not the top level of its own repository, git is missing,
    /// or the output is not a full hex object name.
    pub runtime_git_sha: Probe<String>,
    /// Whether `git status --porcelain` lists anything at run time, across the
    /// whole repository, untracked files included. Unlike `build_git_dirty`,
    /// it also counts files that do not change the binary. Unknown when the
    /// crate dir is not the top level of its own repository or git is
    /// missing or fails.
    pub runtime_git_dirty: Probe<bool>,
    /// Whether `build_git_sha` and `runtime_git_sha` differ: true means the
    /// checkout moved after this binary was built. Unknown unless both are known.
    pub sha_mismatch: Probe<bool>,
    /// `std::env::consts::OS` / `ARCH` of the running binary.
    pub os: &'static str,
    /// See `os`.
    pub arch: &'static str,
    /// `uname -r`.
    pub kernel: Probe<String>,
    /// The CPU's model name: `model name` in /proc/cpuinfo on Linux,
    /// `sysctl -n machdep.cpu.brand_string` on macOS; Unknown elsewhere.
    pub cpu_model: Probe<String>,
    /// TAILBENCH_MACHINE_LABEL if set, else `"<os>-<arch>-<n>cpu"`. Never the hostname.
    pub machine_label: String,
    /// `rustc --version` of the compiler that built this binary (from build.rs).
    pub rust_version: &'static str,
    /// Cargo's PROFILE when this binary was built: "release" or "debug",
    /// whichever the profile inherits from (a custom profile reports that,
    /// not its own name). Unknown if cargo did not set it.
    pub build_profile: Probe<String>,
    /// Cargo's OPT_LEVEL when this binary was built ("0".."3", "s", "z"): the
    /// profile's real optimization level. Unknown if cargo did not set it.
    pub opt_level: Probe<String>,
    /// `RunConfig::kind`.
    pub runner: RunnerKind,
    /// `RunConfig::interval`, in nanoseconds.
    pub interval_ns: u128,
    /// `RunConfig::count`.
    pub count: u64,
    /// `RunConfig::seed`.
    pub seed: Option<u64>,
    /// Wall-clock start, nanoseconds since the Unix epoch. For the record
    /// only: no measurement uses it.
    pub start_unix_ns: Probe<u128>,
    /// Whether tokio's clock was paused (test-util) when the manifest was taken.
    pub paused_clock: bool,
    /// Flavor of the tokio runtime the manifest was collected on (the one
    /// `run` drives the runner on): "current_thread" or "multi_thread".
    /// Unknown outside a runtime, or for a flavor tokio adds later.
    pub runtime_flavor: Probe<String>,
    /// That runtime's worker threads (`RuntimeMetrics::num_workers`, stable
    /// in tokio; always 1 on current_thread). Unknown outside a runtime.
    pub runtime_workers: Probe<u64>,
    /// `RunConfig::extra`, keys sorted.
    pub extra: BTreeMap<String, String>,
}

impl Manifest {
    /// Probes the system for a run of `config`. Blocking: runs `git` and
    /// `uname`. Call it inside the tokio runtime that will drive the run, so
    /// `paused_clock` describes that runtime's clock.
    pub fn collect(config: &RunConfig) -> Self {
        let build_git_sha = parse_build_sha(env!("TAILBENCH_BUILD_GIT_SHA"));
        let (runtime_git_sha, runtime_git_dirty) = runtime_git(Path::new(env!("CARGO_MANIFEST_DIR")));
        let mismatch = sha_mismatch(&build_git_sha, &runtime_git_sha);
        let (runtime_flavor, runtime_workers) = tokio_runtime();
        Self {
            tool_version: env!("CARGO_PKG_VERSION"),
            build_git_sha,
            build_git_dirty: parse_build_dirty(env!("TAILBENCH_BUILD_GIT_DIRTY")),
            runtime_git_sha,
            runtime_git_dirty,
            sha_mismatch: mismatch,
            os: OS,
            arch: ARCH,
            kernel: command_stdout("uname", &["-r"]).into(),
            cpu_model: cpu_model(),
            machine_label: machine_label(),
            rust_version: env!("TAILBENCH_RUSTC_VERSION"),
            build_profile: option_env!("TAILBENCH_BUILD_PROFILE").map(str::to_owned).into(),
            opt_level: option_env!("TAILBENCH_OPT_LEVEL").map(str::to_owned).into(),
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
            runtime_flavor,
            runtime_workers,
            extra: config.extra.clone(),
        }
    }
}

/// Flavor and worker count of the tokio runtime this is called on, or
/// Unknown for both outside a runtime.
fn tokio_runtime() -> (Probe<String>, Probe<u64>) {
    let Ok(handle) = Handle::try_current() else {
        return (Probe::Unknown, Probe::Unknown);
    };
    let flavor = match handle.runtime_flavor() {
        RuntimeFlavor::CurrentThread => Probe::Known("current_thread".to_owned()),
        RuntimeFlavor::MultiThread => Probe::Known("multi_thread".to_owned()),
        // RuntimeFlavor is #[non_exhaustive].
        _ => Probe::Unknown,
    };
    let workers = u64::try_from(handle.metrics().num_workers()).ok().into();
    (flavor, workers)
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

/// Run-time `(git_sha, git_dirty)` of the tailbench source tree at `dir`.
/// Like build.rs, git is asked only if `dir` is the top level of its own
/// repository, not a parent repository tailbench was vendored or unpacked
/// into; otherwise both are Unknown.
pub(crate) fn runtime_git(dir: &Path) -> (Probe<String>, Probe<bool>) {
    if !git::is_repo_top_level(dir) {
        return (Probe::Unknown, Probe::Unknown);
    }
    (git_sha(dir), git_dirty(dir))
}

/// HEAD of the repository at `dir`, lowercased. Anything but a full hex SHA
/// (SHA-1 or SHA-256) is reported as Unknown.
fn git_sha(dir: &Path) -> Probe<String> {
    git::stdout(dir, &["rev-parse", "HEAD"])
        .and_then(|sha| normalize_sha(&sha))
        .into()
}

/// Whether the working tree at `dir` has changes anywhere in the repository,
/// untracked files included.
fn git_dirty(dir: &Path) -> Probe<bool> {
    git::stdout(dir, &["status", "--porcelain"])
        .map(|status| !status.is_empty())
        .into()
}

/// TAILBENCH_BUILD_GIT_SHA from build.rs, lowercased. "UNKNOWN" and
/// anything else that is not a full hex SHA are Unknown.
fn parse_build_sha(raw: &str) -> Probe<String> {
    normalize_sha(raw).into()
}

/// `sha` in lowercase if it is a full hex SHA, else `None`. Both SHA fields
/// go through this, so they can be compared as plain strings.
fn normalize_sha(sha: &str) -> Option<String> {
    Some(sha)
        .filter(|sha| is_full_hex_sha(sha))
        .map(str::to_ascii_lowercase)
}

/// TAILBENCH_BUILD_GIT_DIRTY from build.rs. Only "true" and "false" are
/// known; "UNKNOWN" or anything else is Unknown, never false.
fn parse_build_dirty(raw: &str) -> Probe<bool> {
    match raw {
        "true" => Probe::Known(true),
        "false" => Probe::Known(false),
        _ => Probe::Unknown,
    }
}

/// Whether the build-time and run-time commits differ. Unknown unless both
/// are known. Both are lowercased when parsed, so plain equality is enough.
fn sha_mismatch(build: &Probe<String>, runtime: &Probe<String>) -> Probe<bool> {
    match (build, runtime) {
        (Probe::Known(build), Probe::Known(runtime)) => Probe::Known(build != runtime),
        _ => Probe::Unknown,
    }
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
    use std::path::PathBuf;

    use super::*;

    /// Whether `git` can be run at all.
    fn git_installed() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|out| out.status.success())
    }

    /// Whether `test` may assume the crate dir is the top level of its own
    /// repository, as in a tailbench checkout. In a vendored copy or a
    /// crates.io tarball it is not, so the test prints why and skips.
    fn in_own_repo(test: &str) -> bool {
        let reason = if !git_installed() {
            "git is not installed"
        } else if !git::is_repo_top_level(Path::new(env!("CARGO_MANIFEST_DIR"))) {
            "crate dir is not the top level of its own git repository"
        } else {
            return true;
        };
        eprintln!("skipping: {test}: {reason}");
        false
    }

    /// A new directory under the system temp dir, removed on drop (so also
    /// when the test panics). `create_dir` fails rather than reuse a path.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
            let name = format!("tailbench-{tag}-{}-{nanos}", std::process::id());
            let path = std::env::temp_dir().join(name);
            std::fs::create_dir(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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
        if !in_own_repo("git_sha_of_this_repo_is_a_full_hex_sha") {
            return;
        }
        let Probe::Known(sha) = git_sha(Path::new(env!("CARGO_MANIFEST_DIR"))) else {
            panic!("expected a git SHA for the tailbench source tree");
        };
        assert!(sha.len() == 40 || sha.len() == 64, "unexpected SHA length: {sha}");
        assert!(sha.bytes().all(|b| b.is_ascii_hexdigit()), "not hex: {sha}");
    }

    #[test]
    fn git_ignores_an_inherited_git_dir() {
        if !in_own_repo("git_ignores_an_inherited_git_dir") {
            return;
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut child = Command::new("git");
        child.env("GIT_DIR", "/nonexistent/tailbench-bogus-git-dir");
        let sha = git::stdout_of(child, dir, &["rev-parse", "HEAD"]);
        assert_eq!(Probe::from(sha), git_sha(dir), "-C must win over GIT_DIR");
        assert!(matches!(git_sha(dir), Probe::Known(_)), "expected this repo's SHA");
    }

    #[test]
    fn this_crate_is_the_top_level_of_its_repo() {
        if !in_own_repo("this_crate_is_the_top_level_of_its_repo") {
            return;
        }
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(git::is_repo_top_level(dir));
        // src/ is inside the same repository but is not its top level.
        assert!(!git::is_repo_top_level(&dir.join("src")));
        assert!(!git::is_repo_top_level(Path::new(
            "/nonexistent/tailbench-manifest-test"
        )));
    }

    #[test]
    fn build_git_sha_of_this_repo_is_a_full_hex_sha() {
        if !in_own_repo("build_git_sha_of_this_repo_is_a_full_hex_sha") {
            return;
        }
        let Probe::Known(sha) = parse_build_sha(env!("TAILBENCH_BUILD_GIT_SHA")) else {
            panic!("expected build.rs to record a git SHA for the tailbench source tree");
        };
        assert!(is_full_hex_sha(&sha), "not a full hex SHA: {sha}");
    }

    #[test]
    fn runtime_git_is_unknown_inside_a_parent_repo() {
        if !git_installed() {
            eprintln!("skipping: runtime_git_is_unknown_inside_a_parent_repo: git is not installed");
            return;
        }
        let parent = TempDir::new("parent-repo");
        // git::stdout drops inherited GIT_DIR etc., so this cannot touch the
        // repository the tests are run from.
        let git = |args: &[&str]| git::stdout(&parent.0, args).unwrap_or_else(|| panic!("git {args:?} failed"));
        git(&["init", "-q"]);
        std::fs::write(parent.0.join("file"), "x").expect("write file");
        git(&["add", "file"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            "init",
        ]);
        let parent_sha = git(&["rev-parse", "HEAD"]);
        // Stands in for tailbench vendored into another project's repository.
        let vendored = parent.0.join("vendor").join("tailbench");
        std::fs::create_dir_all(&vendored).expect("create vendored dir");
        // Precondition: asked directly, git reports the parent's commit here.
        assert_eq!(git_sha(&vendored), Probe::Known(parent_sha.clone()));

        let (sha, dirty) = runtime_git(&vendored);
        assert_eq!(sha, Probe::Unknown, "parent repo's SHA {parent_sha} leaked");
        assert_eq!(dirty, Probe::Unknown, "parent repo's status leaked");
    }

    const SHA1: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA256: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF";

    #[test]
    fn parse_build_dirty_accepts_only_true_and_false() {
        assert_eq!(parse_build_dirty("true"), Probe::Known(true));
        assert_eq!(parse_build_dirty("false"), Probe::Known(false));
        // Anything else is Unknown, never Known(false).
        assert_eq!(parse_build_dirty("UNKNOWN"), Probe::Unknown);
        assert_eq!(parse_build_dirty(""), Probe::Unknown);
        assert_eq!(parse_build_dirty("garbage"), Probe::Unknown);
    }

    #[test]
    fn parse_build_sha_accepts_only_full_hex_shas() {
        assert_eq!(parse_build_sha(SHA1), Probe::Known(SHA1.to_owned()));
        assert_eq!(parse_build_sha(SHA256), Probe::Known(SHA256.to_ascii_lowercase()));
        assert_eq!(parse_build_sha(&SHA1[..7]), Probe::Unknown, "short");
        assert_eq!(parse_build_sha(&SHA1.replace('a', "g")), Probe::Unknown, "non-hex");
        assert_eq!(parse_build_sha("UNKNOWN"), Probe::Unknown);
    }

    #[test]
    fn parse_build_sha_lowercases() {
        let upper = SHA1.to_ascii_uppercase();
        assert_eq!(parse_build_sha(&upper), Probe::Known(SHA1.to_owned()));
        // So an uppercase build SHA still matches git's lowercase output.
        assert_eq!(
            sha_mismatch(&parse_build_sha(&upper), &Probe::Known(SHA1.to_owned())),
            Probe::Known(false)
        );
    }

    #[test]
    fn sha_mismatch_needs_both_shas() {
        let a = Probe::Known(SHA1.to_owned());
        let b = Probe::Known(SHA1.replace('a', "b"));
        assert_eq!(sha_mismatch(&a, &a), Probe::Known(false), "equal");
        assert_eq!(sha_mismatch(&a, &b), Probe::Known(true), "different");
        assert_eq!(sha_mismatch(&a, &Probe::Unknown), Probe::Unknown, "runtime unknown");
        assert_eq!(sha_mismatch(&Probe::Unknown, &a), Probe::Unknown, "build unknown");
        assert_eq!(
            sha_mismatch(&Probe::Unknown, &Probe::Unknown),
            Probe::Unknown,
            "both unknown"
        );
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
