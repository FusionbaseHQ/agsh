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
    builder.create(dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = std::fs::symlink_metadata(dir)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "broker state directory must be a real directory, not a symlink",
            ));
        }
        if metadata.uid() != rustix::process::geteuid().as_raw() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "broker state directory is owned by another user",
            ));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let mode = std::fs::symlink_metadata(dir)?.permissions().mode() & 0o777;
        if mode != 0o700 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "broker state directory is not private",
            ));
        }
    }

    Ok(())
}

/// Prepare the parent of an explicitly configured socket path. Existing
/// directories are accepted only when they are real, owned by this user, and
/// mode 0700. They are never chmodded implicitly; an unsafe override fails
/// closed instead.
pub fn ensure_socket_parent(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};

                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "broker socket parent must be a real directory",
                    ));
                }
                if metadata.uid() != rustix::process::geteuid().as_raw() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "broker socket parent is owned by another user",
                    ));
                }
                if metadata.permissions().mode() & 0o777 != 0o700 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "broker socket parent must be mode 0700",
                    ));
                }
            }
            #[cfg(not(unix))]
            if !metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "broker socket parent must be a directory",
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ensure_dir(dir),
        Err(error) => Err(error),
    }
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

    #[cfg(unix)]
    #[test]
    fn ensure_dir_tightens_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("agsh-broker-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();

        ensure_dir(&dir).unwrap();

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_dir_rejects_a_symlink_without_chmodding_its_target() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let base = std::env::temp_dir().join(format!("agsh-broker-link-{}", std::process::id()));
        let target = base.join("target");
        let link = base.join("link");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &link).unwrap();

        assert!(ensure_dir(&link).is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn socket_parent_rejects_non_private_existing_directory_without_chmodding_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("agsh-broker-socket-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            ensure_socket_parent(&dir).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );

        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn socket_parent_accepts_private_owned_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "agsh-broker-private-socket-parent-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        ensure_socket_parent(&dir).unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
