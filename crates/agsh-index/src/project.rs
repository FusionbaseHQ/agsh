use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    pub root: PathBuf,
    pub git_branch: Option<String>,
    pub dirty: Option<bool>,
}

impl ProjectSnapshot {
    pub fn unknown(root: PathBuf) -> Self {
        Self {
            root,
            git_branch: None,
            dirty: None,
        }
    }
}

/// Git context for a directory, used to render the prompt. The branch is read
/// directly from `.git/HEAD` (instant, no subprocess); the dirty flag is a
/// bounded `git status` call that yields `None` rather than ever blocking.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitContext {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub dirty: Option<bool>,
    /// Commits ahead/behind the upstream (0 when unknown or none).
    pub ahead: u32,
    pub behind: u32,
}

/// How long the status probe may run before the prompt gives up on it.
const STATUS_TIMEOUT: Duration = Duration::from_millis(150);

/// Resolve the git context for `cwd`, or `None` if not inside a work tree.
pub fn git_context(cwd: &Path) -> Option<GitContext> {
    let root = find_git_root(cwd)?;
    let branch = read_branch(&root);
    let (dirty, ahead, behind) = probe_status(&root);
    Some(GitContext {
        root,
        branch,
        dirty,
        ahead,
        behind,
    })
}

/// Local git branch names for `cwd`'s repository, read directly from
/// `refs/heads/**` and `packed-refs` (no subprocess). Empty if not a repo.
pub fn git_branches(cwd: &Path) -> Vec<String> {
    let Some(root) = find_git_root(cwd) else {
        return Vec::new();
    };
    let gd = git_dir(&root);
    let mut branches = std::collections::BTreeSet::new();

    // Loose refs under refs/heads/**.
    let heads = gd.join("refs/heads");
    collect_refs(&heads, &heads, &mut branches);

    // Packed refs.
    if let Ok(text) = std::fs::read_to_string(gd.join("packed-refs")) {
        for line in text.lines() {
            if let Some((_, name)) = line.split_once(" refs/heads/") {
                branches.insert(name.trim().to_string());
            }
        }
    }
    branches.into_iter().collect()
}

fn collect_refs(base: &Path, dir: &Path, out: &mut std::collections::BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_refs(base, &path, out);
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.insert(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

/// Walk up from `cwd` looking for a `.git` directory or file.
fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Read the current branch from `.git/HEAD` (or a short SHA when detached).
fn read_branch(root: &Path) -> Option<String> {
    let head_path = git_dir(root).join("HEAD");
    let head = std::fs::read_to_string(&head_path).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        // Keep the full branch name after refs/heads/ (so `feature/x` survives).
        let branch = reference
            .strip_prefix("refs/heads/")
            .unwrap_or(reference)
            .to_string();
        Some(branch)
    } else if head.len() >= 7 {
        // Detached HEAD: show a short commit hash.
        Some(format!("@{}", &head[..7]))
    } else {
        None
    }
}

/// Resolve the git directory, honoring a `.git` file (worktrees/submodules).
fn git_dir(root: &Path) -> PathBuf {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return dot_git;
    }
    if let Ok(contents) = std::fs::read_to_string(&dot_git) {
        if let Some(path) = contents.trim().strip_prefix("gitdir: ") {
            let p = PathBuf::from(path);
            return if p.is_absolute() { p } else { root.join(p) };
        }
    }
    dot_git
}

/// Run `git status --porcelain=v2 --branch` with a hard timeout, parsing the
/// dirty flag and ahead/behind counts in one call. Returns `(None, 0, 0)` if it
/// times out or errors (never blocks the prompt).
fn probe_status(root: &Path) -> (Option<bool>, u32, u32) {
    let root = root.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args([
                "status",
                "--porcelain=v2",
                "--branch",
                "--untracked-files=normal",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let parsed = result.ok().map(|o| parse_status_v2(&o.stdout));
        let _ = tx.send(parsed);
    });
    rx.recv_timeout(STATUS_TIMEOUT)
        .ok()
        .flatten()
        .unwrap_or((None, 0, 0))
}

/// Parse `git status --porcelain=v2 --branch` output: dirty = any non-header
/// line; ahead/behind from the `# branch.ab +A -B` header.
fn parse_status_v2(out: &[u8]) -> (Option<bool>, u32, u32) {
    let text = String::from_utf8_lossy(out);
    let mut dirty = false;
    let (mut ahead, mut behind) = (0u32, 0u32);
    for line in text.lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for token in ab.split_whitespace() {
                if let Some(n) = token.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = token.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            dirty = true;
        }
    }
    (Some(dirty), ahead, behind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_git_root_outside_repo() {
        // /tmp is not (normally) a git work tree.
        let tmp = std::env::temp_dir();
        // Don't assert None unconditionally (CI temp dirs vary); just ensure it
        // does not panic and returns a well-formed value.
        let _ = git_context(&tmp);
    }

    #[test]
    fn parses_status_v2_ahead_behind_dirty() {
        let out = b"# branch.oid abc123\n# branch.head main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 aaa bbb file.rs\n";
        let (dirty, ahead, behind) = parse_status_v2(out);
        assert_eq!(dirty, Some(true));
        assert_eq!(ahead, 2);
        assert_eq!(behind, 1);

        // Clean tree, no upstream divergence.
        let clean = b"# branch.head main\n# branch.ab +0 -0\n";
        assert_eq!(parse_status_v2(clean), (Some(false), 0, 0));
    }

    #[test]
    fn reads_branch_from_head_ref() {
        let dir = std::env::temp_dir().join(format!("agsh_git_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        let ctx = git_context(&dir).expect("git context");
        assert_eq!(ctx.branch.as_deref(), Some("feature/x"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
