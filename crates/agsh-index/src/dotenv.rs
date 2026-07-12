//! Project `.env` detection, parsing, and a content-digest trust store.
//!
//! Auto-applying a project's `.env` is gated on trust: a `.env` is only loaded
//! if its current SHA-256 digest matches one the user explicitly trusted (via
//! the `trust` builtin). This prevents an untrusted repo from injecting
//! environment variables just because you `cd` into it. The digest detects
//! edits; it is not a signature and does not authenticate the project author.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_DOTENV_BYTES: usize = 1024 * 1024;
const TRUST_DIGEST_PREFIX: &str = "sha256:";

/// Versioned cryptographic digest persisted in the project-env trust store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrustDigest([u8; 32]);

impl TrustDigest {
    fn parse(value: &str) -> Option<Self> {
        let hex = value.strip_prefix(TRUST_DIGEST_PREFIX)?;
        let bytes = decode_hex(hex)?;
        let bytes: [u8; 32] = bytes.try_into().ok()?;
        Some(Self(bytes))
    }
}

impl fmt::Display for TrustDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(TRUST_DIGEST_PREFIX)?;
        formatter.write_str(&encode_hex(&self.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DotenvSnapshot {
    pub digest: TrustDigest,
    pub pairs: Vec<(String, String)>,
}

/// The `.env` file for a directory, if present.
pub fn find_dotenv(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(".env");
    std::fs::symlink_metadata(&path).ok().map(|_| path)
}

/// Parse a `.env` file into KEY=VALUE pairs, ignoring blanks/comments, an
/// optional `export ` prefix, and surrounding single/double quotes.
pub fn parse_dotenv(path: &Path) -> Vec<(String, String)> {
    read_dotenv(path)
        .map(|snapshot| snapshot.pairs)
        .unwrap_or_default()
}

pub fn read_dotenv(path: &Path) -> Option<DotenvSnapshot> {
    read_dotenv_checked(path).ok()
}

/// Read and parse a `.env`, retaining I/O diagnostics for the explicit trust
/// command. Automatic activation uses [`read_dotenv`] and simply fails closed.
pub fn read_dotenv_checked(path: &Path) -> io::Result<DotenvSnapshot> {
    let bytes = read_bounded_regular_file(path, MAX_DOTENV_BYTES, ".env")?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(DotenvSnapshot {
        digest: content_digest_bytes(&bytes),
        pairs: parse_dotenv_text(text),
    })
}

fn parse_dotenv_text(text: &str) -> Vec<(String, String)> {
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

/// A versioned SHA-256 content digest of `path` for trust comparison.
pub fn content_digest(path: &Path) -> Option<TrustDigest> {
    let bytes = read_bounded_regular_file(path, MAX_DOTENV_BYTES, ".env").ok()?;
    Some(content_digest_bytes(&bytes))
}

fn read_bounded_regular_file(path: &Path, limit: usize, label: &str) -> io::Result<Vec<u8>> {
    use rustix::fs::{Mode, OFlags};

    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error.raw_os_error()))?;
    let file = std::fs::File::from(descriptor);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} path is not a regular file"),
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} exceeds {limit} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} exceeds {limit} bytes"),
        ));
    }
    Ok(bytes)
}

fn content_digest_bytes(bytes: &[u8]) -> TrustDigest {
    TrustDigest(sha256(bytes))
}

/// Persistent set of trusted `.env` files, keyed by directory.
#[derive(Debug, Default)]
pub struct TrustStore {
    entries: BTreeMap<PathBuf, TrustDigest>,
    path: Option<PathBuf>,
}

impl TrustStore {
    pub fn load() -> io::Result<Self> {
        let path = trust_path();
        let mut entries = BTreeMap::new();
        if let Some(p) = &path {
            match read_bounded_text(p, MAX_TRUST_STORE_BYTES) {
                Ok(Some(text)) => entries = parse_trust_store(&text)?,
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Self { entries, path })
    }

    pub fn is_trusted(&self, dir: &Path, digest: TrustDigest) -> bool {
        self.entries.get(dir) == Some(&digest)
    }

    /// Trust `dir`'s `.env` at `digest`, persisting the change before success is
    /// reported. On failure the in-memory update is rolled back as well.
    pub fn trust(&mut self, dir: &Path, digest: TrustDigest) -> io::Result<()> {
        let dir = dir.to_path_buf();
        let prior = self.entries.insert(dir.clone(), digest);
        if let Err(error) = self.persist() {
            match prior {
                Some(prior) => {
                    self.entries.insert(dir, prior);
                }
                None => {
                    self.entries.remove(&dir);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self) -> io::Result<()> {
        let path = self.path.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no trust-store path (HOME and XDG_CONFIG_HOME are unset)",
            )
        })?;
        let text = serialize_trust_store(&self.entries)?;
        persist_trust_file(path, text.as_bytes())
    }
}

static TRUST_TEMP_ID: AtomicU64 = AtomicU64::new(0);
const MAX_TRUST_STORE_BYTES: usize = 1024 * 1024;

fn read_bounded_text(path: &Path, limit: usize) -> io::Result<Option<String>> {
    validate_existing_parent(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => validate_trust_file_metadata(&metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    match read_bounded_regular_file(path, limit, "trust store") {
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn parse_trust_store(text: &str) -> io::Result<BTreeMap<PathBuf, TrustDigest>> {
    let mut entries = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        // Pre-0.3 stores used an unversioned 64-bit DefaultHasher value. It is
        // deliberately ignored rather than trusted; the next successful
        // `agtrust` rewrites the store in the cryptographic format.
        if line
            .split_once(' ')
            .is_some_and(|(hash, _)| !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let (digest, encoded_path) = line.split_once('\t').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid trust-store record on line {}", index + 1),
            )
        })?;
        let digest = TrustDigest::parse(digest).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid trust-store digest on line {}", index + 1),
            )
        })?;
        let path = decode_path(encoded_path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid trust-store path on line {}", index + 1),
            )
        })?;
        entries.insert(path, digest);
    }
    Ok(entries)
}

fn serialize_trust_store(entries: &BTreeMap<PathBuf, TrustDigest>) -> io::Result<String> {
    let mut text = String::new();
    for (path, digest) in entries {
        let line = format!("{digest}\t{}\n", encode_path(path));
        if text.len().saturating_add(line.len()) > MAX_TRUST_STORE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("trust store exceeds {MAX_TRUST_STORE_BYTES} bytes"),
            ));
        }
        text.push_str(&line);
    }
    Ok(text)
}

#[cfg(unix)]
fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    encode_hex(path.as_os_str().as_bytes())
}

#[cfg(not(unix))]
fn encode_path(path: &Path) -> String {
    encode_hex(path.to_string_lossy().as_bytes())
}

#[cfg(unix)]
fn decode_path(value: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(decode_hex(
        value,
    )?)))
}

#[cfg(not(unix))]
fn decode_path(value: &str) -> Option<PathBuf> {
    String::from_utf8(decode_hex(value)?)
        .ok()
        .map(PathBuf::from)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let mut decoded = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let high = decode_nibble(pair[0])?;
        let low = decode_nibble(pair[1])?;
        decoded.push((high << 4) | low);
    }
    Some(decoded)
}

fn decode_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn validate_existing_parent(path: &Path) -> io::Result<()> {
    let parent = trust_parent(path)?;
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) => validate_parent_metadata(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn trust_parent(path: &Path) -> io::Result<&Path> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trust store path is empty",
        ));
    }
    match path.parent() {
        Some(parent) if parent.as_os_str().is_empty() => Ok(Path::new(".")),
        Some(parent) => Ok(parent),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trust store path has no parent directory",
        )),
    }
}

#[cfg(unix)]
fn validate_trust_file_metadata(metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trust-store path is not a regular file",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trust store is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trust store is writable by another user",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_trust_file_metadata(metadata: &std::fs::Metadata) -> io::Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trust-store path is not a regular file",
        ))
    }
}

#[cfg(unix)]
fn validate_parent_metadata(metadata: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trust-store parent must be a real directory",
        ));
    }
    if metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trust-store parent is not owned by the current user",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "trust-store parent is writable by another user",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_parent_metadata(metadata: &std::fs::Metadata) -> io::Result<()> {
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "trust-store parent must be a directory",
        ))
    }
}

// FIPS 180-4 SHA-256. Inputs are already bounded to 1 MiB; this compact,
// allocation-free compression routine is covered by the standard test vectors.
fn sha256(input: &[u8]) -> [u8; 32] {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    fn compress(state: &mut [u32; 8], block: &[u8], constants: &[u32; 64]) {
        let mut schedule = [0u32; 64];
        for (index, word) in block.chunks_exact(4).take(16).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
        for index in 0..64 {
            let upper_e = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(upper_e)
                .wrapping_add(choice)
                .wrapping_add(constants[index])
                .wrapping_add(schedule[index]);
            let upper_a = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = upper_a.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut state = INITIAL;
    let mut chunks = input.chunks_exact(64);
    for chunk in &mut chunks {
        compress(&mut state, chunk, &K);
    }

    let remainder = chunks.remainder();
    let mut tail = [0u8; 128];
    tail[..remainder.len()].copy_from_slice(remainder);
    tail[remainder.len()] = 0x80;
    let padded_len = if remainder.len() < 56 { 64 } else { 128 };
    let bit_len = (input.len() as u64).wrapping_mul(8);
    tail[padded_len - 8..padded_len].copy_from_slice(&bit_len.to_be_bytes());
    for chunk in tail[..padded_len].chunks_exact(64) {
        compress(&mut state, chunk, &K);
    }

    let mut digest = [0u8; 32];
    for (word, output) in state.iter().zip(digest.chunks_exact_mut(4)) {
        output.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

fn persist_trust_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = trust_parent(path)?;
    if bytes.len() > MAX_TRUST_STORE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("trust store exceeds {MAX_TRUST_STORE_BYTES} bytes"),
        ));
    }
    if !parent.exists() {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.recursive(true).create(parent)?;
    }
    validate_existing_parent(path)?;

    for _ in 0..128 {
        let id = TRUST_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".agsh-trust-{}-{id}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp) {
            Ok(mut file) => {
                let result = (|| {
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    std::fs::rename(&temp, path)?;
                    if let Ok(parent_file) = std::fs::File::open(parent) {
                        let _ = parent_file.sync_all();
                    }
                    Ok(())
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique trust-store temporary file",
    ))
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
    fn digest_changes_with_content() {
        let dir = std::env::temp_dir().join(format!("agsh_dotenv_h_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        std::fs::write(&path, "A=1").unwrap();
        let h1 = content_digest(&path).unwrap();
        std::fs::write(&path, "A=2").unwrap();
        let h2 = content_digest(&path).unwrap();
        assert_ne!(h1, h2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_digest_matches_sha256_test_vectors() {
        assert_eq!(
            content_digest_bytes(b"").to_string(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            content_digest_bytes(b"abc").to_string(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            content_digest_bytes(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")
                .to_string(),
            "sha256:248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            content_digest_bytes(&vec![b'a'; 1_000_000]).to_string(),
            "sha256:cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn snapshot_digest_and_pairs_come_from_one_read() {
        let dir = std::env::temp_dir().join(format!("agsh_dotenv_snapshot_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".env");
        let bytes = b"FIRST=one\nSECOND='two words'\n";
        std::fs::write(&path, bytes).unwrap();

        let snapshot = read_dotenv(&path).unwrap();

        assert_eq!(snapshot.digest, content_digest_bytes(bytes));
        assert_eq!(
            snapshot.pairs,
            [
                ("FIRST".to_string(), "one".to_string()),
                ("SECOND".to_string(), "two words".to_string()),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_dotenv_is_rejected() {
        let path = std::env::temp_dir().join(format!(
            "agsh-dotenv-oversized-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, vec![b'x'; MAX_DOTENV_BYTES + 1]).unwrap();
        assert!(read_dotenv(&path).is_none());
        assert!(content_digest(&path).is_none());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn dotenv_reader_rejects_symlinks_and_non_regular_files() {
        use std::os::unix::fs::symlink;

        assert!(read_dotenv(Path::new("/dev/zero")).is_none());
        let dir = std::env::temp_dir().join(format!("agsh_dotenv_link_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let link = dir.join(".env");
        std::fs::write(&target, b"FOO=bar\n").unwrap();
        symlink(&target, &link).unwrap();
        assert!(find_dotenv(&dir).is_some());
        assert!(read_dotenv_checked(&link).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn trust_store_replace_is_private_and_does_not_follow_a_final_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = std::env::temp_dir().join(format!("agsh_trust_atomic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        let trust = dir.join("trusted_env");
        std::fs::write(&victim, b"do not overwrite").unwrap();
        symlink(&victim, &trust).unwrap();
        let payload = serialize_trust_store(&BTreeMap::from([(
            PathBuf::from("/workspace"),
            content_digest_bytes(b"FOO=bar\n"),
        )]))
        .unwrap();

        persist_trust_file(&trust, payload.as_bytes()).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not overwrite");
        assert!(!std::fs::symlink_metadata(&trust)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&trust).unwrap(), payload);
        let metadata = std::fs::metadata(&trust).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_trust_store_input_is_rejected_without_unbounded_reading() {
        let dir = std::env::temp_dir().join(format!("agsh_trust_bound_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trusted_env");
        std::fs::write(&path, vec![b'x'; MAX_TRUST_STORE_BYTES + 1]).unwrap();

        assert!(read_bounded_text(&path, MAX_TRUST_STORE_BYTES).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn trust_store_reader_rejects_symlinks_and_special_files() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = std::env::temp_dir().join(format!("agsh_trust_input_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let link = dir.join("trusted_env");
        std::fs::write(&target, b"").unwrap();
        symlink(&target, &link).unwrap();

        assert!(read_bounded_text(&link, MAX_TRUST_STORE_BYTES).is_err());
        assert!(read_bounded_text(Path::new("/dev/zero"), MAX_TRUST_STORE_BYTES).is_err());

        let writable = dir.join("world-writable");
        std::fs::write(&writable, b"").unwrap();
        std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(read_bounded_text(&writable, MAX_TRUST_STORE_BYTES).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_persistence_failure_is_returned_and_rolled_back() {
        let dir = std::env::temp_dir().join(format!("agsh_trust_failure_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let non_directory = dir.join("not-a-directory");
        std::fs::write(&non_directory, b"blocker").unwrap();
        let project = Path::new("/workspace/project");
        let digest = content_digest_bytes(b"FOO=bar\n");
        let mut store = TrustStore {
            entries: BTreeMap::new(),
            path: Some(non_directory.join("trusted_env")),
        };

        assert!(store.trust(project, digest).is_err());
        assert!(!store.is_trusted(project, digest));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_store_format_round_trips_delimiter_safe_path_encoding() {
        let path = PathBuf::from("/workspace/a path\nwith-newline");
        let digest = content_digest_bytes(b"FOO=bar\n");
        let entries = BTreeMap::from([(path.clone(), digest)]);

        let serialized = serialize_trust_store(&entries).unwrap();
        assert!(serialized.starts_with("sha256:"));
        assert!(!serialized.contains("a path"));
        assert_eq!(parse_trust_store(&serialized).unwrap(), entries);

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;

            let raw_path = PathBuf::from(std::ffi::OsString::from_vec(vec![
                b'/', b't', b'm', b'p', b'/', 0xff,
            ]));
            let raw_entries = BTreeMap::from([(raw_path, digest)]);
            let raw_serialized = serialize_trust_store(&raw_entries).unwrap();
            assert_eq!(parse_trust_store(&raw_serialized).unwrap(), raw_entries);
        }

        let legacy = "123456789 /workspace/legacy\n";
        assert!(parse_trust_store(legacy).unwrap().is_empty());
        assert_eq!(
            trust_parent(Path::new("trusted_env")).unwrap(),
            Path::new(".")
        );
        assert!(trust_parent(Path::new("")).is_err());
    }
}
