use std::io::Write;
use std::process::{Command, Stdio};

fn run_interactive(input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agsh");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait agsh");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn project_env_requires_trust_then_activates() {
    let base = std::env::temp_dir().join(format!("agsh_env_{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::write(base.join(".env"), "MY_PROJECT_VAR=hello123\n").unwrap();
    let trust = base.join("trustfile");

    // Untrusted: the .env is NOT sourced.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "-c",
            &format!("cd {}; echo v=$MY_PROJECT_VAR", base.display()),
        ])
        .env("AGSH_TRUST_FILE", &trust)
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "v=\n");

    // After trust, it activates within the session.
    let mut child = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .env("AGSH_TRUST_FILE", &trust)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agsh");
    let script = format!("cd {}\nagtrust\necho v=$MY_PROJECT_VAR\n", base.display());
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("v=hello123"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn z_jumps_to_frecent_directory() {
    let base = std::env::temp_dir().join(format!("agsh_z_{}", std::process::id()));
    let api = base.join("backend-api");
    let web = base.join("frontend-web");
    std::fs::create_dir_all(&api).unwrap();
    std::fs::create_dir_all(&web).unwrap();
    let script = format!(
        "cd {a}\ncd {w}\nagz backend\npwd\n",
        a = api.display(),
        w = web.display()
    );
    let out = run_interactive(&script);
    assert!(
        out.contains("backend-api"),
        "z should jump to the frecent backend dir; output: {out}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn view_pipes_raw_bytes_unchanged() {
    // `view` renders for the human terminal, but piped/`-c` output must be the
    // exact file bytes (rendering only applies to the human display plane).
    let base = std::env::temp_dir().join(format!("agsh_view_{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let md = base.join("doc.md");
    std::fs::write(&md, "# Title\n\nbody\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &format!("agview {}", md.display())])
        .output()
        .expect("run agsh");
    // stdout is a pipe here (not a TTY), so bytes are raw.
    assert_eq!(String::from_utf8_lossy(&output.stdout), "# Title\n\nbody\n");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn runs_a_script_file_with_args() {
    let base = std::env::temp_dir().join(format!("agsh_scriptfile_{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let script = base.join("s.agsh");
    std::fs::write(
        &script,
        "echo \"first $1\"\nfor i in a b; do echo \"loop $i\"; done\nexit 4\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([script.to_str().unwrap(), "ARG1"])
        .output()
        .expect("run agsh");
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "first ARG1\nloop a\nloop b\n"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn script_file_heredoc_after_command() {
    let base = std::env::temp_dir().join(format!("agsh_hd_{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let script = base.join("h.agsh");
    std::fs::write(&script, "echo before\ncat <<EOF\nbody\nEOF\necho after\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .arg(script.to_str().unwrap())
        .output()
        .expect("run agsh");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "before\nbody\nafter\n"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn missing_script_file_exits_127() {
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .arg("/nonexistent/agsh-script-xyz.agsh")
        .output()
        .expect("run agsh");
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn risk_flags_dangerous_and_clears_safe() {
    let danger = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "risk 'rm -rf /'"])
        .output()
        .expect("run agsh");
    assert!(String::from_utf8_lossy(&danger.stdout).contains("fs.recursive_delete"));
    let safe = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "risk 'ls -la'"])
        .output()
        .expect("run agsh");
    assert!(String::from_utf8_lossy(&safe.stdout).contains("no findings"));
}

#[test]
fn context_json_has_fields() {
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "agcontext --json"])
        .output()
        .expect("run agsh");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("\"cwd\""));
    assert!(s.contains("\"recent\""));
    assert!(s.contains("\"last\""));
}

#[test]
fn peek_reads_line_range_with_numbers() {
    let base = std::env::temp_dir().join(format!("agsh_peek_{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let f = base.join("nums.txt");
    std::fs::write(&f, "one\ntwo\nthree\nfour\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &format!("peek {} --range 2:3", f.display())])
        .output()
        .expect("run agsh");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("2  two"));
    assert!(s.contains("3  three"));
    assert!(!s.contains("one"));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn patch_applies_diff_from_heredoc() {
    let base = std::env::temp_dir().join(format!("agsh_patch_{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let f = base.join("code.txt");
    std::fs::write(&f, "a\nb\nc\n").unwrap();
    let script = format!(
        "agpatch {} <<'EOF'\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\nEOF",
        f.display()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &script])
        .output()
        .expect("run agsh");
    assert!(out.status.success());
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "a\nB\nc\n");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn command_not_found_suggests_and_hints() {
    // A typo of a builtin yields a "did you mean" suggestion, exit 127.
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "ech hello"])
        .output()
        .expect("run agsh");
    assert_eq!(output.status.code(), Some(127));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Did you mean"), "stderr: {stderr}");
    assert!(stderr.contains("echo"), "stderr: {stderr}");

    // A known uninstalled tool yields an install hint (static, deterministic).
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "rg pattern"])
        .output()
        .expect("run agsh");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Install:"), "stderr: {stderr}");
    assert!(stderr.contains("ripgrep"), "stderr: {stderr}");
}

#[test]
fn per_command_output_wrapper_overrides_session_mode() {
    // `raw <cmd>` renders raw output even though the session default is semantic.
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "semantic", "-c", "raw echo plain123"])
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "plain123\n");

    // `semantic <cmd>` renders a JSON observation even in a raw session.
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "semantic true"])
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"exit_code\": 0"), "stdout: {stdout}");
}

#[test]
fn clean_mode_strips_ansi_and_redacts_secrets() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "--output",
            "clean",
            "-c",
            "printf '\\033[31mtok ghp_abcdefghijklmnopqrstuvwxyz0123\\033[0m\\n'",
        ])
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains('\u{1b}'), "ANSI not stripped: {stdout:?}");
    assert!(
        stdout.contains("[REDACTED]"),
        "secret not redacted: {stdout}"
    );
    assert!(!stdout.contains("ghp_"), "secret leaked: {stdout}");
}

#[test]
fn semantic_mode_emits_trace_refs_that_resolve() {
    // In a capturing session, a command's raw output is recorded and a later
    // `trace <id>` reads it back exactly.
    let out = run_interactive_args(
        &["--output", "clean"],
        "printf 'LINE1\\nLINE2\\n'\nagtrace cmd_0000000000000001\n",
    );
    assert!(out.contains("LINE1"), "output: {out}");
    assert!(out.contains("LINE2"), "output: {out}");
}

fn run_interactive_args(args: &[&str], input: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn agsh");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait agsh");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn multiline_if_block_continues_until_complete() {
    let out = run_interactive("if true\nthen\necho multiline-ok\nfi\n");
    assert!(out.contains("multiline-ok"), "output: {out}");
}

#[test]
fn multiline_for_loop_and_line_continuation() {
    let out = run_interactive("for i in 1 2\ndo\necho row$i\ndone\n");
    assert!(
        out.contains("row1") && out.contains("row2"),
        "output: {out}"
    );

    let cont = run_interactive("echo foo\\\nbar\n");
    assert!(cont.contains("foobar"), "output: {cont}");
}

#[test]
fn multiline_heredoc_via_interactive_input() {
    let out = run_interactive("cat <<EOF\nheredoc-body\nEOF\n");
    assert!(out.contains("heredoc-body"), "output: {out}");
}

#[test]
fn background_job_runs_and_wait_returns_status() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "sh -c 'exit 7' & wait %1"])
        .output()
        .expect("run agsh");
    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn background_job_prints_notice_to_stderr() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "sleep 0.2 & wait"])
        .output()
        .expect("run agsh");
    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[1]"), "expected job notice, got: {stderr}");
}

#[test]
fn jobs_lists_running_background_job_then_kill() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "sleep 3 & jobs; kill %1; wait; echo done"])
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[1]"), "jobs output: {stdout}");
    assert!(stdout.contains("Running"), "jobs output: {stdout}");
    assert!(stdout.contains("done"), "stdout: {stdout}");
}

#[test]
fn background_isolates_variable_changes() {
    // A backgrounded command runs in its own process; it cannot mutate the
    // parent shell's variables.
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "X=1; X=2 & wait; echo \"X=$X\""])
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "X=1\n");
}

#[test]
fn pty_broker_gives_child_a_tty() {
    // Even though agsh's own stdout here is a pipe (captured), the child run
    // under `pty` sees a TTY on stdout.
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "pty sh -c 'test -t 1 && echo isatty || echo notty'"])
        .output()
        .expect("run agsh");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("isatty"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn pty_broker_propagates_exit_code() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "pty sh -c 'exit 4'"])
        .output()
        .expect("run agsh");
    assert_eq!(output.status.code(), Some(4));
}

#[test]
fn signal_terminated_command_reports_128_plus_signal() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "sh -c 'kill -INT $$'"])
        .output()
        .expect("run agsh");
    // 128 + SIGINT(2) = 130.
    assert_eq!(output.status.code(), Some(130));
}

#[test]
fn sigint_interrupts_loop_without_killing_shell() {
    use std::time::{Duration, Instant};
    let mut child = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "while :; do sleep 0.05; done"])
        .spawn()
        .expect("spawn agsh");
    std::thread::sleep(Duration::from_millis(400));
    Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("send SIGINT");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            return; // the shell interrupted the loop and exited
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("agsh did not exit after SIGINT (loop not interrupted)");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn exec_replaces_agsh_process_for_raw_cli_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "exec /bin/sh -c 'printf exec-ok'"])
        .output()
        .expect("run agsh");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"exec-ok");
    assert!(output.stderr.is_empty());
}

#[test]
fn exec_is_disabled_when_cli_output_is_captured() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "--output",
            "semantic",
            "-c",
            "exec /bin/sh -c 'printf should-not-run'",
        ])
        .output()
        .expect("run agsh");

    assert_eq!(output.status.code(), Some(126));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"exit_code\": 126"));
    assert!(output.stderr.is_empty());
}

// ---- confine guardrail (command allowlist) --------------------------------

#[test]
fn confine_sticky_session_denies_nonallowlisted() {
    // The in-session `confine` builtin confines the current session: subsequent
    // non-allowlisted commands are denied (no output leaks). Works in an
    // already-open shell, not just at launch.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "confine echo; echo ok; uname"])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
    assert!(String::from_utf8_lossy(&out.stderr).contains("not permitted"));
}

#[test]
fn confine_inherited_from_env() {
    // A child agsh self-confines from AGSH_CONFINE (how confinement propagates).
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "echo ok; uname"])
        .env("AGSH_CONFINE", "echo")
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
}

#[test]
fn confine_propagates_to_child_agsh() {
    // The payload spawns a child agsh, which inherits the jail via AGSH_CONFINE.
    let bin = env!("CARGO_BIN_EXE_agsh");
    let dir = std::path::Path::new(bin).parent().unwrap();
    let path = format!(
        "{}:{}",
        dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new(bin)
        .args(["--allow", "echo", "--run", "agsh -c 'echo childok; uname'"])
        .env("PATH", path)
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Core guardrail invariant on every platform: the non-allowlisted `uname`
    // never runs, so its output never leaks.
    assert!(
        !stdout.contains("Darwin") && !stdout.contains("Linux"),
        "leak: {stdout:?}"
    );
    if cfg!(target_os = "macos") {
        // Seatbelt/shim enforcement: the allowlisted command still runs.
        assert!(stdout.contains("childok"), "stdout={stdout:?}");
    } else {
        // No kernel allowlist backend (e.g. Landlock) built in → fail closed:
        // refuse to run at all rather than enforce weakly. See docs/CONFINE.md.
        assert!(
            stderr.contains("cannot enforce"),
            "expected fail-closed, stderr={stderr:?}"
        );
    }
}

#[test]
fn confine_cannot_widen_itself() {
    // A confined session cannot grant itself more (narrow-only).
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "confine echo; confine echo,uname; uname"])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("Darwin"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not permitted"));
}

#[test]
fn confine_shims_intercept_agent_bash_tool() {
    // The real-world bypass: an agent runs commands via its own `bash -c '…'`
    // subprocess (not through agsh). With shims installed by `--allow`, that bash
    // is routed back through confined agsh, so a non-allowlisted command is denied.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--allow", "ls,df", "--run", "bash -c 'uname'"])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("Darwin") && !stdout.contains("Linux"),
        "leak: {stdout:?}"
    );
    if cfg!(target_os = "macos") {
        assert!(stderr.contains("not permitted"), "stderr={stderr:?}");
    } else {
        // Fail closed with no kernel allowlist backend (see docs/CONFINE.md).
        assert!(stderr.contains("cannot enforce"), "stderr={stderr:?}");
    }
}

#[test]
fn shims_only_installed_when_confined() {
    // A normal (unconfined) session must NOT touch PATH/SHELL or install shims.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "echo $PATH"])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("agsh-confine"),
        "unconfined session installed shims"
    );
}

#[test]
fn confine_shim_handles_login_and_rc_flags() {
    // Regression: a login shell with --norc/--rcfile etc. must not have its flags
    // mistaken for the `-c` command (the `--norc -l -c` bug). Allowed commands run.
    for inv in [
        "bash --norc -l -c 'echo OK'",
        "bash --noprofile --norc -l -c 'echo OK'",
        "bash --rcfile /tmp/nope -c 'echo OK'",
        "bash -lc 'echo OK'",
        "bash -il -c 'echo OK'",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["--allow", "echo", "--run", inv])
            .env_remove("AGSH_CONFINE")
            .output()
            .expect("run agsh");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if cfg!(target_os = "macos") {
            assert_eq!(stdout, "OK\n", "invocation {inv:?} stderr={stderr:?}");
        } else {
            // Fail closed without a kernel allowlist backend (see docs/CONFINE.md).
            assert!(
                stderr.contains("cannot enforce"),
                "invocation {inv:?} stderr={stderr:?}"
            );
        }
    }
    // ...and a denied command stays denied even with login/rc flags.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--allow", "echo", "--run", "bash --norc -l -c 'uname'"])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("Darwin") && !stdout.contains("Linux"),
        "leak: {stdout:?}"
    );
    if cfg!(target_os = "macos") {
        assert!(stderr.contains("not permitted"), "stderr={stderr:?}");
    } else {
        assert!(stderr.contains("cannot enforce"), "stderr={stderr:?}");
    }
}

// ---- OS-enforced confine (MILESTONE_CONFINE_OS) ---------------------------

#[test]
fn confine_refuses_self_managing_agent() {
    // A known agent is refused with guidance + exit 2 (rather than pretending).
    for args in [
        vec!["-c", "confine ls,df -- claude"],
        vec!["--allow", "ls,df", "--run", "claude"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(&args)
            .env_remove("AGSH_CONFINE")
            .output()
            .expect("run agsh");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("self-managing agent"), "{args:?}: {err:?}");
        assert!(err.contains("allowedTools"), "{args:?}");
    }
}

#[cfg(target_os = "macos")]
#[test]
fn confine_readonly_scrubs_credential_env() {
    // SHIP_READINESS_PLAN P0-3: a confining preset must not hand inherited
    // cloud/API tokens (or the ssh-agent socket) to the payload.
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "-c",
            "confine read-only -- sh -c 'echo [$AWS_SECRET_ACCESS_KEY][$SSH_AUTH_SOCK][$GITHUB_TOKEN]'",
        ])
        .env("AWS_SECRET_ACCESS_KEY", "SEKRET")
        .env("SSH_AUTH_SOCK", "/tmp/agent.sock")
        .env("GITHUB_TOKEN", "ghp_leak")
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(
        so.contains("[][][]"),
        "credential env leaked into confined payload: {so:?}"
    );
}

/// A `python3` that actually runs **under `sandbox-exec`** in this environment,
/// or `None` to skip. Homebrew's framework `python3` (common on CI macOS runners)
/// re-execs its inner interpreter and fails to `posix_spawn` it under Seatbelt, so
/// a bare `python3 --version` check is not enough. Probe under the *exec-allowlist*
/// mode (the strictest — a re-execing interpreter can't even start there), so the
/// interpreter we return is guaranteed usable by every confine test; prefer the
/// non-framework system python.
#[cfg(target_os = "macos")]
fn sandbox_capable_python() -> Option<&'static str> {
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return None;
    }
    for py in ["/usr/bin/python3", "python3"] {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", &format!("confine ls -- {py} -c 'print(\"ok\")'")])
            .env_remove("AGSH_CONFINE")
            .output();
        if matches!(out, Ok(ref o) if String::from_utf8_lossy(&o.stdout).contains("ok")) {
            return Some(py);
        }
    }
    None
}

#[cfg(target_os = "macos")]
#[test]
fn confine_leaf_payload_kernel_enforced() {
    // Real enforcement: a leaf payload runs, but the kernel denies any command off
    // the allowlist — via PATH bash, absolute /bin/bash, and python os.system —
    // while allowlisted commands work. Snapshot-proof (no shim dependency).
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    // ls allowed runs; du denied.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "-c",
            "confine ls -- bash -c 'ls / >/dev/null && echo ok; du -sh /tmp'",
        ])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    let so = String::from_utf8_lossy(&out.stdout);
    let se = String::from_utf8_lossy(&out.stderr);
    assert!(so.contains("ok"), "allowed ls didn't run: {so:?}");
    assert!(
        se.contains("Operation not permitted"),
        "du not denied: {se:?}"
    );

    // Denied via absolute /bin/bash always, and via python's os.system wherever a
    // sandbox-capable python exists; no output leak either way.
    let mut payloads = vec!["confine ls -- /bin/bash -c 'du -sh /tmp'".to_string()];
    if let Some(py) = sandbox_capable_python() {
        payloads.push(format!(
            "confine ls -- {py} -c 'import os; os.system(\"du -sh /tmp\")'"
        ));
    }
    for payload in payloads {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", &payload])
            .env_remove("AGSH_CONFINE")
            .output()
            .expect("run agsh");
        let so = String::from_utf8_lossy(&out.stdout);
        assert!(
            !so.contains("/tmp\t") && !so.contains("total"),
            "du leaked: {so:?}"
        );
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
            "payload {payload:?} not kernel-denied"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn confine_leaf_payload_quoting_preserved() {
    // The payload's argv (incl. quoted args like `python3 -c '…'`) must survive
    // the sh -c wrapper: the script runs, only its child command is denied.
    let Some(py) = sandbox_capable_python() else {
        return;
    };
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &format!("confine ls -- {py} -c 'print(\"py-ran\")'")])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "py-ran\n");
}

#[test]
fn confine_force_bypasses_agent_refusal() {
    // --force lets a known agent through (it won't be refused as an agent).
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "confine --force ls -- claude --version 2>&1 || true"])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("self-managing agent"),
        "still refused as agent under --force"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn confine_no_command_injection_via_tmpdir() {
    // Regression for the confirmed HIGH finding: a hostile $TMPDIR with a command
    // substitution must NOT execute when the sandbox wrapper string is re-parsed
    // by agsh (the profile path is single-quoted, not Rust Debug double-quoted).
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let marker = std::env::temp_dir().join(format!("agsh_pwn_{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let evil_dir = format!("/tmp/sb$(touch {})d", marker.display());
    std::fs::create_dir_all(&evil_dir).unwrap();
    let _ = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "confine ls -- /bin/echo hi"])
        .env("TMPDIR", format!("{evil_dir}/"))
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    let pwned = marker.exists();
    let _ = std::fs::remove_file(&marker);
    let _ = std::fs::remove_dir_all(&evil_dir);
    assert!(
        !pwned,
        "command injection via TMPDIR fired — confine bypassed"
    );
}

#[test]
fn confine_refuses_wrapped_agents() {
    // The refusal sees through wrappers and case (env/nice/Claude).
    for payload in [
        "confine ls -- env claude",
        "confine ls -- nice -n5 claude",
        "confine ls -- Claude",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", payload])
            .env_remove("AGSH_CONFINE")
            .output()
            .expect("run agsh");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("self-managing agent"),
            "{payload:?} not refused"
        );
    }
}

// ---- production robustness regressions ------------------------------------

#[test]
fn deep_arithmetic_errors_instead_of_crashing() {
    // Pathologically nested arithmetic must yield a clean error, never a stack
    // overflow / process abort. Integer $(()) and float `math` both guarded.
    for expr in [
        format!("echo $(( {}1{} ))", "(".repeat(50000), ")".repeat(50000)),
        format!("echo $(( {}0 ))", "~".repeat(50000)),
        format!("agmath \"{}1.0{}\"", "(".repeat(50000), ")".repeat(50000)),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", &expr])
            .output()
            .expect("run agsh");
        assert!(
            out.status.code().is_some(),
            "agsh crashed (signal) on deep nesting: {:?}",
            out.status
        );
        assert_ne!(out.status.code(), Some(0), "expected an error exit");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("too deeply"),
            "expected a 'nested too deeply' error"
        );
    }
    // Normal arithmetic still works.
    let ok = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "echo $(( (1+2)*3 + 2**4 ))"])
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&ok.stdout), "25\n");
}

#[test]
fn large_stdin_capture_does_not_deadlock() {
    // Capturing a command that echoes >128KB of stdin must not deadlock.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "x=$(cat <<< \"$(seq 50000)\"); echo len=${#x}"])
        .output()
        .expect("run agsh");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("len="));
    assert_ne!(String::from_utf8_lossy(&out.stdout), "len=0\n");
}

#[test]
fn pipe_to_early_closing_consumer_is_silent() {
    // `… | head` (consumer exits early) must not print a spurious "Broken pipe".
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "seq 100000 | head -1; echo ok"])
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\nok\n");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("Broken pipe"),
        "spurious broken-pipe message: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn view_of_binary_is_raw_when_not_a_tty() {
    // `view image.bin` piped/redirected must emit the exact bytes (no lossy UTF-8).
    let dir = std::env::temp_dir().join(format!("agsh_viewbin_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bin.png");
    let bytes: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01, 0xff, 0xfe,
    ];
    std::fs::write(&path, bytes).unwrap();
    let via_view = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &format!("agview {}", path.display())])
        .output()
        .expect("run agsh");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        via_view.stdout, bytes,
        "view corrupted the binary on a pipe"
    );
}

// ---- confine v2 capability presets (MILESTONE_CONFINE_V2) ------------------

#[cfg(target_os = "macos")]
#[test]
fn confine_read_only_blocks_write_delete_network_secrets() {
    let Some(py) = sandbox_capable_python() else {
        return; // no interpreter that runs under the sandbox here
    };
    let dir = std::env::temp_dir().join(format!("agsh_cv2_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let victim = dir.join("victim.txt");
    std::fs::write(&victim, "keep").unwrap();

    let run = |payload: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", payload])
            .env_remove("AGSH_CONFINE")
            .output()
            .expect("run agsh")
    };

    // os.remove (direct unlink) must be denied — the v1 gap, now closed.
    let _ = run(&format!(
        "confine read-only -- {py} -c 'import os; os.remove(\"{}\")'",
        victim.display()
    ));
    assert!(victim.exists(), "read-only failed to block os.remove");

    // A write outside scratch is denied.
    let outside = dir.join("new.txt");
    let _ = run(&format!(
        "confine read-only -- {py} -c 'open(\"{}\",\"w\").write(\"x\")'",
        outside.display()
    ));
    assert!(
        !outside.exists(),
        "read-only failed to block out-of-scratch write"
    );

    // Scratch (TMPDIR) IS writable, so tools that need temp still work.
    let ok = run(&format!("confine read-only -- {py} -c 'import os; p=os.path.join(os.environ[\"TMPDIR\"],\"s\"); open(p,\"w\").write(\"1\"); print(\"SCRATCH_OK\")'"));
    assert!(
        String::from_utf8_lossy(&ok.stdout).contains("SCRATCH_OK"),
        "scratch should be writable: {}",
        String::from_utf8_lossy(&ok.stderr)
    );

    // Network is denied.
    let net = run(&format!("confine read-only -- {py} -c 'import socket; socket.create_connection((\"1.1.1.1\",80),2); print(\"CONNECTED\")'"));
    assert!(
        !String::from_utf8_lossy(&net.stdout).contains("CONNECTED"),
        "read-only failed to block network"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "macos")]
#[test]
fn confine_offline_denies_network_but_allows_writes() {
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("agsh_off_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("w.txt");
    // offline = network off, filesystem unchanged → this write succeeds.
    let _ = std::process::Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "-c",
            &format!("confine offline -- /bin/sh -c 'echo hi > {}'", f.display()),
        ])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    assert!(f.exists(), "offline should leave writes alone");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn confine_parse_spec_classifies_tokens() {
    // Backend-independent: presets, flags, and allowlist entries by shape.
    let toks = |a: &[&str]| -> Vec<String> { a.iter().map(|s| s.to_string()).collect() };
    let (allow, opts) =
        agsh_exec::confine_parse_spec(&toks(&["ls,df", "read-only", "--no-net"])).expect("parse");
    assert_eq!(allow, vec!["ls".to_string(), "df".to_string()]);
    assert_eq!(opts.preset, agsh_exec::Preset::ReadOnly);
    assert_eq!(opts.net, Some(false));

    let (_, opts) =
        agsh_exec::confine_parse_spec(&toks(&["workspace", "--net", "--explain"])).expect("parse");
    assert_eq!(opts.preset, agsh_exec::Preset::Workspace);
    assert_eq!(opts.net, Some(true));
    assert!(opts.explain);

    // Bare list = exec-only (v1 back-compat).
    let (allow, opts) = agsh_exec::confine_parse_spec(&toks(&["ls"])).expect("parse");
    assert_eq!(allow, vec!["ls".to_string()]);
    assert_eq!(opts.preset, agsh_exec::Preset::ExecOnly);

    assert!(agsh_exec::confine_parse_spec(&toks(&["--bogus"])).is_err());
}

#[test]
fn sessions_lists_planted_claude_session_for_cwd() {
    // A planted Claude session under a temp HOME must be found for the matching cwd.
    let base = std::env::temp_dir().join(format!("agsh_sess_{}", std::process::id()));
    let proj = base.join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let proj = std::fs::canonicalize(&proj).unwrap();
    let home = base.join("home");
    let encoded = proj.to_string_lossy().replace('/', "-");
    let pdir = home.join(".claude/projects").join(&encoded);
    std::fs::create_dir_all(&pdir).unwrap();
    let line = format!(
        r#"{{"type":"user","cwd":"{}","message":{{"content":"hello from the test session"}}}}"#,
        proj.to_string_lossy()
    );
    std::fs::write(
        pdir.join("11111111-2222-3333-4444-555555555555.jsonl"),
        format!("{line}\n"),
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "sessions"])
        .env("HOME", &home)
        .current_dir(&proj)
        .output()
        .expect("run agsh");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Claude"), "no Claude session listed:\n{s}");
    assert!(
        s.contains("hello from the test session"),
        "summary missing:\n{s}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn observe_forwards_output_and_exit_code() {
    // `--observe CMD` runs CMD as a captured external command, forwarding its
    // output (raw = exact bytes) and exit code.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--observe", "printf", "a\nb\n"])
        .output()
        .expect("run agsh");
    assert_eq!(out.stdout, b"a\nb\n", "raw --observe must be exact bytes");
    assert!(out.status.success());

    let code = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--observe", "sh", "-c", "exit 7"])
        .status()
        .expect("run agsh")
        .code();
    assert_eq!(
        code,
        Some(7),
        "--observe must forward the child's exit code"
    );

    // A leading `--` separator is tolerated.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--observe", "--", "echo", "sep-ok"])
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "sep-ok");
}

#[test]
fn intercept_is_off_by_default_and_opt_in_routes_shells() {
    // Off by default: no shim dir on PATH.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "echo $PATH"])
        .env_remove("AGSH_INTERCEPT")
        .output()
        .expect("run agsh");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("agsh-intercept"),
        "interception must be off by default"
    );

    // Opt-in: the shim dir is prepended, and a `bash -c` routes through agsh back
    // to the real shell (output preserved).
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "echo $PATH; bash -c 'echo ROUTED42'"])
        .env("AGSH_INTERCEPT", "compact")
        .output()
        .expect("run agsh");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agsh-intercept"),
        "opt-in must install a shim dir:\n{text}"
    );
    assert!(
        text.contains("ROUTED42"),
        "routed bash output must survive:\n{text}"
    );
}

#[test]
fn intercept_native_flavor_interprets_in_agsh() {
    // `<mode>:native` routes a shell `-c` command into agsh's own interpreter.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "bash -c 'echo NATIVE-A | tr a-z A-Z'"])
        .env("AGSH_INTERCEPT", "compact:native")
        .output()
        .expect("run agsh");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("NATIVE-A"),
        "native-flavor interpret failed:\n{text}"
    );
}

// Tier 2 (exec-interposition) is platform/toolchain-sensitive: it needs the
// interposer dylib built and a C compiler, and DYLD_INSERT_LIBRARIES only works on
// non-hardened binaries. This test validates it where it can and skips otherwise.
#[cfg(target_os = "macos")]
#[test]
fn deep_intercept_catches_absolute_path_shell() {
    let exe = std::path::PathBuf::from(env!("CARGO_BIN_EXE_agsh"));
    let dir = exe.parent().unwrap();
    let lib = dir.join("libagsh_intercept.dylib");
    if !lib.exists() {
        eprintln!("skip: interposer dylib not built");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("agsh_deep_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let src = tmp.join("h.c");
    // The payload prints several lines: a tiny (≤3-line) success is shown
    // verbatim by compact mode's fast path, which would hide the [ok] framing
    // this test uses as proof of interception. Builtins only — an external
    // child (e.g. seq) inherits DYLD_INSERT_LIBRARIES and can abort on some
    // macOS CI images.
    std::fs::write(
        &src,
        r#"#include <unistd.h>
int main(void){char*a[]={"/bin/bash","-c","echo DEEP-TEST-HIT;echo L2;echo L3;echo L4;echo L5",0};execv("/bin/bash",a);return 1;}"#,
    )
    .unwrap();
    let bin = tmp.join("h");
    let cc = Command::new("cc").args(["-o"]).arg(&bin).arg(&src).status();
    if !matches!(cc, Ok(s) if s.success()) {
        eprintln!("skip: cc unavailable");
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    let out = Command::new(&bin)
        .env("DYLD_INSERT_LIBRARIES", &lib)
        .env("AGSH_SELF", &exe)
        .env("AGSH_INTERCEPT_MODE", "compact")
        .output()
        .expect("run harness");
    let text = String::from_utf8_lossy(&out.stdout);
    let _ = std::fs::remove_dir_all(&tmp);
    // Routed through agsh --observe: the marker survives AND the compact-summary
    // framing (`[ok]`) proves the absolute-path exec was captured, not run raw.
    assert!(text.contains("DEEP-TEST-HIT"), "marker missing:\n{text}");
    assert!(
        text.contains("[ok]"),
        "absolute-path exec was not intercepted (no compact framing):\n{text}"
    );
}

#[test]
fn mode_intercept_runtime_toggle() {
    let run = |script: &str| -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", script])
            .env_remove("AGSH_INTERCEPT")
            .output()
            .expect("run agsh");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    // Off by default; turning it on/off is reflected and mutates PATH for children.
    assert_eq!(run("mode:intercept").trim(), "off");
    assert!(run("mode:intercept compact; mode:intercept")
        .lines()
        .any(|l| l == "on"));
    assert!(run("mode:intercept compact; echo $PATH").contains("agsh-intercept"));
    let seq = run("mode:intercept compact; mode:intercept off; mode:intercept; echo P=$PATH");
    assert!(seq.lines().any(|l| l == "off"), "off not shown:\n{seq}");
    let path_line = seq.lines().find(|l| l.starts_with("P=")).unwrap();
    assert!(
        !path_line.contains("agsh-intercept"),
        "off must clean PATH:\n{path_line}"
    );
}

#[test]
fn compact_raw_ref_suppressed_when_shown_and_persisted_when_elided() {
    let bin = env!("CARGO_BIN_EXE_agsh");
    // Small output is fully shown → no redundant raw pointer.
    let out = Command::new(bin)
        .args(["--output", "compact", "-c", "echo small-output"])
        .output()
        .expect("run agsh");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("small-output"));
    assert!(
        !s.contains("raw:"),
        "fully-shown output must not carry a raw ref:\n{s}"
    );

    // Large output with $AGSH_TRACE_DIR → the ref is a catable file path that holds
    // the full raw output (resolvable across processes).
    let dir = std::env::temp_dir().join(format!("agsh_trace_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(bin)
        .args(["--output", "compact", "-c", "seq 1 600"])
        .env("AGSH_TRACE_DIR", &dir)
        .output()
        .expect("run agsh");
    let s = String::from_utf8_lossy(&out.stdout);
    let path = s
        .lines()
        .find_map(|l| l.strip_prefix("raw: "))
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or_else(|| panic!("expected a raw file ref for elided output:\n{s}"));
    let raw = std::fs::read_to_string(path).expect("trace file should exist on disk");
    assert_eq!(
        raw.lines().count(),
        600,
        "trace file must hold the full raw output"
    );
    assert!(raw.contains("600"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agtrace_grep_searches_a_trace_file() {
    let dir = std::env::temp_dir().join(format!("agsh_agtg_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("t.out");
    std::fs::write(&f, "alpha ok\nbeta error 1\ngamma ok\ndelta error 2\n").unwrap();
    // Matches → structured count + numbered lines, exit 0.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &format!("agtrace grep error {}", f.display())])
        .output()
        .expect("run agsh");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("[2 matches]"), "count header missing:\n{s}");
    assert!(
        s.contains("2: beta error 1") && s.contains("4: delta error 2"),
        "{s}"
    );
    assert_eq!(out.status.code(), Some(0));
    // No match → exit 1 (grep-style).
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &format!("agtrace grep zzz {}", f.display())])
        .output()
        .expect("run agsh");
    assert_eq!(out.status.code(), Some(1));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeated_command_not_found_advisory_shown_once() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agsh");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"ech a\nech b\nech c\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    // The error line + exit still happen every time; the advisory is shown once.
    assert_eq!(
        err.matches("command not found").count(),
        3,
        "errors:\n{err}"
    );
    assert_eq!(
        err.matches("Did you mean").count(),
        1,
        "advisory once:\n{err}"
    );
}

#[test]
fn intercept_sets_fail_fast_env_for_interactive_tools() {
    let bin = env!("CARGO_BIN_EXE_agsh");
    let run = |intercept: Option<&str>, git: Option<&str>| -> String {
        let mut c = Command::new(bin);
        c.args(["-c", "echo ${GIT_TERMINAL_PROMPT:-unset}"]);
        match intercept {
            Some(v) => {
                c.env("AGSH_INTERCEPT", v);
            }
            None => {
                c.env_remove("AGSH_INTERCEPT");
            }
        }
        match git {
            Some(v) => {
                c.env("GIT_TERMINAL_PROMPT", v);
            }
            None => {
                c.env_remove("GIT_TERMINAL_PROMPT");
            }
        }
        String::from_utf8_lossy(&c.output().unwrap().stdout)
            .trim()
            .to_string()
    };
    assert_eq!(run(None, None), "unset", "off by default");
    assert_eq!(
        run(Some("compact"), None),
        "0",
        "fail-fast under interception"
    );
    assert_eq!(run(Some("compact"), Some("1")), "1", "user value respected");
}

#[test]
fn agjob_runs_in_background_and_captures_output() {
    let dir = std::env::temp_dir().join(format!("agsh_agjob_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "agjob sh -c 'echo JOB-ONE; echo JOB-TWO'"])
        .env("AGSH_TRACE_DIR", &dir)
        .output()
        .expect("run agsh");
    let s = String::from_utf8_lossy(&out.stdout);
    // Returns immediately with a job id + the log path.
    let log = s
        .lines()
        .find_map(|l| l.split("output: ").nth(1))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| panic!("no job log path in:\n{s}"));
    // The detached job finishes shortly after; poll its log.
    let mut content = String::new();
    for _ in 0..100 {
        content = std::fs::read_to_string(&log).unwrap_or_default();
        if content.contains("JOB-TWO") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        content.contains("JOB-ONE") && content.contains("JOB-TWO"),
        "job output not captured to the log:\n{content}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- crash / DoS guards (SHIP_READINESS_PLAN P0-6, P0-7) -------------------
// Pathological input must terminate with a normal exit code — never be killed
// by a signal (SIGABRT from a stack overflow) and never hang/OOM. On Unix
// `status.code()` is `None` only when the child was terminated by a signal, so
// it is the direct "did the process crash?" probe.

fn agsh_dash_c(src: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", src])
        .output()
        .expect("run agsh")
}

#[test]
fn deep_arithmetic_nesting_errors_without_crashing() {
    // Ternary and assignment recursion in `$(( … ))` used to overflow the stack.
    let ternary = format!("echo $(( {}1{} ))", "1?".repeat(5000), ":1".repeat(5000));
    let assignment = format!("echo $(( {}1 ))", "a=".repeat(5000));
    for src in [ternary, assignment] {
        let out = agsh_dash_c(&src);
        assert!(
            out.status.code().is_some(),
            "killed by signal (stack overflow?) on {}…",
            &src[..40.min(src.len())]
        );
        assert_ne!(out.status.code(), Some(0), "expected a clean error exit");
    }
}

#[test]
fn deep_execution_nesting_errors_without_crashing() {
    // Nested command substitution, nested subshells, and unbounded function
    // recursion used to abort the whole process.
    let cmd_subst = format!("echo {}echo hi{}", "$(".repeat(2000), ")".repeat(2000));
    let subshell = format!("{}echo hi{}", "( ".repeat(2000), " )".repeat(2000));
    let recursion = "f() { f; }; f".to_string();
    for src in [cmd_subst, subshell, recursion] {
        let out = agsh_dash_c(&src);
        assert!(
            out.status.code().is_some(),
            "killed by signal (stack overflow?)"
        );
        assert_ne!(out.status.code(), Some(0), "expected a clean error exit");
    }
}

#[test]
fn pathological_brace_expansion_is_bounded() {
    // A range far over the element cap is left literal rather than allocating
    // ~1e9 strings.
    let over_cap = agsh_dash_c("echo {1..1000000000}");
    assert_eq!(
        String::from_utf8_lossy(&over_cap.stdout),
        "{1..1000000000}\n",
        "huge range should be left literal, not expanded"
    );
    // Ranges that step past i64 bounds must neither panic (debug) nor spin
    // forever via wraparound (release).
    for src in [
        "echo {9223372036854775806..9223372036854775807..5}",
        "echo {-9223372036854775808..-9223372036854775807}",
    ] {
        let out = agsh_dash_c(src);
        assert!(out.status.code().is_some(), "overflow crashed on {src}");
    }
    // Ordinary brace expansion is unaffected.
    let normal = agsh_dash_c("echo {1..5}; echo a{b,c}; echo {a..e}");
    assert_eq!(
        String::from_utf8_lossy(&normal.stdout),
        "1 2 3 4 5\nab ac\na b c d e\n"
    );
}

#[cfg(unix)]
#[test]
fn trace_files_are_private() {
    // SHIP_READINESS_PLAN P0-11: on-disk traces are unredacted and can contain
    // secrets, so the dir must be 0700 and files 0600 — not umask-default 0755/0644.
    use std::os::unix::fs::PermissionsExt;
    let base = std::env::temp_dir().join(format!("agsh-trace-perm-{}", std::process::id()));
    let dir = base.join("traces");
    let _ = std::fs::remove_dir_all(&base);
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "compact", "-c", "echo trace-me"])
        .env("AGSH_TRACE_DIR", &dir)
        .output()
        .expect("run agsh");
    assert!(out.status.success(), "agsh failed: {:?}", out.status);
    let dir_mode = std::fs::metadata(&dir)
        .expect("trace dir")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700, "trace dir not private: {dir_mode:o}");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read trace dir") {
        let entry = entry.unwrap();
        let mode = entry.metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "trace file {:?} not private: {mode:o}",
            entry.file_name()
        );
        checked += 1;
    }
    assert!(
        checked > 0,
        "expected at least one trace file to be written"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn set_bundled_and_multiple_flags_apply() {
    // SHIP_READINESS_PLAN P1-1: `set -euo pipefail` and other bundled/multi-flag
    // forms used to error and, worse, silently leave the options off.

    // A bundled `-eu` enables errexit, so a failure stops the script.
    let out = agsh_dash_c("set -eu; false; echo REACHED");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("REACHED"),
        "errexit from `set -eu` didn't stop execution"
    );
    assert_ne!(out.status.code(), Some(0));

    // `-euo pipefail` turns all three on (checked via `set -o`).
    let shown = agsh_dash_c("set -euo pipefail; set -o");
    let s = String::from_utf8_lossy(&shown.stdout);
    assert!(s.contains("errexit\ton"), "errexit not on: {s}");
    assert!(s.contains("nounset\ton"), "nounset not on: {s}");
    assert!(s.contains("pipefail\ton"), "pipefail not on: {s}");

    // `+e` turns errexit back off; `-o NAME` and operands still work.
    let out = agsh_dash_c("set -e -u; set +e; false; echo AFTER");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("AFTER"),
        "`set +e` should re-enable continuation"
    );
    let out = agsh_dash_c("set -o pipefail; set a b c; echo \"$1-$2-$3\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a-b-c\n");
    let out = agsh_dash_c("set -- -x -y; echo \"$1 $2\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "-x -y\n");

    // Unknown flag / unknown option name error with exit 2.
    assert_eq!(agsh_dash_c("set -Z").status.code(), Some(2));
    assert_eq!(agsh_dash_c("set -o bogus").status.code(), Some(2));
}

#[test]
fn type_and_command_v_report_all_builtins_truthfully() {
    // SHIP_READINESS_PLAN P1-2: introspection used the resolver's narrower list,
    // so builtins missing from it were misreported (`type getopts` → the external
    // /usr/bin/getopts; `type local` → not found). They must agree with execution.
    for name in [
        "getopts", "local", "trap", "declare", "let", "shift", "return", "readonly", ":", "times",
        "shopt", "complete",
    ] {
        let t = agsh_dash_c(&format!("type {name}"));
        assert!(
            String::from_utf8_lossy(&t.stdout).contains("is an agsh builtin"),
            "`type {name}` should report a builtin, got stdout={:?} stderr={:?}",
            String::from_utf8_lossy(&t.stdout),
            String::from_utf8_lossy(&t.stderr)
        );
        assert_eq!(t.status.code(), Some(0), "`type {name}` exit");

        let c = agsh_dash_c(&format!("command -v {name}"));
        assert_eq!(
            String::from_utf8_lossy(&c.stdout).trim(),
            name,
            "`command -v {name}` should print the name"
        );
        assert_eq!(c.status.code(), Some(0), "`command -v {name}` exit");
    }
}

#[test]
fn capturing_mode_bounds_large_output() {
    // SHIP_READINESS_PLAN P0-9: a capturing mode must drain + bound huge output
    // instead of buffering it all. 200 MB (>> the 2 MiB cap) must complete and
    // preserve the exit code — not hang or OOM — and agsh's emitted output stays
    // bounded, nowhere near 200 MB. (The head+tail retention is unit-tested in
    // `read_capped`; this is the end-to-end no-OOM/no-hang guard.)
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "--output",
            "compact",
            "-c",
            "head -c 200000000 /dev/zero; exit 4",
        ])
        .output()
        .expect("run agsh");
    assert!(out.status.code().is_some(), "killed by signal (hang/OOM?)");
    assert_eq!(out.status.code(), Some(4), "exit code lost through capture");
    assert!(
        out.stdout.len() < 8 * 1024 * 1024,
        "capture not bounded: emitted {} bytes",
        out.stdout.len()
    );
}

/// Run `agsh -c src`, killing it after `secs` and returning `None` on timeout,
/// so a pipeline-backpressure regression fails cleanly instead of hanging CI.
fn agsh_c_timeout(src: &str, secs: u64) -> Option<std::process::Output> {
    use std::io::Read;
    let mut child = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", src])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agsh");
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut r) = child.stdout.take() {
                let _ = r.read_to_end(&mut stdout);
            }
            if let Some(mut r) = child.stderr.take() {
                let _ = r.read_to_end(&mut stderr);
            }
            return Some(std::process::Output {
                status,
                stdout,
                stderr,
            });
        }
        if start.elapsed() > std::time::Duration::from_secs(secs) {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn pipeline_infinite_producer_stops_on_early_consumer_exit() {
    // SHIP_READINESS_PLAN P0-8: a compound/loop producer piped into a consumer
    // that exits early (`… | head`) must stop (SIGPIPE-like) rather than running
    // forever (or to completion) first. External and builtin producers both.
    for (src, want) in [
        ("{ yes; } | head -n1", "y\n"),
        ("(yes) | head -n1", "y\n"),
        ("while true; do echo x; done | head -n1", "x\n"),
        (
            "for i in $(seq 1 100000); do echo $i; done | head -n1",
            "1\n",
        ),
    ] {
        let out = agsh_c_timeout(src, 20)
            .unwrap_or_else(|| panic!("{src:?} did not terminate (backpressure regression)"));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            want,
            "wrong output for {src:?}"
        );
    }
}

#[test]
fn pipeline_compound_producer_streams_in_order() {
    // The backpressure fix must not disturb normal streaming/ordering when the
    // consumer reads everything.
    let out = agsh_c_timeout("{ echo one; seq 2; echo three; } | cat", 20).expect("hang");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "one\n1\n2\nthree\n");
    let out = agsh_c_timeout("{ echo a; echo b; echo c; } | grep b", 20).expect("hang");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "b\n");
}

#[test]
fn substitution_scanning_honors_quotes() {
    // SHIP_READINESS_PLAN P1-3: quotes inside $(…) / ${…} must not terminate the
    // substitution early — these all used to error "unterminated quoted string".
    for (src, want) in [
        ("echo $(echo ')')", ")\n"),
        ("echo $(echo \"a)b\")", "a)b\n"),
        ("echo $(printf '%s' 'a)b')", "a)b\n"),
        ("echo $(echo \"$(echo nested)\")", "nested\n"),
        ("echo $(echo $(echo hi))", "hi\n"),
    ] {
        let out = agsh_dash_c(src);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            want,
            "src={src:?} stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.status.code(), Some(0), "src={src:?}");
    }
}

#[test]
fn capturing_mode_forces_c_locale_for_parsers() {
    // SHIP_READINESS_PLAN P1-12: agent capturing modes run externals under LC_ALL=C
    // so localized output (e.g. `git status` in a non-English locale) can't fool
    // the heuristic compactors into e.g. reporting a dirty tree as "clean".
    let compact = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "compact", "-c", "printenv LC_ALL"])
        .env("LC_ALL", "de_DE.UTF-8")
        .output()
        .expect("run agsh");
    let cs = String::from_utf8_lossy(&compact.stdout);
    assert!(
        cs.lines().any(|l| l.trim() == "C"),
        "compact should force LC_ALL=C: {cs:?}"
    );
    assert!(
        !cs.contains("de_DE"),
        "compact leaked the user locale to the child: {cs:?}"
    );

    // Raw mode must not alter the child's locale (exact-bytes contract).
    let raw = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "raw", "-c", "printenv LC_ALL"])
        .env("LC_ALL", "de_DE.UTF-8")
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&raw.stdout), "de_DE.UTF-8\n");
}

#[test]
fn help_builtin_lists_and_details_commands() {
    // Bare `help` is a readable overview naming the agsh-specific tools.
    let out = agsh_dash_c("help");
    assert_eq!(out.status.code(), Some(0));
    let overview = String::from_utf8_lossy(&out.stdout);
    for needle in ["mode:output", "agview", "confine", "sessions", "agtrace"] {
        assert!(
            overview.contains(needle),
            "help overview missing {needle:?}"
        );
    }

    // `help <command>` gives detail for that command.
    let out = agsh_dash_c("help mode");
    assert_eq!(out.status.code(), Some(0));
    let detail = String::from_utf8_lossy(&out.stdout);
    assert!(
        detail.contains("lossless-ref"),
        "help mode missing detail: {detail:?}"
    );

    // An unknown topic fails (exit 1) and explains itself on stderr, not stdout.
    let out = agsh_dash_c("help frobnicate");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no help topic"));

    // And it is registered as a builtin (registries agree — type resolves it).
    let out = agsh_dash_c("type help");
    assert!(String::from_utf8_lossy(&out.stdout).contains("builtin"));
}

#[test]
fn resume_restores_a_dead_sessions_state_and_consumes_the_journal() {
    let base = std::env::temp_dir().join(format!("agsh_resume_{}", std::process::id()));
    let sessions = base.join("sessions");
    let workdir = base.join("proj");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();

    // A journal from a session that died (no `exit` record, pid can't exist).
    let journal = format!(
        concat!(
            "{{\"e\":\"start\",\"id\":\"t1\",\"pid\":99999999,\"cwd\":\"/\",",
            "\"host\":\"h\",\"at\":1,\"version\":\"0\"}}\n",
            "{{\"e\":\"cwd\",\"path\":\"{workdir}\"}}\n",
            "{{\"e\":\"env\",\"k\":\"API_URL\",\"v\":\"http://localhost:9\"}}\n",
            "{{\"e\":\"alias\",\"k\":\"gs\",\"v\":\"git status\"}}\n",
            "{{\"e\":\"job\",\"pgid\":99999998,\"cmd\":\"npm run dev &\",\"at\":2}}\n",
            "{{\"e\":\"fg\",\"cmd\":\"claude\",\"at\":2}}\n",
        ),
        workdir = workdir.display()
    );
    std::fs::write(sessions.join("t1.jsonl"), &journal).unwrap();

    // `resume list` sees it, with what was running.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "resume list"])
        .env("AGSH_SESSION_DIR", &sessions)
        .output()
        .expect("run agsh");
    let listing = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stderr: {listing}");
    assert!(listing.contains("claude"), "listing: {listing}");

    // `resume` replays cwd + env + alias into the live session.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "resume; pwd; echo url=$API_URL; alias gs"])
        .env("AGSH_SESSION_DIR", &sessions)
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&workdir.display().to_string()),
        "cwd not restored: {stdout}"
    );
    assert!(stdout.contains("url=http://localhost:9"), "env: {stdout}");
    assert!(stdout.contains("git status"), "alias: {stdout}");
    assert!(
        stdout.contains("claude") && stdout.contains("sessions"),
        "agent resume hint missing: {stdout}"
    );
    assert!(
        stdout.contains("npm run dev") && stdout.contains("died"),
        "dead background job not reported: {stdout}"
    );

    // The journal is consumed: a second resume finds nothing.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "resume"])
        .env("AGSH_SESSION_DIR", &sessions)
        .output()
        .expect("run agsh");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nothing to restore"));

    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn compact_pwd_matches_raw_pwd() {
    // A successful tiny output IS its most compact form: `compact pwd` must
    // print exactly what `pwd` prints — no [ok] header, no counts scaffolding,
    // and no workspace shortening of the answer to "." (user report).
    let raw = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "pwd"])
        .output()
        .expect("run agsh");
    let compact = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "compact", "-c", "pwd"])
        .output()
        .expect("run agsh");
    assert_eq!(
        String::from_utf8_lossy(&compact.stdout),
        String::from_utf8_lossy(&raw.stdout),
        "compact pwd must equal raw pwd"
    );
}
