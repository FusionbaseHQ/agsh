#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// Keep protocol v1 backward-compatible: installers publish the helper before
// the main binary so an interrupted future upgrade can briefly pair versions.
pub const INTERNAL_EXEC_HELPER_FLAG: &str = "--internal-exec-helper-v1";
// Inspect only the preload binding understood by this platform so an inert
// caller variable is never rewritten.
#[cfg(target_os = "macos")]
const PRELOAD_ENVIRONMENT: &[u8] = b"DYLD_INSERT_LIBRARIES=";
#[cfg(not(target_os = "macos"))]
const PRELOAD_ENVIRONMENT: &[u8] = b"LD_PRELOAD=";

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
    // raw exec calls, but give the direct target the exact environment snapshot
    // from before this process-local boundary was set. Hardened macOS launches
    // restore transported DYLD bindings in that snapshot. The bounded ENOEXEC
    // text fallback below extends the boundary only when raw bytes require it.
    std::env::set_var("AGSH_INTERCEPT_ACTIVE", "1");

    // The interposer has this helper's architecture, while macOS can hand off to
    // arm64e or cross-architecture targets. Newer dyld builds terminate rather
    // than ignore an incompatible preload. Keep our managed entry only when a
    // bounded Mach-O header advertises a compatible native slice; unknown
    // targets run normally and retain PATH-shim interception.
    let target_environment = direct_target_environment(argv, &environment);
    let error = execve_once(argv, target_environment.as_deref().unwrap_or(&environment));
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
    let fallback_environment = text_fallback_environment(&environment);
    execve_once(&shell_argv, &fallback_environment)
}

/// Keep the explicit `/bin/sh` fallback and its descendants inside the raw
/// observation boundary. In deep-interception mode, macOS may re-exec its shell
/// bootstrap before reading the text file; allowing that transition to be
/// observed would compact bytes intended for a pipe or redirect.
fn text_fallback_environment(environment: &[CString]) -> Vec<CString> {
    let active_prefix = b"AGSH_INTERCEPT_ACTIVE=";
    let Some(managed_interposer) = managed_interposer_path(environment) else {
        return environment.to_vec();
    };

    // The fallback is already inside a raw boundary, so loading our interposer
    // serves no purpose. More importantly, Apple system shells can use an
    // arm64e slice that cannot load an ordinary arm64 development dylib. Keep
    // unrelated caller preloads while removing only agsh's own entry.
    let mut fallback = environment
        .iter()
        .filter_map(|binding| environment_without_managed_interposer(binding, &managed_interposer))
        .collect::<Vec<_>>();

    if !environment
        .iter()
        .any(|binding| binding.as_bytes().starts_with(active_prefix))
    {
        fallback.push(CString::new("AGSH_INTERCEPT_ACTIVE=1").unwrap());
    }
    fallback
}

#[cfg(test)]
fn deep_intercept_environment(environment: &[CString]) -> bool {
    managed_interposer_path(environment).is_some()
}

fn managed_interposer_path(environment: &[CString]) -> Option<Vec<u8>> {
    let executable = std::env::current_exe().ok()?;
    managed_interposer_path_for_executable(environment, &executable)
}

fn managed_interposer_path_for_executable(
    environment: &[CString],
    executable: &Path,
) -> Option<Vec<u8>> {
    let executable_directory = executable.parent()?;
    let library_name = if cfg!(target_os = "macos") {
        "libagsh_intercept.dylib"
    } else {
        "libagsh_intercept.so"
    };
    let candidates = [
        executable_directory.join(library_name),
        executable_directory
            .join("..")
            .join("lib")
            .join(library_name),
    ];
    let prefix = PRELOAD_ENVIRONMENT;
    let preload = environment.iter().find_map(|binding| {
        binding
            .as_bytes()
            .strip_prefix(prefix)
            .filter(|value| !value.is_empty())
    })?;

    candidates.into_iter().find_map(|candidate| {
        let candidate = candidate.as_os_str().as_bytes();
        (preload == candidate || preload.strip_prefix(candidate)?.starts_with(b":"))
            .then(|| candidate.to_vec())
    })
}

fn environment_without_managed_interposer(
    binding: &CString,
    managed_interposer: &[u8],
) -> Option<CString> {
    let prefix = PRELOAD_ENVIRONMENT;
    let Some(value) = binding.as_bytes().strip_prefix(prefix) else {
        return Some(binding.clone());
    };
    let Some(suffix) = value.strip_prefix(managed_interposer) else {
        return Some(binding.clone());
    };
    if suffix.is_empty() {
        return None;
    }
    let Some(retained) = suffix.strip_prefix(b":") else {
        return Some(binding.clone());
    };
    if retained.is_empty() {
        return None;
    }

    // install_deep_intercept prepends this exact path and one colon. Remove
    // exactly those bytes and preserve the caller's prior binding verbatim.
    let mut sanitized = prefix.to_vec();
    sanitized.extend_from_slice(retained);
    Some(CString::new(sanitized).expect("existing environment cannot contain an interior NUL"))
}

#[cfg(target_os = "macos")]
fn direct_target_environment(argv: &[OsString], environment: &[CString]) -> Option<Vec<CString>> {
    let program = argv.first()?;
    let managed_interposer = managed_interposer_path(environment)?;
    if mach_o_accepts_native_interposer(Path::new(program)) {
        return None;
    }
    Some(
        environment
            .iter()
            .filter_map(|binding| {
                environment_without_managed_interposer(binding, &managed_interposer)
            })
            .collect(),
    )
}

#[cfg(not(target_os = "macos"))]
fn direct_target_environment(_argv: &[OsString], _environment: &[CString]) -> Option<Vec<CString>> {
    None
}

#[cfg(target_os = "macos")]
fn mach_o_accepts_native_interposer(path: &Path) -> bool {
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
    mach_o_prefix_accepts_interposer(&prefix[..length], native_interposer_architecture())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn native_interposer_architecture() -> InterposerArchitecture {
    InterposerArchitecture::PlainArm64
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn native_interposer_architecture() -> InterposerArchitecture {
    InterposerArchitecture::X86_64
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Clone, Copy)]
enum InterposerArchitecture {
    #[cfg(any(test, target_arch = "aarch64"))]
    PlainArm64,
    #[cfg(any(test, target_arch = "x86_64"))]
    X86_64,
}

#[cfg(any(test, target_os = "macos"))]
#[derive(Clone, Copy)]
enum MachOByteOrder {
    Big,
    Little,
}

#[cfg(any(test, target_os = "macos"))]
fn mach_o_prefix_accepts_interposer(prefix: &[u8], architecture: InterposerArchitecture) -> bool {
    const FAT_ARCH_BYTES: usize = 20;
    const FAT_ARCH_64_BYTES: usize = 32;

    let Some(magic) = prefix.get(..4) else {
        return false;
    };
    match magic {
        b"\xcf\xfa\xed\xfe" => {
            mach_o_header_accepts_interposer(prefix, MachOByteOrder::Little, architecture)
        }
        b"\xfe\xed\xfa\xcf" => {
            mach_o_header_accepts_interposer(prefix, MachOByteOrder::Big, architecture)
        }
        b"\xca\xfe\xba\xbe" => {
            fat_mach_o_accepts_interposer(prefix, MachOByteOrder::Big, FAT_ARCH_BYTES, architecture)
        }
        b"\xbe\xba\xfe\xca" => fat_mach_o_accepts_interposer(
            prefix,
            MachOByteOrder::Little,
            FAT_ARCH_BYTES,
            architecture,
        ),
        b"\xca\xfe\xba\xbf" => fat_mach_o_accepts_interposer(
            prefix,
            MachOByteOrder::Big,
            FAT_ARCH_64_BYTES,
            architecture,
        ),
        b"\xbf\xba\xfe\xca" => fat_mach_o_accepts_interposer(
            prefix,
            MachOByteOrder::Little,
            FAT_ARCH_64_BYTES,
            architecture,
        ),
        _ => false,
    }
}

#[cfg(any(test, target_os = "macos"))]
fn mach_o_header_accepts_interposer(
    prefix: &[u8],
    order: MachOByteOrder,
    architecture: InterposerArchitecture,
) -> bool {
    let Some(cpu_type) = mach_o_u32(prefix, 4, order) else {
        return false;
    };
    let Some(cpu_subtype) = mach_o_u32(prefix, 8, order) else {
        return false;
    };
    mach_o_slice_compatibility(cpu_type, cpu_subtype, architecture) == Some(true)
}

#[cfg(any(test, target_os = "macos"))]
fn fat_mach_o_accepts_interposer(
    prefix: &[u8],
    order: MachOByteOrder,
    entry_bytes: usize,
    architecture: InterposerArchitecture,
) -> bool {
    const MAX_FAT_ARCHITECTURES: u32 = 64;

    let Some(count) = mach_o_u32(prefix, 4, order) else {
        return false;
    };
    if count == 0 || count > MAX_FAT_ARCHITECTURES {
        return false;
    }
    let required_bytes = 8 + count as usize * entry_bytes;
    if prefix.len() < required_bytes {
        return false;
    }
    let mut has_compatible_slice = false;
    for index in 0..count as usize {
        let offset = 8 + index * entry_bytes;
        let Some(cpu_type) = mach_o_u32(prefix, offset, order) else {
            return false;
        };
        let Some(cpu_subtype) = mach_o_u32(prefix, offset + 4, order) else {
            return false;
        };
        match mach_o_slice_compatibility(cpu_type, cpu_subtype, architecture) {
            Some(true) => has_compatible_slice = true,
            Some(false) => return false,
            None => {}
        }
    }
    has_compatible_slice
}

#[cfg(any(test, target_os = "macos"))]
fn mach_o_slice_compatibility(
    cpu_type: u32,
    cpu_subtype: u32,
    architecture: InterposerArchitecture,
) -> Option<bool> {
    #[cfg(any(test, target_arch = "aarch64"))]
    const CPU_TYPE_ARM64: u32 = 0x0100_000c;
    #[cfg(any(test, target_arch = "x86_64"))]
    const CPU_TYPE_X86_64: u32 = 0x0100_0007;
    const CPU_SUBTYPE_MASK: u32 = 0xff00_0000;

    let subtype = cpu_subtype & !CPU_SUBTYPE_MASK;
    match architecture {
        #[cfg(any(test, target_arch = "aarch64"))]
        InterposerArchitecture::PlainArm64 if cpu_type == CPU_TYPE_ARM64 => {
            Some(matches!(subtype, 0 | 1))
        }
        #[cfg(any(test, target_arch = "x86_64"))]
        InterposerArchitecture::X86_64 if cpu_type == CPU_TYPE_X86_64 => {
            Some(matches!(subtype, 3 | 8))
        }
        _ => None,
    }
}

#[cfg(any(test, target_os = "macos"))]
fn mach_o_u32(prefix: &[u8], offset: usize, order: MachOByteOrder) -> Option<u32> {
    let bytes: [u8; 4] = prefix
        .get(offset..offset.checked_add(4)?)?
        .try_into()
        .ok()?;
    Some(match order {
        MachOByteOrder::Big => u32::from_be_bytes(bytes),
        MachOByteOrder::Little => u32::from_le_bytes(bytes),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_managed_interposer() -> Vec<u8> {
        let executable = std::env::current_exe().expect("test executable path");
        let name = if cfg!(target_os = "macos") {
            "libagsh_intercept.dylib"
        } else {
            "libagsh_intercept.so"
        };
        executable
            .parent()
            .expect("test executable directory")
            .join(name)
            .as_os_str()
            .as_bytes()
            .to_vec()
    }

    fn test_managed_preload(suffix: &[u8]) -> CString {
        let mut binding = PRELOAD_ENVIRONMENT.to_vec();
        binding.extend_from_slice(&test_managed_interposer());
        binding.extend_from_slice(suffix);
        CString::new(binding).unwrap()
    }

    #[test]
    fn text_fallback_preserves_an_existing_observation_boundary() {
        let environment = [
            CString::new("PATH=/bin").unwrap(),
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            CString::new("AGSH_INTERCEPT_ACTIVE=1").unwrap(),
            test_managed_preload(b""),
        ];
        let fallback = text_fallback_environment(&environment);
        let bindings = fallback
            .iter()
            .map(|binding| binding.to_string_lossy())
            .collect::<Vec<_>>();
        assert!(bindings
            .iter()
            .any(|binding| binding == "AGSH_INTERCEPT_ACTIVE=1"));
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.starts_with("AGSH_INTERCEPT_ACTIVE="))
                .count(),
            1
        );
        assert!(!deep_intercept_environment(&fallback));
    }

    #[test]
    fn text_fallback_keeps_an_active_deep_subtree_raw() {
        let environment = [
            CString::new("PATH=/bin").unwrap(),
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(b""),
        ];
        let fallback = text_fallback_environment(&environment);
        assert!(fallback
            .iter()
            .any(|binding| binding.to_bytes() == b"AGSH_INTERCEPT_ACTIVE=1"));
        assert!(!deep_intercept_environment(&fallback));
    }

    #[test]
    fn text_fallback_does_not_mark_an_ordinary_target_as_active() {
        let environment = [CString::new("PATH=/bin").unwrap()];
        let fallback = text_fallback_environment(&environment);
        assert!(!fallback
            .iter()
            .any(|binding| binding.as_bytes().starts_with(b"AGSH_INTERCEPT_ACTIVE=")));
    }

    #[test]
    fn text_fallback_does_not_match_a_similarly_named_preload_library() {
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(b"_backup"),
        ];
        let fallback = text_fallback_environment(&environment);
        assert!(!fallback
            .iter()
            .any(|binding| binding.as_bytes().starts_with(b"AGSH_INTERCEPT_ACTIVE=")));
    }

    #[test]
    fn text_fallback_ignores_the_other_platforms_preload_binding() {
        let inert_binding = if cfg!(target_os = "macos") {
            "LD_PRELOAD=/tmp/libagsh_intercept.so"
        } else {
            "DYLD_INSERT_LIBRARIES=/tmp/libagsh_intercept.dylib"
        };
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            CString::new(inert_binding).unwrap(),
        ];

        let fallback = text_fallback_environment(&environment);
        assert_eq!(fallback.as_slice(), environment.as_slice());
        assert!(!fallback
            .iter()
            .any(|binding| binding.as_bytes().starts_with(b"AGSH_INTERCEPT_ACTIVE=")));
    }

    #[test]
    fn text_fallback_preserves_unrelated_preload_entries() {
        let (suffix, expected) = if cfg!(target_os = "macos") {
            (
                b":/opt/first.dylib:/opt/second.dylib".as_slice(),
                b"DYLD_INSERT_LIBRARIES=/opt/first.dylib:/opt/second.dylib".as_slice(),
            )
        } else {
            (
                b":/opt/first.so /opt/second.so".as_slice(),
                b"LD_PRELOAD=/opt/first.so /opt/second.so".as_slice(),
            )
        };
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(suffix),
        ];
        let fallback = text_fallback_environment(&environment);
        assert!(fallback
            .iter()
            .any(|binding| binding.as_bytes() == expected));
        assert!(fallback
            .iter()
            .any(|binding| binding.as_bytes() == b"AGSH_INTERCEPT_ACTIVE=1"));
    }

    #[test]
    fn text_fallback_preserves_a_later_same_named_caller_preload() {
        let (suffix, expected) = if cfg!(target_os = "macos") {
            (
                b":/caller/libagsh_intercept.dylib:/opt/other.dylib".as_slice(),
                b"DYLD_INSERT_LIBRARIES=/caller/libagsh_intercept.dylib:/opt/other.dylib"
                    .as_slice(),
            )
        } else {
            (
                b":/caller/libagsh_intercept.so:/opt/other.so".as_slice(),
                b"LD_PRELOAD=/caller/libagsh_intercept.so:/opt/other.so".as_slice(),
            )
        };
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(suffix),
        ];

        let fallback = text_fallback_environment(&environment);
        assert!(fallback
            .iter()
            .any(|binding| binding.as_bytes() == expected));
    }

    #[test]
    fn text_fallback_does_not_claim_a_callers_same_named_library() {
        let caller_binding = if cfg!(target_os = "macos") {
            "DYLD_INSERT_LIBRARIES=/caller/libagsh_intercept.dylib"
        } else {
            "LD_PRELOAD=/caller/libagsh_intercept.so"
        };
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            CString::new(caller_binding).unwrap(),
        ];

        let fallback = text_fallback_environment(&environment);
        assert_eq!(fallback.as_slice(), environment.as_slice());
        assert!(!deep_intercept_environment(&environment));
    }

    #[test]
    fn managed_preload_ownership_does_not_depend_on_mutable_agsh_self() {
        for self_binding in [None, Some("AGSH_SELF=/caller/override")] {
            let mut environment = vec![test_managed_preload(b"")];
            if let Some(binding) = self_binding {
                environment.push(CString::new(binding).unwrap());
            }

            let fallback = text_fallback_environment(&environment);
            assert!(fallback
                .iter()
                .all(|binding| !binding.as_bytes().starts_with(PRELOAD_ENVIRONMENT)));
            assert!(fallback
                .iter()
                .any(|binding| binding.as_bytes() == b"AGSH_INTERCEPT_ACTIVE=1"));
        }
    }

    #[test]
    fn managed_preload_ownership_accepts_the_installed_lib_layout() {
        let executable = Path::new("/opt/agsh/bin/agsh-exec-helper");
        let library = if cfg!(target_os = "macos") {
            "/opt/agsh/bin/../lib/libagsh_intercept.dylib"
        } else {
            "/opt/agsh/bin/../lib/libagsh_intercept.so"
        };
        let mut preload = PRELOAD_ENVIRONMENT.to_vec();
        preload.extend_from_slice(library.as_bytes());
        let environment = [CString::new(preload).unwrap()];

        assert_eq!(
            managed_interposer_path_for_executable(&environment, executable).as_deref(),
            Some(library.as_bytes())
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn text_fallback_preserves_spaces_in_unrelated_dyld_paths() {
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(b":/opt/My Library/other.dylib"),
        ];
        let fallback = text_fallback_environment(&environment);
        assert!(fallback.iter().any(|binding| {
            binding.as_bytes() == b"DYLD_INSERT_LIBRARIES=/opt/My Library/other.dylib"
        }));
    }

    #[test]
    fn macho_parser_accepts_only_proven_plain_arm64_images() {
        let accepts = |prefix: &[u8]| {
            mach_o_prefix_accepts_interposer(prefix, InterposerArchitecture::PlainArm64)
        };
        let thin_arm64_all = [
            0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        let thin_arm64_v8 = [
            0xfe, 0xed, 0xfa, 0xcf, 0x01, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x01,
        ];
        let fat_x86_and_arm64 = [
            0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x02, 0x01, 0x00, 0x00, 0x07, 0x00, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
            0x01, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
        ];
        let swapped_fat_arm64_v8 = [
            0xbe, 0xba, 0xfe, 0xca, 0x01, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00,
        ];
        let fat_64_arm64 = [
            0xca, 0xfe, 0xba, 0xbf, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x0c, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x00,
        ];
        let swapped_fat_64_arm64 = [
            0xbf, 0xba, 0xfe, 0xca, 0x01, 0x00, 0x00, 0x00, 0x0c, 0x00, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x0e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];

        assert!(accepts(&thin_arm64_all));
        assert!(accepts(&thin_arm64_v8));
        assert!(accepts(&fat_x86_and_arm64));
        assert!(accepts(&swapped_fat_arm64_v8));
        assert!(accepts(&fat_64_arm64));
        assert!(accepts(&swapped_fat_64_arm64));
    }

    #[test]
    fn macho_parser_rejects_arm64e_mixed_unknown_and_malformed_images() {
        let accepts = |prefix: &[u8]| {
            mach_o_prefix_accepts_interposer(prefix, InterposerArchitecture::PlainArm64)
        };
        let thin_arm64e_little_endian = [
            0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x80,
        ];
        let thin_arm64e_big_endian = [
            0xfe, 0xed, 0xfa, 0xcf, 0x01, 0x00, 0x00, 0x0c, 0x80, 0x00, 0x00, 0x02,
        ];
        let fat_x86_and_arm64e = [
            0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x02, 0x01, 0x00, 0x00, 0x07, 0x00, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
            0x01, 0x00, 0x00, 0x0c, 0x80, 0x00, 0x00, 0x02, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
        ];
        let fat_plain_and_arm64e = [
            0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x02, 0x01, 0x00, 0x00, 0x0c, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
            0x01, 0x00, 0x00, 0x0c, 0x80, 0x00, 0x00, 0x02, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
        ];
        let x86_only = [
            0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00,
        ];
        let future_arm64_subtype = [
            0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00,
        ];
        let truncated_fat = [0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x01];
        let absurd_fat = [0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x41];

        for incompatible in [
            thin_arm64e_little_endian.as_slice(),
            thin_arm64e_big_endian.as_slice(),
            fat_x86_and_arm64e.as_slice(),
            fat_plain_and_arm64e.as_slice(),
            x86_only.as_slice(),
            future_arm64_subtype.as_slice(),
            truncated_fat.as_slice(),
            absurd_fat.as_slice(),
            b"#!/bin/sh\n".as_slice(),
        ] {
            assert!(!accepts(incompatible));
        }
    }

    #[test]
    fn macho_parser_models_x86_64_interposer_compatibility() {
        let accepts = |prefix: &[u8]| {
            mach_o_prefix_accepts_interposer(prefix, InterposerArchitecture::X86_64)
        };
        let thin_x86_64_all = [
            0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00,
        ];
        let thin_x86_64h = [
            0xfe, 0xed, 0xfa, 0xcf, 0x01, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x08,
        ];
        let fat_x86_and_arm64e = [
            0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x02, 0x01, 0x00, 0x00, 0x07, 0x00, 0x00,
            0x00, 0x03, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
            0x01, 0x00, 0x00, 0x0c, 0x80, 0x00, 0x00, 0x02, 0x00, 0x00, 0x80, 0x00, 0x00, 0x00,
            0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
        ];
        let thin_arm64 = [
            0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        let future_x86_subtype = [
            0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01, 0x09, 0x00, 0x00, 0x00,
        ];

        assert!(accepts(&thin_x86_64_all));
        assert!(accepts(&thin_x86_64h));
        assert!(accepts(&fat_x86_and_arm64e));
        assert!(!accepts(&thin_arm64));
        assert!(!accepts(&future_x86_subtype));
        assert!(!accepts(b"#!/bin/sh\n"));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn arm64e_target_drops_only_the_managed_interposer() {
        let path = std::env::temp_dir().join(format!(
            "agsh-arm64e-target-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let fat_arm64e = [
            0xca, 0xfe, 0xba, 0xbe, 0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x0c, 0x80, 0x00,
            0x00, 0x02, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x0e,
        ];
        std::fs::write(&path, fat_arm64e).unwrap();
        let argv = [path.clone().into_os_string()];
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(b":/caller/observe.dylib"),
        ];

        let compatible = direct_target_environment(&argv, &environment)
            .expect("arm64e target must reject agsh's ordinary arm64 interposer");
        assert!(compatible.iter().any(|binding| {
            binding.as_bytes() == b"DYLD_INSERT_LIBRARIES=/caller/observe.dylib"
        }));
        assert!(compatible
            .iter()
            .any(|binding| binding.as_bytes() == b"AGSH_SELF=/tmp/agsh"));
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn plain_arm64_target_keeps_the_managed_interposer() {
        let path = std::env::temp_dir().join(format!(
            "agsh-arm64-target-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let thin_arm64 = [
            0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
        ];
        std::fs::write(&path, thin_arm64).unwrap();
        let argv = [path.clone().into_os_string()];
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(b""),
        ];

        assert!(direct_target_environment(&argv, &environment).is_none());
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn script_target_drops_the_managed_interposer() {
        let path = std::env::temp_dir().join(format!(
            "agsh-script-target-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let argv = [path.clone().into_os_string()];
        let environment = [
            CString::new("AGSH_SELF=/tmp/agsh").unwrap(),
            test_managed_preload(b""),
        ];

        let compatible = direct_target_environment(&argv, &environment)
            .expect("script target must not inherit agsh's ordinary arm64 interposer");
        assert!(!compatible
            .iter()
            .any(|binding| binding.as_bytes().starts_with(b"DYLD_INSERT_LIBRARIES=")));
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    #[test]
    fn x86_64_target_policy_strips_arm_only_and_keeps_x86_images() {
        let base = std::env::temp_dir().join(format!(
            "agsh-x86-target-policy-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&base).unwrap();
        let arm_target = base.join("arm64");
        let x86_target = base.join("x86_64");
        std::fs::write(
            &arm_target,
            [
                0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            ],
        )
        .unwrap();
        std::fs::write(
            &x86_target,
            [
                0xcf, 0xfa, 0xed, 0xfe, 0x07, 0x00, 0x00, 0x01, 0x03, 0x00, 0x00, 0x00,
            ],
        )
        .unwrap();
        let environment = [
            CString::new("AGSH_SELF=/overridden").unwrap(),
            test_managed_preload(b":/caller/observe.dylib"),
        ];

        let sanitized = direct_target_environment(&[arm_target.into_os_string()], &environment)
            .expect("arm-only target must reject an x86_64 interposer");
        assert!(sanitized.iter().any(|binding| {
            binding.as_bytes() == b"DYLD_INSERT_LIBRARIES=/caller/observe.dylib"
        }));
        assert!(direct_target_environment(&[x86_target.into_os_string()], &environment,).is_none());
        std::fs::remove_dir_all(base).unwrap();
    }
}
