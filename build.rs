//! Build script: records how this binary was built, so the run manifest
//! reports that rather than whatever is true when the binary runs:
//! - the rustc that compiles tailbench, not whatever `rustc` is on PATH later;
//! - the cargo profile and opt-level it is compiled with;
//! - the git commit of the source tree, and whether the build-relevant files
//!   differed from it, not whatever is checked out later.
//!
//! It must never fail the build: anything it cannot read is "UNKNOWN".

use std::path::PathBuf;
use std::process::Command;

// The git rules src/manifest.rs uses, from the same file, so the two agree.
#[path = "src/git.rs"]
mod git;

const UNKNOWN: &str = "UNKNOWN";

/// The files that change the compiled library. Both the dirty flag and the
/// rerun triggers use this list, so "dirty" means "this binary differs from
/// the commit", not "something in the repo changed".
const BUILD_INPUTS: [&str; 4] = ["src", "build.rs", "Cargo.toml", "Cargo.lock"];

fn main() {
    // Cargo sets RUSTC to the compiler it is building with.
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let version = Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "UNKNOWN".to_owned());
    println!("cargo:rustc-env=TAILBENCH_RUSTC_VERSION={version}");
    println!("cargo:rerun-if-env-changed=RUSTC");

    // Cargo sets both for build scripts. PROFILE is only "release" or
    // "debug" (whichever of release or dev the profile inherits from, so a
    // custom profile reports its root); OPT_LEVEL is the profile's actual
    // opt-level. Left unset if missing, so the manifest says UNKNOWN.
    for (var, key) in [
        ("PROFILE", "TAILBENCH_BUILD_PROFILE"),
        ("OPT_LEVEL", "TAILBENCH_OPT_LEVEL"),
    ] {
        if let Ok(value) = std::env::var(var) {
            println!("cargo:rustc-env={key}={value}");
        }
    }

    // Cargo always sets it; it is also this script's working directory.
    let dir = std::env::var_os("CARGO_MANIFEST_DIR").map_or_else(|| PathBuf::from("."), PathBuf::from);

    // Git is only asked about a repository whose top level is this crate.
    // Vendored, or unpacked from crates.io, inside another project's
    // repository, git would describe that parent instead; then the SHA and
    // dirty flag are UNKNOWN and no git file is watched below.
    let repo = Some(dir.as_path()).filter(|dir| git::is_repo_top_level(dir));

    let sha = repo
        .and_then(|dir| git::stdout(dir, &["rev-parse", "HEAD"]))
        .filter(|sha| git::is_full_hex_sha(sha))
        .unwrap_or_else(|| UNKNOWN.to_owned());
    println!("cargo:rustc-env=TAILBENCH_BUILD_GIT_SHA={sha}");

    let mut status = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    status.extend(BUILD_INPUTS);
    let dirty = repo
        .and_then(|dir| git::stdout(dir, &status))
        .map_or(UNKNOWN, |out| if out.is_empty() { "false" } else { "true" });
    println!("cargo:rustc-env=TAILBENCH_BUILD_GIT_DIRTY={dirty}");

    // Rerun on any change to the build inputs. Cargo scans a directory
    // recursively for the newest mtime, so an edit anywhere under src/ reruns
    // this script and refreshes the dirty flag; otherwise a build after an
    // edit would keep the stale "false" from the last clean build. Listing
    // any path turns off Cargo's default (rerun when any package file
    // changes), so this list plus the git files below is all that reruns us.
    //
    // Never list the package root or target/: every build writes to target/,
    // so the script would be stale after each build and rerun forever.
    for path in BUILD_INPUTS {
        println!("cargo:rerun-if-changed={path}");
    }

    let Some(repo) = repo else { return };
    // Rerun when HEAD moves, so a new commit or checkout refreshes the SHA.
    // Paths come from `git rev-parse --git-path`, never a hardcoded
    // `.git/...`: in a worktree or submodule .git is a file pointing
    // elsewhere, and refs can live in a common dir shared between worktrees.
    // - HEAD: rewritten by checkout/switch (to a branch or a detached commit).
    // - the current branch's ref (e.g. refs/heads/main): moved by commit,
    //   reset, merge, pull without touching HEAD. Skipped when detached.
    // - packed-refs: holds the branch ref instead when it has no loose file.
    // - index: rewritten by commit, checkout and reset.
    // - logs/HEAD: the HEAD reflog, appended whenever HEAD's commit changes.
    //   Covers commits on packed-only branch refs (the new loose ref file was
    //   not watched when this script last ran) and reset --soft (which
    //   rewrites neither HEAD nor the index).
    let mut git_files = vec![
        "HEAD".to_owned(),
        "index".to_owned(),
        "packed-refs".to_owned(),
        "logs/HEAD".to_owned(),
    ];
    git_files
        .extend(git::stdout(repo, &["rev-parse", "--symbolic-full-name", "HEAD"]).filter(|r| r.starts_with("refs/")));
    for name in &git_files {
        let Some(path) = git::stdout(repo, &["rev-parse", "--git-path", name]) else {
            continue;
        };
        // Relative paths are relative to `repo`; join keeps an absolute one.
        let path = repo.join(path);
        // Only watch files that exist. Cargo treats a missing path as always
        // changed, so without a loose ref file or a reflog, every build
        // would rerun this script and recompile.
        if let Some(path) = path.to_str().filter(|_| path.exists()) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}
