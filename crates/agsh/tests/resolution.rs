#![cfg(unix)]

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn test_dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("agsh-resolution-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("create resolver integration test directory");
    path
}

fn isolated_agsh(base: &Path) -> Command {
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("create isolated HOME");
    let mut command = Command::new(env!("CARGO_BIN_EXE_agsh"));
    command
        .current_dir(base)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", base.join("xdg-config"))
        .env("XDG_DATA_HOME", base.join("xdg-data"))
        .env("XDG_STATE_HOME", base.join("xdg-state"))
        .env("AGSH_HISTORY_FILE", base.join("history.jsonl"))
        .env("AGSH_TRUST_FILE", base.join("trust.jsonl"))
        .env("AGSH_SESSION_DIR", base.join("sessions"))
        .env("AGSH_BROKER_DIR", base.join("broker"))
        .env("AGSH_TRACE_DIR", base.join("traces"))
        .env("AGSH_NORC", "1")
        .env_remove("AGSH_BROKER_SOCKET")
        .env_remove("AGSH_CONFINE")
        .env_remove("AGSH_CONFINE_AGENTS")
        .env_remove("AGSH_CONFINE_ALLOW_AGENTS")
        .env_remove("AGSH_CONFINE_RUNTIME")
        .env_remove("AGSH_ICONS")
        .env_remove("AGSH_INTERCEPT")
        .env_remove("AGSH_INTERCEPT_ACTIVE")
        .env_remove("AGSH_INTERCEPT_MODE")
        .env_remove("AGSH_KEEP_ID")
        .env_remove("AGSH_KEPT")
        .env_remove("AGSH_OUTPUT_MODE")
        .env_remove("AGSH_RC")
        .env_remove("AGSH_RESUME_BANNER")
        .env_remove("AGSH_SELF")
        .env_remove("AGSH_SESSION")
        .env_remove("AGSH_THEME_FILE")
        .env_remove("AGSH_TOKEN_CONFIG")
        .env_remove("AGSH_TRACE_DIR_CAP")
        .env_remove("BASH_ENV")
        .env_remove("DYLD_INSERT_LIBRARIES")
        .env_remove("ENV")
        .env_remove("LD_PRELOAD")
        .env_remove("NO_COLOR")
        .env_remove("ZDOTDIR");
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(agsh_output::is_sensitive_env_name)
        {
            command.env_remove(name);
        }
    }
    command
}

fn write_script(path: &Path, output: &str, mode: u32) {
    std::fs::write(path, format!("#!/bin/sh\nprintf '%s\\n' '{output}'\n"))
        .expect("write resolver integration test executable");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .expect("set resolver integration test permissions");
}

fn cannot_execute_directly(path: &Path) -> bool {
    matches!(
        Command::new(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied
    )
}

#[test]
fn path_search_skips_an_inaccessible_earlier_candidate() {
    let base = test_dir("skip-inaccessible");
    let first_dir = base.join("first");
    let second_dir = base.join("second");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::create_dir_all(&second_dir).unwrap();
    let first = first_dir.join("agsh-path-probe");
    let second = second_dir.join("agsh-path-probe");
    write_script(&first, "wrong", 0o001);
    write_script(&second, "right", 0o700);
    if !cannot_execute_directly(&first) {
        let _ = std::fs::remove_dir_all(base);
        return;
    }

    let path = format!("{}:{}", first_dir.display(), second_dir.display());
    let output = isolated_agsh(&base)
        .args(["-c", "agsh-path-probe"])
        .env("PATH", path)
        .output()
        .expect("run agsh PATH resolution probe");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert_eq!(output.stdout, b"right\n");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn explicit_existing_non_executable_command_returns_126() {
    let base = test_dir("explicit-non-executable");
    let candidate = base.join("agsh-path-probe");
    write_script(&candidate, "unused", 0o600);
    let source = candidate.to_str().expect("UTF-8 temp path");

    let output = isolated_agsh(&base)
        .args(["-c", source])
        .output()
        .expect("run explicit non-executable command");

    assert_eq!(output.status.code(), Some(126));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("permission denied"),
        "stderr={stderr:?}"
    );
    assert!(!stderr.contains("command not found"), "stderr={stderr:?}");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn cached_command_is_re_resolved_after_it_becomes_inaccessible() {
    let base = test_dir("cache-permissions");
    let first_dir = base.join("first");
    let second_dir = base.join("second");
    std::fs::create_dir_all(&first_dir).unwrap();
    std::fs::create_dir_all(&second_dir).unwrap();
    let first = first_dir.join("agsh-path-probe");
    let second = second_dir.join("agsh-path-probe");
    write_script(&first, "first", 0o700);
    write_script(&second, "second", 0o700);

    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o001)).unwrap();
    if !cannot_execute_directly(&first) {
        let _ = std::fs::remove_dir_all(base);
        return;
    }
    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o700)).unwrap();

    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path = format!(
        "{}:{}:{}",
        first_dir.display(),
        second_dir.display(),
        inherited_path
    );
    let source = format!(
        "agsh-path-probe; chmod 001 '{}'; agsh-path-probe",
        first.display()
    );
    let output = isolated_agsh(&base)
        .args(["-c", &source])
        .env("PATH", path)
        .output()
        .expect("run cached PATH resolution probe");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert_eq!(output.stdout, b"first\nsecond\n");
    let _ = std::fs::remove_dir_all(base);
}

fn write_executable_fixture(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write executable fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("make fixture executable");
}

fn assert_agsh_status_matrix(base: &Path, path: &Path, expected: i32) {
    let path = path.to_str().expect("UTF-8 fixture path");
    let cases = [
        ("normal", path.to_string()),
        ("list", format!("true; {path}")),
        ("streaming pipeline", format!("printf input | {path}")),
        ("PTY", format!("pty {path}")),
        ("exec", format!("exec {path}")),
    ];

    for (route, source) in cases {
        let output = isolated_agsh(base)
            .args(["-c", &source])
            .output()
            .unwrap_or_else(|error| panic!("run {route} launch-failure probe: {error}"));
        assert_eq!(
            output.status.code(),
            Some(expected),
            "route={route} source={source:?} stdout={:?} stderr={:?}",
            output.stdout,
            output.stderr
        );
    }
}

#[test]
fn invalid_executable_image_returns_126_across_launch_routes() {
    let base = test_dir("bad-executable-image");
    let candidate = base.join("bad-image");
    write_executable_fixture(&candidate, b"\0not an executable image\n");

    assert_agsh_status_matrix(&base, &candidate, 126);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn executable_text_without_a_shebang_preserves_shell_fallback() {
    let base = test_dir("executable-text-fallback");
    let candidate = base.join("text-script");
    write_executable_fixture(&candidate, b"exit 42\n");

    assert_agsh_status_matrix(&base, &candidate, 42);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn missing_shebang_interpreter_is_found_but_unlaunchable() {
    let base = test_dir("missing-shebang-interpreter");
    let candidate = base.join("missing-interpreter");
    write_executable_fixture(
        &candidate,
        b"#!/definitely/missing/agsh-interpreter\nprintf unreachable\\n\n",
    );

    // Reference shells vary by version/platform between reserved statuses 126
    // and 127 here. They agree that the script was found but could not launch;
    // agsh deliberately uses 126 while the resolved script still exists.
    for shell in [Path::new("/bin/sh"), Path::new("/bin/bash")] {
        if !shell.is_file() {
            continue;
        }
        let status = Command::new(shell)
            .args([
                "-c",
                "\"$1\"",
                shell.to_str().unwrap(),
                candidate.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run reference shell missing-interpreter probe")
            .code();
        assert!(
            matches!(status, Some(126 | 127)),
            "reference shell {} returned {status:?}",
            shell.display()
        );
    }

    assert_agsh_status_matrix(&base, &candidate, 126);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn disappeared_or_never_existing_command_returns_127() {
    let base = test_dir("missing-at-launch");
    let missing = base.join("does-not-exist");

    let output = isolated_agsh(&base)
        .args(["-c", missing.to_str().expect("UTF-8 fixture path")])
        .output()
        .expect("run missing command probe");
    assert_eq!(output.status.code(), Some(127));

    let _ = std::fs::remove_dir_all(base);
}
