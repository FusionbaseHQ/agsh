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
    assert!(stdout.contains("childok"), "stdout={stdout:?}");
    assert!(
        !stdout.contains("Darwin") && !stdout.contains("Linux"),
        "leak: {stdout:?}"
    );
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
    assert!(
        !stdout.contains("Darwin") && !stdout.contains("Linux"),
        "leak: {stdout:?}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not permitted"));
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
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "OK\n",
            "invocation {inv:?} stderr={:?}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // ...and a denied command stays denied even with login/rc flags.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--allow", "echo", "--run", "bash --norc -l -c 'uname'"])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("Darwin") && !stdout.contains("Linux"),
        "leak: {stdout:?}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not permitted"));
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

    // Denied via absolute /bin/bash and via python's os.system; no output leak.
    for payload in [
        "confine ls -- /bin/bash -c 'du -sh /tmp'",
        "confine ls -- python3 -c 'import os; os.system(\"du -sh /tmp\")'",
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", payload])
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
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "confine ls -- python3 -c 'print(\"py-ran\")'"])
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
    if !std::path::Path::new("/usr/bin/sandbox-exec").exists() {
        return;
    }
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // no python3 to drive the payload
    }
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
        "confine read-only -- python3 -c 'import os; os.remove(\"{}\")'",
        victim.display()
    ));
    assert!(victim.exists(), "read-only failed to block os.remove");

    // A write outside scratch is denied.
    let outside = dir.join("new.txt");
    let _ = run(&format!(
        "confine read-only -- python3 -c 'open(\"{}\",\"w\").write(\"x\")'",
        outside.display()
    ));
    assert!(
        !outside.exists(),
        "read-only failed to block out-of-scratch write"
    );

    // Scratch (TMPDIR) IS writable, so tools that need temp still work.
    let ok = run("confine read-only -- python3 -c 'import os; p=os.path.join(os.environ[\"TMPDIR\"],\"s\"); open(p,\"w\").write(\"1\"); print(\"SCRATCH_OK\")'");
    assert!(
        String::from_utf8_lossy(&ok.stdout).contains("SCRATCH_OK"),
        "scratch should be writable: {}",
        String::from_utf8_lossy(&ok.stderr)
    );

    // Network is denied.
    let net = run("confine read-only -- python3 -c 'import socket; socket.create_connection((\"1.1.1.1\",80),2); print(\"CONNECTED\")'");
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
    std::fs::write(
        &src,
        r#"#include <unistd.h>
int main(void){char*a[]={"/bin/bash","-c","echo DEEP-TEST-HIT",0};execv("/bin/bash",a);return 1;}"#,
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
