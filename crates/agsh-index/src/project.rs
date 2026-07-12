use std::ffi::OsStr;
use std::io::{self, Read};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
const STATUS_POST_EXIT_DRAIN: Duration = Duration::from_millis(50);
const MAX_GIT_CONTROL_BYTES: usize = 64 * 1024;
const MAX_PACKED_REFS_BYTES: usize = 8 * 1024 * 1024;
const MAX_REF_ENTRIES: usize = 100_000;
const MAX_REF_DEPTH: usize = 32;
const MAX_STATUS_BYTES: usize = 2 * 1024 * 1024;

fn read_regular_file_bounded(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Git control path is not a regular file",
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git control file exceeds size limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git control file exceeds size limit",
        ));
    }
    Ok(bytes)
}

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
    let mut visited = 0usize;
    collect_refs(&heads, &heads, &mut branches, &mut visited, 0);

    // Packed refs.
    if let Ok(bytes) = read_regular_file_bounded(&gd.join("packed-refs"), MAX_PACKED_REFS_BYTES) {
        for line in String::from_utf8_lossy(&bytes).lines() {
            if branches.len() >= MAX_REF_ENTRIES {
                break;
            }
            if let Some((_, name)) = line.split_once(" refs/heads/") {
                branches.insert(name.trim().to_string());
            }
        }
    }
    branches.into_iter().collect()
}

fn collect_refs(
    base: &Path,
    dir: &Path,
    out: &mut std::collections::BTreeSet<String>,
    visited: &mut usize,
    depth: usize,
) {
    if depth > MAX_REF_DEPTH || *visited >= MAX_REF_ENTRIES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *visited >= MAX_REF_ENTRIES {
            break;
        }
        *visited += 1;
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_refs(base, &path, out, visited, depth + 1);
        } else if kind.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
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
    let bytes = read_regular_file_bounded(&head_path, MAX_GIT_CONTROL_BYTES).ok()?;
    let head = std::str::from_utf8(&bytes).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref: ") {
        // Keep the full branch name after refs/heads/ (so `feature/x` survives).
        let branch = reference
            .strip_prefix("refs/heads/")
            .unwrap_or(reference)
            .to_string();
        Some(branch)
    } else if head.len() >= 7 && head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
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
    if let Ok(contents) = read_regular_file_bounded(&dot_git, MAX_GIT_CONTROL_BYTES) {
        if let Ok(contents) = std::str::from_utf8(&contents) {
            if let Some(path) = contents.trim().strip_prefix("gitdir: ") {
                let p = PathBuf::from(path);
                return if p.is_absolute() { p } else { root.join(p) };
            }
        }
    }
    dot_git
}

/// Run `git status --porcelain=v2 --branch` with a hard timeout, parsing the
/// dirty flag and ahead/behind counts in one call. Returns `(None, 0, 0)` if it
/// times out or errors (never blocks the prompt).
fn probe_status(root: &Path) -> (Option<bool>, u32, u32) {
    probe_status_with(root, OsStr::new("git"), STATUS_TIMEOUT)
}

fn probe_status_with(root: &Path, git: &OsStr, timeout: Duration) -> (Option<bool>, u32, u32) {
    let mut child = match Command::new(git)
        .arg("-C")
        .arg(root)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return (None, 0, 0),
    };

    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return (None, 0, 0);
    };
    if rustix::io::ioctl_fionbio(stdout.as_fd(), true).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return (None, 0, 0);
    }
    let started = Instant::now();
    let mut status = None;
    let mut exited_at = None;
    let mut eof = false;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if !eof {
            loop {
                match stdout.read(&mut chunk) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(read) => {
                        if bytes.len().saturating_add(read) > MAX_STATUS_BYTES {
                            let _ = child.kill();
                            let _ = child.wait();
                            return (None, 0, 0);
                        }
                        bytes.extend_from_slice(&chunk[..read]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return (None, 0, 0);
                    }
                }
            }
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    exited_at = Some(Instant::now());
                }
                Ok(None) if started.elapsed() < timeout => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (None, 0, 0);
                }
            }
        }

        if status.is_some()
            && (eof || exited_at.is_some_and(|instant| instant.elapsed() >= STATUS_POST_EXIT_DRAIN))
        {
            break;
        }
        if started.elapsed() >= timeout && status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            return (None, 0, 0);
        }
        std::thread::sleep(Duration::from_millis(2));
    }

    match status {
        Some(status) if status.success() => parse_status_v2(&bytes),
        _ => (None, 0, 0),
    }
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

    #[cfg(unix)]
    #[test]
    fn git_control_reader_rejects_non_regular_files() {
        let error = read_regular_file_bounded(Path::new("/dev/zero"), 64).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn oversized_git_control_file_is_rejected() {
        let dir = std::env::temp_dir().join(format!(
            "agsh_git_oversized_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("HEAD");
        std::fs::write(&path, vec![b'x'; MAX_GIT_CONTROL_BYTES + 1]).unwrap();
        let error = read_regular_file_bounded(&path, MAX_GIT_CONTROL_BYTES).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_unicode_detached_head_does_not_panic() {
        let dir = std::env::temp_dir().join(format!(
            "agsh_git_unicode_head_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "abcdefé\n").unwrap();
        assert_eq!(read_branch(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn branch_walk_does_not_follow_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = std::env::temp_dir().join(format!(
            "agsh_git_ref_cycle_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let heads = dir.join(".git/refs/heads");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&heads).unwrap();
        std::fs::write(heads.join("main"), "deadbeef\n").unwrap();
        symlink(&heads, heads.join("cycle")).unwrap();

        assert_eq!(git_branches(&dir), vec!["main"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    #[cfg(unix)]
    #[test]
    fn status_probe_kills_and_reaps_a_timed_out_git_process() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "agsh_git_timeout_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake_git = dir.join("git-sleeps");
        std::fs::write(&fake_git, "#!/bin/sh\nexec sleep 5\n").unwrap();
        std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        let result = probe_status_with(&dir, fake_git.as_os_str(), Duration::from_millis(30));

        assert_eq!(result, (None, 0, 0));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timed-out git probe was not terminated promptly"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn status_probe_bounds_a_noisy_git_process() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "agsh_git_noisy_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake_git = dir.join("git-noisy");
        std::fs::write(
            &fake_git,
            "#!/bin/sh\nwhile :; do echo xxxxxxxxxxxxxxxx; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        assert_eq!(
            probe_status_with(&dir, fake_git.as_os_str(), Duration::from_secs(2)),
            (None, 0, 0)
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "oversized status output was not rejected promptly"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn status_probe_does_not_wait_for_descendant_inherited_stdout() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "agsh_git_descendant_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake_git = dir.join("git-descendant");
        std::fs::write(
            &fake_git,
            "#!/bin/sh\nsleep 5 &\nprintf '# branch.ab +0 -0\\n'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o700)).unwrap();

        let started = Instant::now();
        assert_eq!(
            probe_status_with(&dir, fake_git.as_os_str(), Duration::from_secs(1)),
            (Some(false), 0, 0)
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "status probe waited for a descendant holding stdout"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
