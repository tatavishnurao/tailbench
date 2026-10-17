//! The git rules shared by the library (src/manifest.rs) and build.rs, which
//! includes this file with `#[path = "src/git.rs"]`, so the build-time and
//! run-time fields are read the same way. It must stay std-only and
//! self-contained.

use std::path::Path;
use std::process::Command;

/// Variables that point git at a repository, work tree, index or object store
/// other than the one `-C` names. They are inherited when tailbench is built
/// or run from a git hook, `git rebase --exec` or some editors, and would
/// override `-C`, so every git command here runs without them.
const REPO_ENV: [&str; 5] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"];

/// Whether `sha` is a full hex object name: 40 chars (SHA-1) or 64 (SHA-256).
pub(crate) fn is_full_hex_sha(sha: &str) -> bool {
    (sha.len() == 40 || sha.len() == 64) && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Trimmed stdout of `git -C <dir> <args>` if it exited successfully, else
/// `None` (no git, not a repository, bad output).
pub(crate) fn stdout(dir: &Path, args: &[&str]) -> Option<String> {
    stdout_of(Command::new("git"), dir, args)
}

/// [`stdout`], run through `git`, a `git` command whose environment the
/// caller may already have set; tests use it to fake an inherited variable.
/// [`REPO_ENV`] is removed from it.
/// `--no-optional-locks` keeps git from writing the index while the user may
/// be running git.
pub(crate) fn stdout_of(mut git: Command, dir: &Path, args: &[&str]) -> Option<String> {
    for var in REPO_ENV {
        git.env_remove(var);
    }
    let out = git.arg("--no-optional-locks").arg("-C").arg(dir).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_owned())
}

/// Whether `dir` is the top level of the git repository that contains it.
/// When tailbench is vendored, or unpacked from crates.io, inside another
/// project's repository, git finds that parent repository: its HEAD and
/// status say nothing about tailbench's source. So git is trusted only when
/// the repository is tailbench's own. Both paths are canonicalized so a
/// symlink or a different spelling of the same directory still matches.
pub(crate) fn is_repo_top_level(dir: &Path) -> bool {
    let Some(top) = stdout(dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (std::fs::canonicalize(top), std::fs::canonicalize(dir)) {
        (Ok(top), Ok(dir)) => top == dir,
        _ => false,
    }
}
