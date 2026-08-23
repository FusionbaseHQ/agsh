use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// Keep protocol v1 backward-compatible: installers publish the helper before
// the main binary so an interrupted future upgrade can briefly pair versions.
pub const INTERNAL_EXEC_HELPER_FLAG: &str = "--internal-exec-helper-v1";

/// Replace this process with `argv[0]` without libc's implicit ENOEXEC shell
/// fallback. If the kernel rejects an executable text file, perform one
/// explicit, bounded `/bin/sh` fallback. Native-image magic, malformed
/// shebangs, unreadable files, and binary first lines fail closed.
pub fn execve_with_text_fallback(argv: &[OsString]) -> io::Error {
    let environment = match encoded_environment() {
        Ok(environment) => environment,
        Err(error) => return error,
    };
    // A preload interposer may already be loaded in this helper (notably on
    // Linux and in unsigned development builds). Make its hook pass through our
    // raw exec calls, but give the target the exact environment snapshot from
    // before this private marker was set. Hardened macOS launches restore
    // transported DYLD bindings in that snapshot. The target's own descendants
    // therefore remain eligible for interception.
    std::env::set_var("AGSH_INTERCEPT_ACTIVE", "1");

    let error = execve_once(argv, &environment);
    if error.raw_os_error() != Some(nix::errno::Errno::ENOEXEC as i32) {
        return error;
    }

    let Some(program) = argv.first() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "empty command");
    };
    if !executable_text_file(Path::new(program)) {
        return io::Error::other("cannot execute binary file");
    }

    let mut shell_argv = Vec::with_capacity(argv.len() + 1);
    shell_argv.push(OsString::from("/bin/sh"));
    shell_argv.push(program.clone());
    shell_argv.extend(argv.iter().skip(1).cloned());
    execve_once(&shell_argv, &environment)
}

fn encoded_environment() -> io::Result<Vec<CString>> {
    let mut environment = BTreeMap::<Vec<u8>, Vec<u8>>::new();
    #[cfg(target_os = "macos")]
    let mut transported = Vec::new();

    for (name, value) in std::env::vars_os() {
        let name = name.as_os_str().as_bytes();
        #[cfg(target_os = "macos")]
        if let Some(encoded_name) =
            name.strip_prefix(agsh_broker::MACOS_EXEC_ENV_TRANSPORT_PREFIX.as_bytes())
        {
            let decoded_name = decode_transport_name(encoded_name)?;
            transported.push((decoded_name, value.as_os_str().as_bytes().to_vec()));
            continue;
        }
        environment.insert(name.to_vec(), value.as_os_str().as_bytes().to_vec());
    }

    // Transported values win deterministically if a helper is invoked with a
    // duplicate real binding. Normal agsh launches remove the real binding.
    #[cfg(target_os = "macos")]
    for (name, value) in transported {
        environment.insert(name, value);
    }

    environment
        .into_iter()
        .map(|(name, value)| {
            let mut binding = name;
            binding.push(b'=');
            binding.extend_from_slice(&value);
            CString::new(binding).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "environment binding contains an interior NUL byte",
                )
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn decode_transport_name(encoded: &[u8]) -> io::Result<Vec<u8>> {
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }

    if encoded.is_empty() || !encoded.len().is_multiple_of(2) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid macOS exec-helper environment transport",
        ));
    }
    let mut decoded = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.chunks_exact(2) {
        let Some(high) = nibble(pair[0]) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid macOS exec-helper environment transport",
            ));
        };
        let Some(low) = nibble(pair[1]) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid macOS exec-helper environment transport",
            ));
        };
        decoded.push((high << 4) | low);
    }
    if !decoded.starts_with(b"DYLD_") || decoded.contains(&b'=') || decoded.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid macOS exec-helper environment transport",
        ));
    }
    Ok(decoded)
}

fn execve_once(argv: &[OsString], environment: &[CString]) -> io::Error {
    let Some(program) = argv.first() else {
        return io::Error::new(io::ErrorKind::InvalidInput, "empty command");
    };
    let cstring = |bytes: &[u8], label: &str| {
        CString::new(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} contains an interior NUL byte"),
            )
        })
    };
    let program = match cstring(program.as_os_str().as_bytes(), "command path") {
        Ok(program) => program,
        Err(error) => return error,
    };
    let args = match argv
        .iter()
        .map(|arg| cstring(arg.as_os_str().as_bytes(), "command argument"))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(args) => args,
        Err(error) => return error,
    };
    match nix::unistd::execve(&program, &args, environment) {
        Ok(never) => match never {},
        Err(error) => io::Error::from_raw_os_error(error as i32),
    }
}

/// Match the conservative executable-text rule used by the shell executor.
/// The kernel has already rejected the image before this probe runs, so custom
/// binfmt handlers retain precedence. The non-blocking, regular-file check
/// prevents a path observed as a FIFO/device during the probe from becoming
/// blocking shell input. `/bin/sh` later reopens the pathname, so a replacement
/// race remains a documented pre-1.0 limitation.
fn executable_text_file(path: &Path) -> bool {
    use rustix::fs::{Mode, OFlags};

    const PREFIX_BYTES: usize = 4096;
    let descriptor = match rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(descriptor) => descriptor,
        Err(_) => return false,
    };
    let mut file = File::from(descriptor);
    if !file.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }

    let mut prefix = [0_u8; PREFIX_BYTES];
    let mut length = 0;
    while length < prefix.len() {
        match file.read(&mut prefix[length..]) {
            Ok(0) => break,
            Ok(read) => length += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    let prefix = &prefix[..length];
    if prefix.starts_with(b"#!") || has_native_executable_magic(prefix) {
        return false;
    }
    let newline = prefix.iter().position(|byte| *byte == b'\n');
    // The text decision is deliberately bounded. If a full prefix still does
    // not contain the end of the first line, its binary/text status is
    // inconclusive and must fail closed rather than letting `/bin/sh` inspect
    // bytes outside the validated window.
    if newline.is_none() && length == PREFIX_BYTES {
        return false;
    }
    let first_line_end = newline.unwrap_or(prefix.len());
    !prefix[..first_line_end].contains(&0)
}

fn has_native_executable_magic(prefix: &[u8]) -> bool {
    matches!(
        prefix.get(..4),
        Some(
            b"\x7fELF"
                | b"\xfe\xed\xfa\xce"
                | b"\xce\xfa\xed\xfe"
                | b"\xfe\xed\xfa\xcf"
                | b"\xcf\xfa\xed\xfe"
                | b"\xca\xfe\xba\xbe"
                | b"\xbe\xba\xfe\xca"
                | b"\xca\xfe\xba\xbf"
                | b"\xbf\xba\xfe\xca"
        )
    )
}
