//! Broker filesystem layout: socket, job logs, daemon log.
//!
//! Everything lives under one 0700 directory: `$AGSH_BROKER_DIR`, else
//! `$XDG_STATE_HOME/agsh/broker`, else `~/.local/state/agsh/broker`. The
//! socket path must stay short (~104-byte `sun_path` limit on macOS), which
//! these defaults comfortably satisfy.

use std::path::{Path, PathBuf};

/// The broker's state directory (not created here; see [`ensure_dir`]).
pub fn broker_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AGSH_BROKER_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return Some(Path::new(&xdg).join("agsh/broker"));
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".local/state/agsh/broker"))
}

/// The control/attach socket: `$AGSH_BROKER_SOCKET` or `<broker_dir>/agshd.sock`.
pub fn socket_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AGSH_BROKER_SOCKET").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    Some(broker_dir()?.join("agshd.sock"))
}

/// Where job output logs live.
pub fn logs_dir() -> Option<PathBuf> {
    Some(broker_dir()?.join("logs"))
}

/// The daemon's own stderr log.
pub fn daemon_log_path() -> Option<PathBuf> {
    Some(broker_dir()?.join("agshd.log"))
}

/// Create `dir` (and parents) with 0700 permissions — broker state can carry
/// job output and environment values.
pub fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_env_override_wins() {
        // Process-global env: assert only the override path.
        std::env::set_var("AGSH_BROKER_SOCKET", "/tmp/x.sock");
        assert_eq!(socket_path(), Some(PathBuf::from("/tmp/x.sock")));
        std::env::remove_var("AGSH_BROKER_SOCKET");
    }

    #[test]
    fn broker_dir_env_override_wins() {
        std::env::set_var("AGSH_BROKER_DIR", "/tmp/bd");
        assert_eq!(broker_dir(), Some(PathBuf::from("/tmp/bd")));
        assert_eq!(logs_dir(), Some(PathBuf::from("/tmp/bd/logs")));
        std::env::remove_var("AGSH_BROKER_DIR");
    }
}
