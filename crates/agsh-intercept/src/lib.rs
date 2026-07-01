//! `agsh-intercept` — the optional exec-interposition layer for agsh shell
//! interception (Tier 2).
//!
//! The PATH/`$SHELL` shims installed by `agsh` catch shells resolved *by name*.
//! A program that calls `/bin/bash` by **absolute path** bypasses them. This tiny
//! shared library, injected via `DYLD_INSERT_LIBRARIES` (macOS) / `LD_PRELOAD`
//! (Linux), interposes the `exec`/`posix_spawn` family: when a process tries to run
//! a shell, the call is rewritten to run `agsh --observe <shell> …` instead, so the
//! shell's output is captured and rendered like everything else.
//!
//! It only acts when the session opted in. It reads:
//!
//! * `AGSH_SELF` — absolute path to the `agsh` binary (required)
//! * `AGSH_INTERCEPT_MODE` — output mode to render in (default `compact`)
//! * `AGSH_INTERCEPT_ACTIVE` — if set, do nothing (already inside an observed
//!   subtree — prevents recursion)
//!
//! With `AGSH_SELF` unset it is completely inert, so merely having it preloaded
//! changes nothing.
//!
//! Caveats: macOS SIP / hardened-runtime binaries strip `DYLD_INSERT_LIBRARIES`;
//! `LD_PRELOAD` is ignored by static binaries and across setuid execs. Those cases
//! fall back to the (still active) PATH shims.
//!
//! This crate is the single, isolated `unsafe` exception in the workspace; it does
//! not opt into the `unsafe_code = "forbid"` lint. Everything else stays unsafe-free.

#![cfg(unix)]

use libc::{c_char, c_int, c_void};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;

type ExecveFn =
    unsafe extern "C" fn(*const c_char, *const *const c_char, *const *const c_char) -> c_int;
type ExecvFn = unsafe extern "C" fn(*const c_char, *const *const c_char) -> c_int;
type PosixSpawnFn = unsafe extern "C" fn(
    *mut libc::pid_t,
    *const c_char,
    *const libc::posix_spawn_file_actions_t,
    *const libc::posix_spawnattr_t,
    *const *mut c_char,
    *const *mut c_char,
) -> c_int;

// Reaching the *real* libc implementation differs by platform:
//   * macOS `__interpose` does NOT redirect the interposing image's own references,
//     so we call the libc function directly (calling it here reaches the original).
//     Using `dlsym(RTLD_NEXT, …)` here would return *our* hook → infinite recursion.
//   * Linux `LD_PRELOAD` shadows the symbol everywhere including here, so we must
//     look up the next definition with `dlsym(RTLD_NEXT, …)`.
#[cfg(target_os = "linux")]
unsafe fn real_sym<T>(name: &[u8]) -> T {
    debug_assert_eq!(
        *name.last().unwrap(),
        0,
        "symbol name must be NUL-terminated"
    );
    let ptr = libc::dlsym(libc::RTLD_NEXT, name.as_ptr() as *const c_char);
    debug_assert!(!ptr.is_null());
    std::mem::transmute_copy::<*mut c_void, T>(&ptr)
}

macro_rules! real_getter {
    ($name:ident, $ty:ty, $sym:literal, $direct:path) => {
        #[cfg(target_os = "macos")]
        unsafe fn $name() -> $ty {
            $direct
        }
        #[cfg(target_os = "linux")]
        unsafe fn $name() -> $ty {
            real_sym(concat!($sym, "\0").as_bytes())
        }
    };
}
real_getter!(real_execve, ExecveFn, "execve", libc::execve);
real_getter!(real_execv, ExecvFn, "execv", libc::execv);
real_getter!(real_execvp, ExecvFn, "execvp", libc::execvp);
real_getter!(
    real_posix_spawn,
    PosixSpawnFn,
    "posix_spawn",
    libc::posix_spawn
);
real_getter!(
    real_posix_spawnp,
    PosixSpawnFn,
    "posix_spawnp",
    libc::posix_spawnp
);

fn os_to_cstring(s: &OsStr) -> Option<CString> {
    CString::new(s.as_bytes()).ok()
}

/// The interception config, or `None` when interception should not act (opted out,
/// or already inside an observed subtree).
fn config() -> Option<(CString, CString)> {
    if std::env::var_os("AGSH_INTERCEPT_ACTIVE").is_some() {
        return None;
    }
    let agsh = os_to_cstring(&std::env::var_os("AGSH_SELF")?)?;
    let mode = os_to_cstring(
        &std::env::var_os("AGSH_INTERCEPT_MODE").unwrap_or_else(|| OsString::from("compact")),
    )?;
    Some((agsh, mode))
}

/// True if `path`'s basename is a shell we want to route through agsh.
unsafe fn is_shell(path: *const c_char) -> bool {
    if path.is_null() {
        return false;
    }
    let bytes = CStr::from_ptr(path).to_bytes();
    let base = match bytes.iter().rposition(|&b| b == b'/') {
        Some(i) => &bytes[i + 1..],
        None => bytes,
    };
    matches!(base, b"bash" | b"sh" | b"zsh" | b"dash" | b"ksh")
}

/// Build the rewritten argv: `agsh --output MODE --observe <path> <original args…>`.
/// Returns owned strings that must outlive the exec call, plus the NULL-terminated
/// pointer array. The `path` and original `argv[1..]` pointers are borrowed from the
/// caller (valid for the duration of the intercepted call).
unsafe fn rewrite_argv(
    agsh: &CStr,
    mode: &CStr,
    path: *const c_char,
    argv: *const *const c_char,
) -> (Vec<CString>, Vec<*const c_char>) {
    let out = CString::new("--output").unwrap();
    let obs = CString::new("--observe").unwrap();
    let mut new: Vec<*const c_char> = vec![
        agsh.as_ptr(),
        out.as_ptr(),
        mode.as_ptr(),
        obs.as_ptr(),
        path,
    ];
    let mut i = 1isize;
    while !(*argv.offset(i)).is_null() {
        new.push(*argv.offset(i));
        i += 1;
    }
    new.push(std::ptr::null());
    (vec![out, obs], new)
}

// --- The hooks (shared logic; registered per-platform below) ----------------

unsafe extern "C" fn hook_execve(
    path: *const c_char,
    argv: *const *const c_char,
    envp: *const *const c_char,
) -> c_int {
    let real = real_execve();
    if let Some((agsh, mode)) = config() {
        if is_shell(path) {
            let (_own, new_argv) = rewrite_argv(&agsh, &mode, path, argv);
            return real(agsh.as_ptr(), new_argv.as_ptr(), envp);
        }
    }
    real(path, argv, envp)
}

unsafe extern "C" fn hook_execv(path: *const c_char, argv: *const *const c_char) -> c_int {
    let real = real_execv();
    if let Some((agsh, mode)) = config() {
        if is_shell(path) {
            let (_own, new_argv) = rewrite_argv(&agsh, &mode, path, argv);
            return real(agsh.as_ptr(), new_argv.as_ptr());
        }
    }
    real(path, argv)
}

unsafe extern "C" fn hook_execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
    let real = real_execvp();
    if let Some((agsh, mode)) = config() {
        if is_shell(file) {
            let (_own, new_argv) = rewrite_argv(&agsh, &mode, file, argv);
            // agsh path is absolute, so execvp resolves it directly.
            return real(agsh.as_ptr(), new_argv.as_ptr());
        }
    }
    real(file, argv)
}

unsafe extern "C" fn hook_posix_spawn(
    pid: *mut libc::pid_t,
    path: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    let real = real_posix_spawn();
    if let Some((agsh, mode)) = config() {
        if is_shell(path) {
            let (_own, new_argv) = rewrite_argv(&agsh, &mode, path, argv as *const *const c_char);
            return real(
                pid,
                agsh.as_ptr(),
                file_actions,
                attrp,
                new_argv.as_ptr() as *const *mut c_char,
                envp,
            );
        }
    }
    real(pid, path, file_actions, attrp, argv, envp)
}

unsafe extern "C" fn hook_posix_spawnp(
    pid: *mut libc::pid_t,
    file: *const c_char,
    file_actions: *const libc::posix_spawn_file_actions_t,
    attrp: *const libc::posix_spawnattr_t,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    let real = real_posix_spawnp();
    if let Some((agsh, mode)) = config() {
        if is_shell(file) {
            let (_own, new_argv) = rewrite_argv(&agsh, &mode, file, argv as *const *const c_char);
            return real(
                pid,
                agsh.as_ptr(),
                file_actions,
                attrp,
                new_argv.as_ptr() as *const *mut c_char,
                envp,
            );
        }
    }
    real(pid, file, file_actions, attrp, argv, envp)
}

// --- Registration -----------------------------------------------------------

/// macOS: pair each hook with the libc symbol via the `__DATA,__interpose` section.
#[cfg(target_os = "macos")]
mod register {
    use super::*;

    #[repr(C)]
    struct Interpose {
        replacement: *const c_void,
        replacee: *const c_void,
    }
    unsafe impl Sync for Interpose {}

    macro_rules! interpose {
        ($name:ident, $hook:expr, $real:path) => {
            #[used]
            #[link_section = "__DATA,__interpose"]
            static $name: Interpose = Interpose {
                replacement: $hook as *const c_void,
                replacee: $real as *const c_void,
            };
        };
    }

    interpose!(I_EXECVE, hook_execve, libc::execve);
    interpose!(I_EXECV, hook_execv, libc::execv);
    interpose!(I_EXECVP, hook_execvp, libc::execvp);
    interpose!(I_POSIX_SPAWN, hook_posix_spawn, libc::posix_spawn);
    interpose!(I_POSIX_SPAWNP, hook_posix_spawnp, libc::posix_spawnp);
}

/// Linux: export same-named symbols so `LD_PRELOAD` shadows libc; the real ones are
/// reached via `dlsym(RTLD_NEXT, …)` inside each hook.
#[cfg(target_os = "linux")]
mod register {
    use super::*;

    #[no_mangle]
    pub unsafe extern "C" fn execve(
        path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> c_int {
        hook_execve(path, argv, envp)
    }
    #[no_mangle]
    pub unsafe extern "C" fn execv(path: *const c_char, argv: *const *const c_char) -> c_int {
        hook_execv(path, argv)
    }
    #[no_mangle]
    pub unsafe extern "C" fn execvp(file: *const c_char, argv: *const *const c_char) -> c_int {
        hook_execvp(file, argv)
    }
    #[no_mangle]
    pub unsafe extern "C" fn posix_spawn(
        pid: *mut libc::pid_t,
        path: *const c_char,
        fa: *const libc::posix_spawn_file_actions_t,
        attr: *const libc::posix_spawnattr_t,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> c_int {
        hook_posix_spawn(pid, path, fa, attr, argv, envp)
    }
    #[no_mangle]
    pub unsafe extern "C" fn posix_spawnp(
        pid: *mut libc::pid_t,
        file: *const c_char,
        fa: *const libc::posix_spawn_file_actions_t,
        attr: *const libc::posix_spawnattr_t,
        argv: *const *mut c_char,
        envp: *const *mut c_char,
    ) -> c_int {
        hook_posix_spawnp(pid, file, fa, attr, argv, envp)
    }
}
