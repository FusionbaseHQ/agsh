//! Project `.env` detection, parsing, and a content-hash trust store.
//!
//! Auto-applying a project's `.env` is gated on trust: a `.env` is only loaded
//! if its current content hash matches one the user explicitly trusted (via the
//! `trust` builtin). This prevents an untrusted repo from injecting environment
//! variables just because you `cd` into it. The hash detects edits (re-trust
//! required); it is a change detector, not a cryptographic signature.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The `.env` file for a directory, if present.
pub fn find_dotenv(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(".env");
    path.is_file().then_some(path)
}

/// Parse a `.env` file into KEY=VALUE pairs, ignoring blanks/comments, an
/// optional `export ` prefix, and surrounding single/double quotes.
pub fn parse_dotenv(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || !key.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        out.push((key.to_string(), value.to_string()));
    }
    out
}

/// A content hash of `path` for trust comparison (change detector, not crypto).
pub fn content_hash(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

/// Persistent set of trusted `.env` files, keyed by directory.
#[derive(Debug, Default)]
pub struct TrustStore {
    entries: BTreeMap<String, u64>,
    path: Option<PathBuf>,
}

impl TrustStore {
    pub fn load() -> Self {
        let path = trust_path();
        let mut entries = BTreeMap::new();
        if let Some(p) = &path {
            if let Ok(text) = std::fs::read_to_string(p) {
                for line in text.lines() {
                    if let Some((hash, dir)) = line.split_once(' ') {
                        if let Ok(h) = hash.parse::<u64>() {
                            entries.insert(dir.to_string(), h);
                        }
                    }
                }
            }
        }
        Self { entries, path }
    }

    pub fn is_trusted(&self, dir: &str, hash: u64) -> bool {
        self.entries.get(dir) == Some(&hash)
    }

    /// Trust `dir`'s `.env` at `hash`, persisting the change.
    pub fn trust(&mut self, dir: &str, hash: u64) {
        self.entries.insert(dir.to_string(), hash);
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut text = String::new();
        for (dir, hash) in &self.entries {
            text.push_str(&format!("{hash} {dir}\n"));
        }
        if let Ok(mut f) = std::fs::File::create(path) {
            let _ = f.write_all(text.as_bytes());
        }
    }
}

fn trust_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("AGSH_TRUST_FILE") {
        return Some(PathBuf::from(p));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(Path::new(&xdg).join("agsh/trusted_env"));
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".config/agsh/trusted_env"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dotenv_lines() {
        let dir = std::env::temp_dir().join(format!("agsh_dotenv_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(
            &path,
            "# comment\nexport FOO=bar\nBAZ=\"q u x\"\nEMPTY=\nbad line\n",
        )
        .unwrap();
        let pairs = parse_dotenv(&path);
        assert_eq!(pairs[0], ("FOO".to_string(), "bar".to_string()));
        assert_eq!(pairs[1], ("BAZ".to_string(), "q u x".to_string()));
        assert!(pairs.iter().any(|(k, v)| k == "EMPTY" && v.is_empty()));
        assert!(!pairs.iter().any(|(k, _)| k == "bad line"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hash_changes_with_content() {
        let dir = std::env::temp_dir().join(format!("agsh_dotenv_h_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "A=1").unwrap();
        let h1 = content_hash(&path).unwrap();
        std::fs::write(&path, "A=2").unwrap();
        let h2 = content_hash(&path).unwrap();
        assert_ne!(h1, h2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
