use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn isolated_agsh(base: &Path, history_file: &Path) -> Command {
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("create isolated HOME");

    let mut command = Command::new(env!("CARGO_BIN_EXE_agsh"));
    command
        .current_dir(base)
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", base.join("xdg-config"))
        .env("XDG_DATA_HOME", base.join("xdg-data"))
        .env("XDG_STATE_HOME", base.join("xdg-state"))
        .env("AGSH_HISTORY_FILE", history_file)
        .env("AGSH_TRUST_FILE", base.join("trust.jsonl"))
        .env("AGSH_NORC", "1")
        .env_remove("AGSH_INTERCEPT")
        .env_remove("AGSH_OUTPUT_MODE")
        .env_remove("AGSH_RC")
        .env_remove("AGSH_SESSION");
    command
}

fn history_test_dir(name: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("agsh_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).expect("create history test directory");
    base
}

fn run_with_piped_stdin(mut command: Command, input: &str) -> std::process::Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agsh with piped stdin");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write piped script");
    child.wait_with_output().expect("wait for piped script")
}

fn run_isolated_piped_script(name: &str, input: &str) -> std::process::Output {
    let base = history_test_dir(name);
    let history = base.join("history.jsonl");
    let output = run_with_piped_stdin(isolated_agsh(&base, &history), input);
    let _ = std::fs::remove_dir_all(base);
    output
}

fn run_isolated_command(name: &str, source: &str) -> std::process::Output {
    let base = history_test_dir(name);
    let history = base.join("history.jsonl");
    let output = isolated_agsh(&base, &history)
        .args(["-c", source])
        .output()
        .expect("run isolated agsh command");
    let _ = std::fs::remove_dir_all(base);
    output
}

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

#[cfg(unix)]
#[test]
fn non_utf8_cli_argument_is_rejected_without_panicking() {
    use std::os::unix::ffi::OsStringExt;

    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .output()
        .expect("run agsh with non-UTF-8 argument");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not valid UTF-8"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn non_utf8_environment_value_is_preserved_for_external_children() {
    use std::os::unix::ffi::OsStringExt;

    let base = history_test_dir("non_utf8_environment");
    let history = base.join("history.jsonl");
    let opaque_value = std::ffi::OsString::from_vec(vec![b'a', 0xff, b'z']);
    let output = isolated_agsh(&base, &history)
        .args(["-c", "env"])
        .env("AGSH_TEST_OPAQUE", opaque_value)
        .output()
        .expect("run agsh with non-UTF-8 environment");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output
            .stdout
            .windows(b"AGSH_TEST_OPAQUE=a\xffz".len())
            .any(|window| window == b"AGSH_TEST_OPAQUE=a\xffz"),
        "opaque environment entry was not preserved: {:?}",
        output.stdout
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn command_mode_assigns_command_name_and_positionals() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "-c",
            "printf '%s|%s|%s\\n' \"$0\" \"$1\" \"$2\"",
            "custom-name",
            "one",
            "two",
        ])
        .output()
        .expect("run command mode with arguments");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"custom-name|one|two\n");
}

#[test]
fn command_mode_without_name_uses_agsh_as_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "printf '%s\\n' \"$0\""])
        .output()
        .expect("run command mode without arguments");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"agsh\n");
}

#[test]
fn script_mode_uses_script_path_as_zero() {
    let base = history_test_dir("script_zero");
    let script = base.join("script.agsh");
    std::fs::write(&script, "printf '%s|%s\\n' \"$0\" \"$1\"\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .arg(&script)
        .arg("first")
        .output()
        .expect("run script with argument");

    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        format!("{}|first\n", script.display()).as_bytes()
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn unbraced_multi_digit_positional_uses_one_digit() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "-c",
            "printf '%s|%s\\n' \"$10\" \"${10}\"",
            "name",
            "one",
            "two",
            "three",
            "four",
            "five",
            "six",
            "seven",
            "eight",
            "nine",
            "ten",
        ])
        .output()
        .expect("run multi-digit positional expansion");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"one0|ten\n");
}

#[test]
fn noninteractive_modes_do_not_create_persistent_history() {
    let base = history_test_dir("noninteractive_history_create");
    let history = base.join("history-dir/history.jsonl");
    let script = base.join("script.agsh");
    std::fs::write(&script, "echo script-mode\n").unwrap();

    let command_output = isolated_agsh(&base, &history)
        .args(["-c", "echo command-mode"])
        .output()
        .expect("run -c mode");
    assert!(command_output.status.success());
    assert!(!history.exists(), "-c created persistent history");

    let script_output = isolated_agsh(&base, &history)
        .arg(&script)
        .output()
        .expect("run script mode");
    assert!(script_output.status.success());
    assert!(!history.exists(), "script file created persistent history");

    let piped_output = run_with_piped_stdin(isolated_agsh(&base, &history), "echo piped-mode\n");
    assert!(piped_output.status.success());
    assert!(!history.exists(), "piped stdin created persistent history");
    assert!(
        !history.parent().expect("history parent").exists(),
        "noninteractive startup touched the persistent history directory"
    );

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn noninteractive_modes_do_not_read_persistent_history() {
    const SENTINEL: &str = "HISTORY_SENTINEL_FROM_DISK";
    let base = history_test_dir("noninteractive_history_read");
    let history = base.join("history-dir/history.jsonl");
    std::fs::create_dir_all(history.parent().unwrap()).unwrap();
    let mut persisted = format!(r#"{{"command":"{SENTINEL}","cwd":"/","started_at":1}}"#);
    persisted.push('\n');
    std::fs::write(&history, &persisted).unwrap();

    let command_output = isolated_agsh(&base, &history)
        .args(["-c", "history"])
        .output()
        .expect("run -c history");
    assert!(command_output.status.success());
    assert!(!String::from_utf8_lossy(&command_output.stdout).contains(SENTINEL));
    assert_eq!(std::fs::read_to_string(&history).unwrap(), persisted);

    let script = base.join("history.agsh");
    std::fs::write(&script, "history\n").unwrap();
    let script_output = isolated_agsh(&base, &history)
        .arg(&script)
        .output()
        .expect("run script history");
    assert!(script_output.status.success());
    assert!(!String::from_utf8_lossy(&script_output.stdout).contains(SENTINEL));
    assert_eq!(std::fs::read_to_string(&history).unwrap(), persisted);

    let piped_output = run_with_piped_stdin(isolated_agsh(&base, &history), "history\n");
    assert!(piped_output.status.success());
    assert!(!String::from_utf8_lossy(&piped_output.stdout).contains(SENTINEL));
    assert_eq!(std::fs::read_to_string(&history).unwrap(), persisted);

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn piped_stdin_script_emits_only_command_output() {
    let output = run_isolated_piped_script(
        "piped_script_output",
        "printf 'first\\n'\nprintf 'second\\n'\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"first\nsecond\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn piped_stdin_script_returns_the_last_command_status() {
    let output =
        run_isolated_piped_script("piped_script_status", "printf 'before-false\\n'\nfalse\n");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(output.stdout, b"before-false\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn piped_stdin_script_executes_multiline_constructs_as_one_source() {
    let output = run_isolated_piped_script(
        "piped_script_multiline",
        "if true\nthen\n  for item in one two\n  do\n    printf '<%s>\\n' \"$item\"\n  done\nfi\ncat <<EOF\nheredoc-ok\nEOF\n",
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"<one>\n<two>\nheredoc-ok\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn empty_piped_stdin_does_not_render_a_prompt() {
    let output = run_isolated_piped_script("piped_script_empty", "");

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn malformed_shell_input_returns_syntax_status_two() {
    let base = history_test_dir("syntax_status");
    let history = base.join("history.jsonl");
    for source in [
        "echo hi |",
        "echo hi 2>&",
        ")",
        "echo $(printf hi",
        "echo ${value",
    ] {
        let output = isolated_agsh(&base, &history)
            .args(["-c", source])
            .output()
            .expect("run malformed -c source");
        assert_eq!(
            output.status.code(),
            Some(2),
            "source={source:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stdout.is_empty(), "source={source:?}");
        assert!(!output.stderr.is_empty(), "source={source:?}");
    }

    let script = base.join("malformed.agsh");
    std::fs::write(&script, "echo hi |\n").unwrap();
    let file_output = isolated_agsh(&base, &history)
        .arg(&script)
        .output()
        .expect("run malformed script file");
    assert_eq!(file_output.status.code(), Some(2));

    let stdin_output = run_with_piped_stdin(isolated_agsh(&base, &history), "echo hi |\n");
    assert_eq!(stdin_output.status.code(), Some(2));

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn project_env_requires_trust_then_activates() {
    let base = history_test_dir("project_env_trust");
    let history = base.join("history.jsonl");
    std::fs::write(base.join(".env"), "MY_PROJECT_VAR=hello123\n").unwrap();
    let trust = base.join("trustfile");

    // Untrusted: the .env is NOT sourced.
    let out = isolated_agsh(&base, &history)
        .args([
            "-c",
            &format!("cd {}; echo v=$MY_PROJECT_VAR", base.display()),
        ])
        .env("AGSH_TRUST_FILE", &trust)
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "v=\n");

    // After trust, it activates within the session.
    let mut child = isolated_agsh(&base, &history)
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

    // The durable, versioned digest activates the same bytes in a fresh shell.
    let out = isolated_agsh(&base, &history)
        .args([
            "-c",
            &format!("cd {}; echo persisted=$MY_PROJECT_VAR", base.display()),
        ])
        .env("AGSH_TRUST_FILE", &trust)
        .output()
        .expect("run agsh with persisted project trust");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "persisted=hello123\n");
    assert!(std::fs::read_to_string(&trust)
        .unwrap()
        .starts_with("sha256:"));

    std::fs::write(base.join(".env"), "MY_PROJECT_VAR=changed\n").unwrap();
    let out = isolated_agsh(&base, &history)
        .args([
            "-c",
            &format!("cd {}; echo changed=$MY_PROJECT_VAR", base.display()),
        ])
        .env("AGSH_TRUST_FILE", &trust)
        .output()
        .expect("run agsh after trusted .env changed");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "changed=\n");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn project_env_duplicate_keys_restore_unexported_shell_binding() {
    let base = history_test_dir("project_env_restore");
    let history = base.join("history.jsonl");
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".env"), "FOO=first\nFOO=second\n").unwrap();

    let script = format!(
        "FOO=outer\ncd {}\nagtrust\nprintf 'inside=<%s>|child=<%s>\\n' \"$FOO\" \"$(sh -c 'printf %s \"${{FOO-unset}}\"')\"\ncd ..\nprintf 'outside=<%s>|child=<%s>\\n' \"$FOO\" \"$(sh -c 'printf %s \"${{FOO-unset}}\"')\"\n",
        project.display()
    );
    let output = run_with_piped_stdin(isolated_agsh(&base, &history), &script);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("activated .env (1 variables)"), "{stdout}");
    assert!(
        stdout.contains("inside=<second>|child=<second>"),
        "{stdout}"
    );
    assert!(stdout.contains("outside=<outer>|child=<unset>"), "{stdout}");

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn project_env_restores_function_local_then_outer_binding() {
    let base = history_test_dir("project_env_local_restore");
    let history = base.join("history.jsonl");
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".env"), "FOO=project\n").unwrap();
    let script = format!(
        "FOO=outer\nf() {{ local FOO=local; cd {}; agtrust >/dev/null; printf 'project=<%s>\\n' \"$FOO\"; cd ..; printf 'local=<%s>\\n' \"$FOO\"; }}\nf\nprintf 'outer=<%s>\\n' \"$FOO\"\n",
        project.display()
    );

    let output = run_with_piped_stdin(isolated_agsh(&base, &history), &script);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "project=<project>\nlocal=<local>\nouter=<outer>\n"
    );

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn project_env_is_not_activated_when_trust_cannot_be_persisted() {
    let base = history_test_dir("project_env_trust_failure");
    let history = base.join("history.jsonl");
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".env"), "FAILED_TRUST=secret\n").unwrap();
    let non_directory = base.join("not-a-directory");
    std::fs::write(&non_directory, b"blocker").unwrap();
    let source = format!(
        "cd {}; agtrust; code=$?; printf 'code=%s value=<%s>\\n' \"$code\" \"$FAILED_TRUST\"",
        project.display()
    );

    let output = isolated_agsh(&base, &history)
        .env("AGSH_TRUST_FILE", non_directory.join("trusted_env"))
        .args(["-c", &source])
        .output()
        .expect("run agsh with an unwritable trust path");

    assert_eq!(String::from_utf8_lossy(&output.stdout), "code=1 value=<>\n");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("trust:"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = std::fs::remove_dir_all(base);
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
fn normal_commands_do_not_receive_advisory_risk_stderr() {
    let target = std::env::temp_dir().join(format!("agsh-risk-nonexistent-{}", std::process::id()));
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", &format!("rm -rf -- {}", target.display())])
        .output()
        .expect("run agsh");

    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stderr.is_empty(),
        "stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    let empty_path = std::env::temp_dir().join(format!("agsh_empty_path_{}", std::process::id()));
    std::fs::create_dir_all(&empty_path).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "rg pattern"])
        .env("PATH", &empty_path)
        .output()
        .expect("run agsh");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Install:"), "stderr: {stderr}");
    assert!(stderr.contains("ripgrep"), "stderr: {stderr}");
    let _ = std::fs::remove_dir_all(&empty_path);
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
fn semantic_observation_has_stable_complete_trace_status_and_backing_refs() {
    let base = history_test_dir("semantic_complete_trace_schema");
    let history = base.join("history.jsonl");
    let traces = base.join("traces");
    let output = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", "seq 1 200"])
        .env("AGSH_TRACE_DIR", &traces)
        .output()
        .expect("run semantic trace schema command");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    let display = String::from_utf8(output.stdout).unwrap();
    assert!(display.contains("\"raw_trace\": {"), "{display}");
    assert!(display.contains("\"complete\": true"), "{display}");
    assert!(display.contains("\"stdout\": \"complete\""), "{display}");
    assert!(display.contains("\"stderr\": \"complete\""), "{display}");
    assert!(display.contains("\"limit_bytes\": 104857600"), "{display}");
    assert!(display.contains("\"raw_stdout\":"), "{display}");
    assert!(display.contains("\"raw_stderr\":"), "{display}");
    assert!(display.contains(traces.to_str().unwrap()), "{display}");
    assert_eq!(std::fs::read_dir(&traces).unwrap().count(), 2);
    let _ = std::fs::remove_dir_all(base);
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
fn semantic_mode_redacts_sensitive_environment_values_and_argv() {
    let secret = "supersecretvalue";
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "--output",
            "semantic",
            "-c",
            "printf '%s\\n' \"$MY_PRIVATE_TOKEN\"",
        ])
        .env("MY_PRIVATE_TOKEN", secret)
        .output()
        .expect("run agsh");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[REDACTED]"), "stdout={stdout:?}");
    assert!(!stdout.contains(secret), "secret leaked: {stdout:?}");
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
    let output = run_isolated_command("background_job_status", "sh -c 'exit 7' & wait %1");
    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn wait_without_operands_returns_zero_after_waiting_for_all_jobs() {
    let output = run_isolated_command("background_wait_all", "sh -c 'exit 7' & wait");
    assert_eq!(output.status.code(), Some(0));
}

#[test]
fn noninteractive_background_job_does_not_print_a_notice() {
    let output = run_isolated_command("background_no_notice", "sleep 0.2 & wait");
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
}

#[test]
fn trailing_background_item_returns_before_the_job_in_raw_and_semantic_modes() {
    let mut markers = Vec::new();
    for mode in ["raw", "semantic"] {
        let base = history_test_dir(&format!("trailing_background_{mode}"));
        let history = base.join("history.jsonl");
        let marker = base.join("finished");
        let source = format!(
            "sh -c 'sleep 2; printf survived || exit $?; touch {}' &",
            marker.display()
        );
        let mut command = isolated_agsh(&base, &history);
        if mode == "semantic" {
            command.args(["--output", "semantic"]);
        }

        let started = std::time::Instant::now();
        let status = command
            .args(["-c", &source])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| panic!("run trailing {mode} background item: {error}"));

        assert!(status.success());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(1500),
            "{mode} mode waited for the asynchronous payload"
        );
        markers.push((base, marker));
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    while markers.iter().any(|(_, marker)| !marker.exists()) && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for (base, marker) in markers {
        assert!(
            marker.exists(),
            "background payload did not finish: {marker:?}"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}

#[test]
fn semantic_case_fallthrough_is_not_downgraded_as_async_output() {
    for (name, source) in [
        (
            "case_semicolon_ampersand",
            "case x in x) printf first ;& y) printf second ;; esac",
        ),
        (
            "case_double_semicolon_ampersand",
            "case x in x) printf first ;;& x) printf second ;; esac",
        ),
    ] {
        let base = history_test_dir(name);
        let history = base.join("history.jsonl");
        let output = isolated_agsh(&base, &history)
            .args(["--output", "semantic", "-c", source])
            .output()
            .expect("run semantic case fallthrough");

        assert!(output.status.success(), "stderr={:?}", output.stderr);
        assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
        let display = String::from_utf8(output.stdout).unwrap();
        assert!(display.trim_start().starts_with('{'), "{display:?}");
        assert!(display.trim_end().ends_with('}'), "{display:?}");
        assert_eq!(display.matches("\"command\"").count(), 1, "{display}");
        assert!(display.contains("firstsecond"), "{display}");
        std::fs::remove_dir_all(base).unwrap();
    }
}

#[test]
fn background_payload_stdin_is_dev_null_after_state_handoff() {
    let output = run_isolated_command(
        "background_stdin_null",
        "{ read BG_INPUT; printf 'status=%s value=<%s>\\n' \"$?\" \"$BG_INPUT\"; } & wait",
    );

    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "status=1 value=<>\n"
    );
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
}

#[test]
fn background_job_inherits_compound_output_routing() {
    let base = history_test_dir("background_compound_output");
    let history = base.join("history.jsonl");
    let output_path = base.join("background.out");
    let source = format!(
        "{{ sh -c 'printf o; printf e >&2' & wait; }} >'{}' 2>&1",
        output_path.display()
    );
    let output = isolated_agsh(&base, &history)
        .args(["-c", &source])
        .output()
        .expect("run background command under compound redirection");

    assert!(
        output.status.success(),
        "stderr={:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty(), "stdout={:?}", output.stdout);
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
    assert_eq!(std::fs::read(&output_path).unwrap(), b"oe");

    std::fs::remove_dir_all(base).unwrap();
}

#[test]
fn background_job_stdout_flows_through_shell_pipeline_stage() {
    let output = run_isolated_command(
        "background_shell_pipeline_stdout",
        "{ sh -c 'printf bg' & wait; } | sed 's/bg/through/'",
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"through");
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
}

#[test]
fn background_job_fd_dup_stays_in_pipeline_and_child_stays_raw() {
    let source = "{ sh -c 'printf o; printf e >&2' & wait; } 2>&1 | tr a-z A-Z";
    let raw = run_isolated_command("background_fd_dup_raw", source);
    assert!(raw.status.success(), "stderr={:?}", raw.stderr);
    assert_eq!(raw.stdout, b"OE");
    assert!(raw.stderr.is_empty(), "stderr={:?}", raw.stderr);

    let base = history_test_dir("background_fd_dup_semantic");
    let history = base.join("history.jsonl");
    let semantic = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", source])
        .output()
        .expect("run semantic background pipeline");
    assert!(semantic.status.success(), "stderr={:?}", semantic.stderr);
    assert!(semantic.stderr.is_empty(), "stderr={:?}", semantic.stderr);
    assert_eq!(semantic.stdout, raw.stdout);
    assert_eq!(semantic.stderr, raw.stderr);
    let _ = std::fs::remove_dir_all(base);

    let base = history_test_dir("background_standalone_semantic");
    let history = base.join("history.jsonl");
    let marker = base.join("payload-ran");
    let standalone_source = format!(
        "sh -c 'printf raw-background-output-marker; touch {}' & wait",
        marker.display()
    );
    let standalone = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", &standalone_source])
        .output()
        .expect("run standalone semantic background job");
    assert!(
        standalone.status.success(),
        "stderr={:?}",
        standalone.stderr
    );
    assert!(marker.exists(), "captured background payload did not run");
    assert!(
        standalone.stderr.is_empty(),
        "stderr={:?}",
        standalone.stderr
    );
    assert_eq!(standalone.stdout, b"raw-background-output-marker");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn async_graph_falls_back_to_raw_without_changing_payload_or_exit_status() {
    let source = "sh -c 'printf bgout; printf bgerr >&2; exit 7' & wait $!";

    let raw = run_isolated_command("background_wait_status_raw", source);
    assert_eq!(raw.status.code(), Some(7));
    assert_eq!(raw.stdout, b"bgout");
    assert_eq!(raw.stderr, b"bgerr");

    let base = history_test_dir("background_wait_status_semantic");
    let history = base.join("history.jsonl");
    let semantic = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", source])
        .output()
        .expect("run captured background status");
    assert_eq!(semantic.status.code(), Some(7));
    assert_eq!(semantic.stdout, raw.stdout);
    assert_eq!(semantic.stderr, raw.stderr);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn async_raw_fallback_preserves_foreground_background_chronology() {
    let base = history_test_dir("background_chronology");
    let ready = base.join("ready");
    let source = format!(
        "sh -c 'while [ ! -f \"{}\" ]; do sleep 0.01; done; printf A' & printf B; touch '{}'; wait",
        ready.display(),
        ready.display()
    );
    let raw = run_isolated_command("background_chronology_raw", &source);
    std::fs::remove_file(&ready).unwrap();
    let history = base.join("history.jsonl");
    let semantic = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", &source])
        .output()
        .unwrap();
    assert_eq!(raw.status.code(), semantic.status.code());
    assert_eq!(raw.stdout, b"BA");
    assert_eq!(semantic.stdout, raw.stdout);
    assert_eq!(semantic.stderr, raw.stderr);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn wait_redirection_does_not_reroute_background_output() {
    let base = history_test_dir("background_wait_redirection");
    let redirected = base.join("wait.out");
    let source = format!("sh -c 'printf bg' & wait >'{}'", redirected.display());
    let history = base.join("history.jsonl");
    let output = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", &source])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"bg");
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read(&redirected).unwrap(), b"");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn semantic_capture_detaches_retained_streams_without_killing_descendant() {
    let base = history_test_dir("semantic_detached_descendant");
    let history = base.join("history.jsonl");
    let marker = base.join("survived");
    let source = format!(
        "sh -c '(sleep 2; printf late-out || exit 98; printf late-err >&2 || exit 99; touch {}) & exit 23'",
        marker.display()
    );
    let started = std::time::Instant::now();
    let output = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", &source])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(23));
    assert!(started.elapsed() < std::time::Duration::from_millis(1500));
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
    let observation = String::from_utf8(output.stdout).unwrap();
    assert!(observation.contains("\"exit_code\": 23"), "{observation}");
    assert!(observation.contains("\"complete\": false"), "{observation}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    while !marker.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        marker.exists(),
        "detached descendant lost its captured descriptors"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn dynamic_capture_paths_detach_opaque_descendant_streams() {
    let base = history_test_dir("dynamic_detached_descendants");
    let script = base.join("spawn.agsh");
    let markers = [
        base.join("eval"),
        base.join("source"),
        base.join("function"),
    ];
    std::fs::write(
        &script,
        format!(
            "sh -c '(sleep 2; printf source-out; printf source-err >&2; touch {}) &'\n",
            markers[1].display()
        ),
    )
    .unwrap();
    let sources = [
        format!(
            "eval \"sh -c '(sleep 2; printf eval-out; printf eval-err >&2; touch {}) &'\"",
            markers[0].display()
        ),
        format!("source '{}'", script.display()),
        format!(
            "f() {{ sh -c '(sleep 2; printf function-out; printf function-err >&2; touch {}) &'; }}; f",
            markers[2].display()
        ),
    ];

    for (index, source) in sources.iter().enumerate() {
        let history = base.join(format!("history-{index}.jsonl"));
        let started = std::time::Instant::now();
        let output = isolated_agsh(&base, &history)
            .args(["--output", "semantic", "-c", source])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "source={source:?} stderr={:?}",
            output.stderr
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(1500));
        assert!(
            output.stderr.is_empty(),
            "source={source:?} stderr={:?}",
            output.stderr
        );
        assert!(
            output.stdout.starts_with(b"{"),
            "source={source:?} stdout={:?}",
            output.stdout
        );
    }

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    while markers.iter().any(|marker| !marker.exists()) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    for marker in &markers {
        assert!(
            marker.exists(),
            "dynamic descendant did not survive: {marker:?}"
        );
    }
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn substitutions_keep_private_stdout_under_compound_redirection() {
    let base = history_test_dir("substitution_private_routing");
    let history = base.join("history.jsonl");
    let command_path = base.join("command.out");
    let process_path = base.join("process.out");
    let source = format!(
        "{{ printf before; value=$(printf sub); printf 'x=%s' \"$value\"; }} >'{}'; \
         {{ printf before; cat <(printf sub) | tr a-z A-Z; printf after; }} >'{}'",
        command_path.display(),
        process_path.display()
    );

    let output = isolated_agsh(&base, &history)
        .args(["-c", &source])
        .output()
        .expect("run substitutions under compound redirection");

    assert!(output.status.success(), "stderr={:?}", output.stderr);
    assert!(output.stdout.is_empty(), "stdout={:?}", output.stdout);
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
    assert_eq!(std::fs::read(&command_path).unwrap(), b"beforex=sub");
    assert_eq!(std::fs::read(&process_path).unwrap(), b"beforeSUBafter");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn substitutions_preserve_left_to_right_fd_snapshot_order() {
    let base = history_test_dir("substitution_fd_snapshot");
    let history = base.join("history.jsonl");
    let command_path = base.join("command.out");
    let process_path = base.join("process.out");
    let source = format!(
        "{{ value=$(printf sub; printf err >&2); printf 'x=%s' \"$value\"; }} \
         2>&1 >'{}'; \
         {{ cat <(printf sub; printf err >&2); printf after; }} 2>&1 >'{}'",
        command_path.display(),
        process_path.display()
    );

    let output = isolated_agsh(&base, &history)
        .args(["-c", &source])
        .output()
        .expect("run substitutions after ordered fd duplication");

    assert!(output.status.success(), "stderr={:?}", output.stderr);
    assert_eq!(output.stdout, b"errerr");
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
    assert_eq!(std::fs::read(&command_path).unwrap(), b"x=sub");
    assert_eq!(std::fs::read(&process_path).unwrap(), b"subafter");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn substitution_fd_snapshots_stay_inside_capture_modes() {
    let cases = [
        (
            "command",
            "value=$(printf sub; printf err >&2); printf 'x=%s' \"$value\"",
            "errx=sub",
            "x=sub",
        ),
        (
            "process",
            "cat <(printf sub; printf err >&2); printf after",
            "errsubafter",
            "subafter",
        ),
    ];
    for mode in ["raw", "compact", "semantic"] {
        for (kind, body, merged, redirected) in cases {
            for (order, redirection, expected_display, expected_file) in [
                ("dup-only", "2>&1".to_string(), merged, None),
                ("dup-before-file", String::new(), "err", Some(redirected)),
            ] {
                let base =
                    history_test_dir(&format!("substitution_capture_fd_{mode}_{kind}_{order}"));
                let history = base.join("history.jsonl");
                let file = base.join("routed.out");
                let redirection = if order == "dup-before-file" {
                    format!("2>&1 >'{}'", file.display())
                } else {
                    redirection
                };
                let source = format!("{{ {body}; }} {redirection}");
                let output = isolated_agsh(&base, &history)
                    .args(["--output", mode, "-c", &source])
                    .output()
                    .unwrap_or_else(|error| {
                        panic!("run {mode} {kind} {order} substitution: {error}")
                    });

                assert!(output.status.success(), "stderr={:?}", output.stderr);
                assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
                if mode == "semantic" {
                    let display = String::from_utf8(output.stdout).unwrap();
                    assert!(display.trim_start().starts_with('{'), "display={display:?}");
                    assert!(
                        display.contains(&format!("\"body\": [\n    \"{expected_display}\"")),
                        "{mode} {kind} {order}: {display}"
                    );
                } else {
                    assert_eq!(
                        output.stdout,
                        expected_display.as_bytes(),
                        "{mode} {kind} {order}"
                    );
                }
                if let Some(expected_file) = expected_file {
                    assert_eq!(std::fs::read(&file).unwrap(), expected_file.as_bytes());
                }
                let _ = std::fs::remove_dir_all(base);
            }
        }
    }
}

#[test]
fn background_stdout_is_captured_inside_substitutions() {
    let output = run_isolated_command(
        "substitution_background_routing",
        "value=$(printf bg & wait); printf 'command=<%s>\\n' \"$value\"; \
         cat <(printf bg & wait) | tr a-z A-Z",
    );

    assert!(output.status.success(), "stderr={:?}", output.stderr);
    assert_eq!(output.stdout, b"command=<bg>\nBG");
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
}

#[cfg(unix)]
#[test]
fn compound_redirection_routes_synthesized_diagnostics() {
    use std::os::unix::fs::PermissionsExt;

    let base = history_test_dir("compound_synthetic_diagnostics");
    let history = base.join("history.jsonl");
    let brace_path = base.join("brace.err");
    let function_path = base.join("function.err");
    let missing_command_path = base.join("missing-command.err");
    let spawn_path = base.join("spawn.err");
    let missing = base.join("missing-input");
    let bad = base.join("bad-interpreter");
    std::fs::write(&bad, "#!/definitely/missing/agsh-interpreter\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = format!(
        "{{ cat <'{}'; }} >'{}' 2>&1; \
         f() {{ cat <'{}'; }}; f >'{}' 2>&1; \
         {{ definitely_missing_agsh_command; }} >'{}' 2>&1; \
         {{ '{}'; }} >'{}' 2>&1",
        missing.display(),
        brace_path.display(),
        missing.display(),
        function_path.display(),
        missing_command_path.display(),
        bad.display(),
        spawn_path.display()
    );

    let output = isolated_agsh(&base, &history)
        .args(["-c", &source])
        .output()
        .expect("run synthesized diagnostics under compound redirection");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "stdout={:?}", output.stdout);
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
    for path in [&brace_path, &function_path] {
        let diagnostic = String::from_utf8(std::fs::read(path).unwrap()).unwrap();
        assert!(diagnostic.contains("missing-input"), "{diagnostic:?}");
        assert!(diagnostic.contains("No such file"), "{diagnostic:?}");
    }
    assert!(
        String::from_utf8(std::fs::read(&missing_command_path).unwrap())
            .unwrap()
            .contains("command not found")
    );
    assert!(String::from_utf8(std::fs::read(&spawn_path).unwrap())
        .unwrap()
        .contains("No such file"));
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn capture_modes_do_not_double_route_compound_streams() {
    let producers = [
        ("external", "", "sh -c 'printf out; printf err >&2'"),
        (
            "function",
            "f() { sh -c 'printf out; printf err >&2'; };",
            "f",
        ),
        ("subshell", "", "(sh -c 'printf out; printf err >&2')"),
        (
            "control",
            "",
            "if true; then sh -c 'printf out; printf err >&2'; fi",
        ),
    ];
    for mode in ["raw", "semantic"] {
        for (producer, setup, body) in producers {
            for (order, redirection, expected_file, expected_logical_stderr) in [
                (
                    "stdout-first",
                    "1>&2 2>",
                    b"err".as_slice(),
                    b"out".as_slice(),
                ),
                (
                    "stderr-first",
                    "2>FILE 1>&2",
                    b"outerr".as_slice(),
                    b"".as_slice(),
                ),
            ] {
                let base = history_test_dir(&format!(
                    "compound_no_double_route_{mode}_{producer}_{order}"
                ));
                let history = base.join("history.jsonl");
                let routed = base.join("routed.err");
                let redirection = if redirection == "1>&2 2>" {
                    format!("1>&2 2>'{}'", routed.display())
                } else {
                    format!("2>'{}' 1>&2", routed.display())
                };
                let source = format!("{setup} {{ {body}; }} {redirection}");
                let output = isolated_agsh(&base, &history)
                    .args(["--output", mode, "-c", &source])
                    .output()
                    .unwrap_or_else(|error| {
                        panic!("run {mode} {producer} {order} routing: {error}")
                    });

                assert!(
                    output.status.success(),
                    "{mode} {producer} {order}: stderr={:?}",
                    output.stderr
                );
                assert_eq!(
                    std::fs::read(&routed).unwrap(),
                    expected_file,
                    "{mode} {producer} {order}"
                );
                if mode == "raw" {
                    assert!(output.stdout.is_empty(), "stdout={:?}", output.stdout);
                    assert_eq!(output.stderr, expected_logical_stderr, "{producer} {order}");
                } else {
                    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
                    let display = String::from_utf8(output.stdout).unwrap();
                    assert!(display.trim_start().starts_with('{'), "display={display:?}");
                    let has_routed_out = display.contains("\"body\": [\n    \"out\"");
                    assert_eq!(
                        has_routed_out,
                        !expected_logical_stderr.is_empty(),
                        "{mode} {producer} {order}: {display}"
                    );
                }
                let _ = std::fs::remove_dir_all(base);
            }
        }
    }
}

#[test]
fn control_structures_apply_output_routing_live() {
    let base = history_test_dir("control_structure_live_routing");
    let history = base.join("history.jsonl");
    let emitter = "sh -c 'printf o1; printf e1 >&2; printf o2; printf e2 >&2'";
    let cases = [
        ("if", format!("if true; then {emitter}; fi")),
        ("for", format!("for item in one; do {emitter}; done")),
        ("while", format!("while true; do {emitter}; break; done")),
        ("until", format!("until false; do {emitter}; break; done")),
        ("case", format!("case value in value) {emitter};; esac")),
    ];

    for mode in ["raw", "compact"] {
        for (name, command) in &cases {
            let path = base.join(format!("{mode}-{name}.out"));
            let source = format!("{command} >'{}' 2>&1", path.display());
            let output = isolated_agsh(&base, &history)
                .args(["--output", mode, "-c", &source])
                .output()
                .unwrap_or_else(|error| panic!("run {mode} {name}: {error}"));
            assert!(
                output.status.success(),
                "{mode} {name}: stderr={:?}",
                output.stderr
            );
            assert!(
                output.stderr.is_empty(),
                "{mode} {name}: stderr={:?}",
                output.stderr
            );
            assert_eq!(std::fs::read(path).unwrap(), b"o1e1o2e2", "{mode} {name}");
        }
    }

    let select_path = base.join("select.out");
    let select_source = format!(
        "printf '1\\n' | select item in one; do {emitter}; break; done >'{}' 2>&1",
        select_path.display()
    );
    for mode in ["raw", "compact"] {
        let output = isolated_agsh(&base, &history)
            .args(["--output", mode, "-c", &select_source])
            .output()
            .expect("run select with live routing");
        assert!(
            output.status.success(),
            "{mode}: stderr={:?}",
            output.stderr
        );
        let select = std::fs::read(&select_path).unwrap();
        assert!(
            select
                .windows(b"o1e1o2e2".len())
                .any(|window| window == b"o1e1o2e2"),
            "{mode} select output={select:?}"
        );
    }
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn xtrace_uses_enclosing_fds_before_live_external_output() {
    let base = history_test_dir("xtrace_live_fd_order");
    let history = base.join("history.jsonl");
    let trace = b"+ sh -c 'printf out; printf err >&2'\n";

    let simple_file = base.join("simple.out");
    let simple = format!(
        "set -x; sh -c 'printf out; printf err >&2' 2>&1 >'{}'",
        simple_file.display()
    );
    let output = isolated_agsh(&base, &history)
        .args(["-c", &simple])
        .output()
        .expect("run xtrace with simple-command redirections");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"err");
    assert_eq!(output.stderr, trace);
    assert_eq!(std::fs::read(&simple_file).unwrap(), b"out");

    let snapshot_file = base.join("snapshot.out");
    let snapshot = format!(
        "set -x; {{ sh -c 'printf out; printf err >&2'; }} 2>&1 >'{}'",
        snapshot_file.display()
    );
    let output = isolated_agsh(&base, &history)
        .args(["-c", &snapshot])
        .output()
        .expect("run xtrace under saved stdout");
    let mut traced_then_err = trace.to_vec();
    traced_then_err.extend_from_slice(b"err");
    assert!(output.status.success());
    assert_eq!(output.stdout, traced_then_err);
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read(&snapshot_file).unwrap(), b"out");

    let stderr_file = base.join("compound.err");
    let stderr_routed = format!(
        "set -x; {{ sh -c 'printf out; printf err >&2'; }} 1>&2 2>'{}'",
        stderr_file.display()
    );
    let output = isolated_agsh(&base, &history)
        .args(["-c", &stderr_routed])
        .output()
        .expect("run xtrace under redirected compound stderr");
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"out");
    assert_eq!(std::fs::read(&stderr_file).unwrap(), traced_then_err);

    let inner_stdout = base.join("inner.out");
    let inner_stderr = base.join("inner.err");
    let inner_redirect = format!(
        "set -x; {{ sh -c 'printf out; printf err >&2' 2>'{}'; }} 2>&1 >'{}'",
        inner_stderr.display(),
        inner_stdout.display()
    );
    let output = isolated_agsh(&base, &history)
        .args(["-c", &inner_redirect])
        .output()
        .expect("run xtrace outside a simple command's stderr redirection");
    assert!(output.status.success());
    assert_eq!(output.stdout, trace);
    assert!(output.stderr.is_empty());
    assert_eq!(std::fs::read(&inner_stdout).unwrap(), b"out");
    assert_eq!(std::fs::read(&inner_stderr).unwrap(), b"err");

    let capture_source = "set -x; { sh -c 'printf out'; } 2>&1";
    let clean = isolated_agsh(&base, &history)
        .args(["--output", "clean", "-c", capture_source])
        .output()
        .expect("run captured xtrace ordering");
    assert!(clean.status.success());
    assert_eq!(clean.stdout, b"+ sh -c 'printf out'\nout");
    assert!(clean.stderr.is_empty());

    let semantic = isolated_agsh(&base, &history)
        .args(["--output", "semantic", "-c", capture_source])
        .output()
        .expect("run semantic xtrace ordering");
    assert!(semantic.status.success());
    assert!(semantic.stderr.is_empty());
    let display = String::from_utf8(semantic.stdout).unwrap();
    assert!(
        display.contains("\"body\": [\n    \"+ sh -c 'printf out'\",\n    \"out\""),
        "display={display:?}"
    );

    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn xtrace_function_call_precedes_function_redirections() {
    let base = history_test_dir("xtrace_function_fd_order");
    let history = base.join("history.jsonl");
    let stdout_file = base.join("function.out");
    let source = format!(
        "set -x; f() {{ sh -c 'printf out; printf err >&2'; }}; f 2>&1 >'{}'",
        stdout_file.display()
    );

    let output = isolated_agsh(&base, &history)
        .args(["-c", &source])
        .output()
        .expect("run xtrace through a function fd snapshot");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"+ sh -c 'printf out; printf err >&2'\nerr");
    assert_eq!(output.stderr, b"+ f\n");
    assert_eq!(std::fs::read(&stdout_file).unwrap(), b"out");

    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn raw_compound_fd_routing_does_not_capture_large_pipeline() {
    let base = history_test_dir("raw_compound_large_pipeline");
    let history = base.join("history.jsonl");
    for (name, source) in [
        ("default", "{ head -c 68157440 /dev/zero | cat; }"),
        ("merged", "{ head -c 68157440 /dev/zero | cat; } 2>&1"),
    ] {
        let output = isolated_agsh(&base, &history)
            .args(["-c", source])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .unwrap_or_else(|error| panic!("run {name} large raw pipeline: {error}"));

        assert!(
            output.status.success(),
            "{name}: stderr={:?}",
            output.stderr
        );
        assert!(
            output.stderr.is_empty(),
            "{name}: stderr={:?}",
            output.stderr
        );
    }
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn mixed_pipeline_spawn_failure_stops_running_shell_stage() {
    use std::os::unix::fs::PermissionsExt;

    let base = history_test_dir("mixed_pipeline_spawn_cleanup");
    let history = base.join("history.jsonl");
    let bad = base.join("bad-interpreter");
    std::fs::write(&bad, "#!/definitely/missing/agsh-interpreter\n").unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (name, wrapper) in [("default", "%s"), ("live", "{ %s; } 2>&1")] {
        let pipeline = format!("{{ sleep 5; }} | '{}'", bad.display());
        let command = wrapper.replace("%s", &pipeline);
        let source = format!("{command}; printf done");

        let started = std::time::Instant::now();
        let output = isolated_agsh(&base, &history)
            .args(["-c", &source])
            .output()
            .unwrap_or_else(|error| panic!("run {name} mixed pipeline: {error}"));

        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{name}: cleanup waited for the silent shell-stage child"
        );
        assert!(
            output.status.success(),
            "{name}: stderr={:?}",
            output.stderr
        );
        assert!(
            output.stdout.ends_with(b"done"),
            "{name}: stdout={:?}",
            output.stdout
        );
    }
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn jobs_lists_running_background_job_then_kill() {
    let output = run_isolated_command(
        "background_jobs_listing",
        "sleep 3 & jobs; kill %1; wait; echo done",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[1]"), "jobs output: {stdout}");
    assert!(stdout.contains("Running"), "jobs output: {stdout}");
    assert!(stdout.contains("done"), "stdout: {stdout}");
}

#[test]
fn background_isolates_variable_changes() {
    // A backgrounded command runs in its own process; it cannot mutate the
    // parent shell's variables.
    let output = run_isolated_command(
        "background_variable_isolation",
        "X=1; X=2 & wait; echo \"X=$X\"",
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "X=1\n");
}

#[test]
fn background_subshell_inherits_shell_locals_functions_and_options() {
    let output = run_isolated_command(
        "background_locals_functions_options",
        "X=local; f() { printf 'f:%s\\n' \"$X\"; }; set -o pipefail; \
         f & wait %1; false | true & wait %2; printf 'status:%s\\n' \"$?\"",
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "f:local\nstatus:1\n"
    );
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
}

#[test]
fn background_subshell_inherits_positionals_arrays_and_attributes_but_isolated() {
    let base = history_test_dir("background_state_inheritance");
    let history = base.join("history.jsonl");
    let source = "set -- first 'two words'; \
        declare -a arr=(zero one); declare -A map=([key]=value); \
        declare -i num=7; readonly lock=fixed; export EXPORTED=outer; LOCAL=local; \
        { arr[0]=child; map[key]=child-map; num=2+3; lock=changed 2>/dev/null; \
          printf 'child:%s|%s|%s|%s|%s|%s|%s|' \"$0\" \"$1\" \"$2\" \"${arr[0]}\" \
            \"${map[key]}\" \"$num\" \"$lock\"; \
          sh -c 'printf \"%s|%s\\n\" \"$EXPORTED\" \"${LOCAL-unset}\"'; } & \
        wait; printf 'parent:%s|%s|%s|%s|%s\\n' \"${arr[0]}\" \"${map[key]}\" \
          \"$num\" \"$lock\" \"$LOCAL\"";

    let output = isolated_agsh(&base, &history)
        .args(["-c", source, "custom-zero"])
        .output()
        .expect("run background state inheritance case");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("child:custom-zero|first|two words|child|child-map|5|fixed|"),
        "{stdout}"
    );
    assert!(stdout.contains("outer|unset\n"), "{stdout}");
    assert!(
        stdout.ends_with("parent:zero|value|7|fixed|local\n"),
        "{stdout}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn background_subshell_restores_outer_value_when_leaving_active_project_env() {
    let base = history_test_dir("background_active_project_env");
    let project = base.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join(".env"), "FOO=project\n").unwrap();
    let history = base.join("history.jsonl");
    let output = isolated_agsh(&base, &history)
        .args([
            "-c",
            "FOO=outer; cd project; agtrust >/dev/null; unset AGSH_TRUST_FILE; \
             { printf 'inside=<%s>\\n' \"$FOO\"; cd ..; \
               printf 'child=<%s>\\n' \"$FOO\"; FOO=child; } & wait; \
             printf 'parent=<%s>\\n' \"$FOO\"",
        ])
        .output()
        .expect("run active project environment background case");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "inside=<project>\nchild=<outer>\nparent=<project>\n"
    );
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn wait_accepts_last_background_pid_and_reports_signal_status() {
    let output = run_isolated_command(
        "background_pid_signal_status",
        "sleep 10 & p=$!; kill -TERM \"$p\"; wait \"$p\"; printf '%s\\n' \"$?\"",
    );

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "143\n");
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
}

#[test]
fn completed_background_status_remains_waitable_by_pid() {
    let base = history_test_dir("background_completed_wait");
    let history = base.join("history.jsonl");
    let output = isolated_agsh(&base, &history)
        .args([
            "-c",
            "sh -c 'exit 9' & p=$!; sleep 0.1; jobs >/dev/null; wait \"$p\"; printf '%s\\n' \"$?\"",
        ])
        .output()
        .expect("wait for an already completed background job");

    assert_eq!(String::from_utf8_lossy(&output.stdout), "9\n");
    assert!(output.stderr.is_empty(), "stderr={:?}", output.stderr);
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn obsolete_background_state_path_flag_cannot_delete_a_file() {
    use std::os::unix::fs::PermissionsExt;

    let base = history_test_dir("background_state_path_rejected");
    let history = base.join("history.jsonl");
    let important = base.join("important.json");
    std::fs::write(&important, b"must remain").unwrap();
    std::fs::set_permissions(&important, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = isolated_agsh(&base, &history)
        .args([
            "--background-state",
            important.to_str().unwrap(),
            "-c",
            "true",
        ])
        .output()
        .expect("invoke obsolete background-state path flag");

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(std::fs::read(&important).unwrap(), b"must remain");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn malformed_background_stdin_handoff_never_executes_payload() {
    let base = history_test_dir("background_state_malformed_stdin");
    let history = base.join("history.jsonl");
    let marker = base.join("must-not-exist");
    let mut command = isolated_agsh(&base, &history);
    command.args([
        "--background-state-stdin",
        "-c",
        &format!("printf ran > {}", marker.display()),
    ]);

    let output = run_with_piped_stdin(command, "{\"version\":1}");

    assert_eq!(output.status.code(), Some(1));
    assert!(!marker.exists());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("background state"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = std::fs::remove_dir_all(base);
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
fn exec_process_replacement_accepts_input_redirection() {
    let base = history_test_dir("exec_input_redirection");
    let history = base.join("history.jsonl");
    std::fs::write(base.join("input.txt"), b"exec-input").unwrap();

    let output = isolated_agsh(&base, &history)
        .args(["-c", "exec /bin/cat < input.txt"])
        .output()
        .expect("run exec with input redirection");

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"exec-input");
    assert!(output.stderr.is_empty());
    let _ = std::fs::remove_dir_all(base);
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
fn confine_inherited_empty_env_denies_every_external_command() {
    // Presence is significant: an empty serialized allowlist is deny-all, not
    // the same as an absent AGSH_CONFINE variable.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "/bin/echo should-not-run"])
        .env("AGSH_CONFINE", "")
        .output()
        .expect("run agsh");

    assert!(!String::from_utf8_lossy(&out.stdout).contains("should-not-run"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not permitted"));
}

#[test]
fn confine_deny_all_propagates_to_background_child() {
    // Intersecting disjoint sticky policies produces deny-all. Background
    // commands run in a child agsh, which must preserve that empty policy.
    let out = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args([
            "-c",
            "confine true; confine false; /bin/echo should-not-run & wait",
        ])
        .env_remove("AGSH_CONFINE")
        .output()
        .expect("run agsh");

    assert!(!String::from_utf8_lossy(&out.stdout).contains("should-not-run"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not permitted"));
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
fn run_requires_an_explicit_nonempty_capability_list() {
    let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--run", "printf UNCONFINED"])
        .output()
        .expect("run agsh");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--run requires a non-empty --allow"));
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
fn raw_external_pipeline_streams_stdout_and_stderr_before_exit() {
    use std::io::Read as _;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let base = history_test_dir("raw_pipeline_liveness");
    let history = base.join("history.jsonl");
    let mut child = isolated_agsh(&base, &history)
        .args([
            "-c",
            "sh -c 'printf OUT_READY; printf ERR_READY >&2; IFS= read -r release' | cat",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn raw pipeline");
    let mut release = child.stdin.take().expect("pipeline release stdin");

    let (sender, receiver) = mpsc::channel();
    let mut stdout = child.stdout.take().expect("pipeline stdout");
    let stdout_sender = sender.clone();
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = [0u8; 9];
        let result = stdout.read_exact(&mut bytes).map(|()| bytes);
        let _ = stdout_sender.send(("stdout", result));
    });
    let mut stderr = child.stderr.take().expect("pipeline stderr");
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = [0u8; 9];
        let result = stderr.read_exact(&mut bytes).map(|()| bytes);
        let _ = sender.send(("stderr", result));
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = std::collections::BTreeMap::new();
    while observed.len() < 2 {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match receiver.recv_timeout(remaining) {
            Ok((stream, result)) => {
                observed.insert(stream, result.expect("read live pipeline output"));
            }
            Err(_) => break,
        }
    }
    let was_still_running = child.try_wait().expect("poll raw pipeline").is_none();
    release.write_all(b"\n").expect("release raw pipeline");
    drop(release);
    let status = child.wait().expect("wait raw pipeline");
    stdout_reader.join().expect("join stdout reader");
    stderr_reader.join().expect("join stderr reader");

    assert_eq!(
        observed.get("stdout").map(<[u8; 9]>::as_slice),
        Some(b"OUT_READY".as_slice())
    );
    assert_eq!(
        observed.get("stderr").map(<[u8; 9]>::as_slice),
        Some(b"ERR_READY".as_slice())
    );
    assert!(
        was_still_running,
        "markers were only observable after the pipeline exited"
    );
    assert!(status.success());
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn raw_external_pipeline_streams_large_output_exactly() {
    use std::io::Read as _;

    const OUTPUT_LEN: u64 = 32 * 1024 * 1024;
    let base = history_test_dir("raw_pipeline_large");
    let history = base.join("history.jsonl");
    let output_path = base.join("pipeline.out");
    let output_file = std::fs::File::create(&output_path).unwrap();
    let status = isolated_agsh(&base, &history)
        .args(["-c", "head -c 33554432 /dev/zero | cat"])
        .stdout(Stdio::from(output_file))
        .status()
        .expect("run large raw pipeline");
    assert!(status.success());

    let mut output = std::fs::File::open(&output_path).unwrap();
    let mut chunk = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let count = output.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        assert!(chunk[..count].iter().all(|byte| *byte == 0));
        total += count as u64;
    }
    assert_eq!(total, OUTPUT_LEN);
    let _ = std::fs::remove_dir_all(base);
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
        .env("AGSH_BROKER_DIR", &dir)
        .output()
        .expect("run agsh");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // Returns immediately with a job id + the log path.
    let log = s
        .lines()
        .find_map(|l| l.split("output: ").nth(1))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| panic!("no job log path in:\n{s}"));
    assert!(
        log.starts_with(dir.join("logs")),
        "unexpected log: {}",
        log.display()
    );
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

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    let stopped = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "keep stop"])
        .env("AGSH_BROKER_DIR", &dir)
        .output()
        .expect("stop broker");
    assert!(stopped.status.success());
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
    for (label, src) in [
        ("command substitution", cmd_subst),
        ("subshell", subshell),
        ("function recursion", recursion),
    ] {
        let out = agsh_dash_c(&src);
        assert!(
            out.status.code().is_some(),
            "{label} killed by signal (stack overflow?)"
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

#[test]
fn pathological_glob_pattern_does_not_overflow_the_stack() {
    let source = format!("printf '%s\\n' {}", "*".repeat(30_000));
    let out = agsh_dash_c(&source);

    assert!(
        out.status.code().is_some(),
        "glob matcher was killed by a signal: {:?}",
        out.status
    );
}

#[test]
fn unicode_patterns_match_characters_instead_of_utf8_bytes() {
    let out = agsh_dash_c("v=é; case $v in ?) printf case-ok;; esac; printf '/%s' \"${v#?}\"");

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"case-ok/");
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

#[test]
fn non_tty_rich_mode_streams_raw_output_past_the_render_buffer_limit() {
    let base = history_test_dir("rich_raw_large_output");
    let history = base.join("history.jsonl");
    let mut agsh = isolated_agsh(&base, &history)
        .args(["--output", "rich", "-c", "head -c 67108865 /dev/zero"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn non-TTY rich agsh");
    let stdout = agsh.stdout.take().expect("agsh stdout pipe");
    let counter = Command::new("wc")
        .arg("-c")
        .stdin(Stdio::from(stdout))
        .output()
        .expect("count streamed rich bytes");
    let output = agsh.wait_with_output().expect("wait for rich agsh");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert!(counter.status.success());
    assert_eq!(String::from_utf8_lossy(&counter.stdout).trim(), "67108865");
    let _ = std::fs::remove_dir_all(base);
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
        ("alias forever=yes; forever | head -c1", "y"),
        ("yes <<<'ignored' | head -c1", "y"),
        ("{ yes; } <<<'ignored' | head -c1", "y"),
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
fn unsupported_pipeline_descriptor_fails_before_starting_producer() {
    let out = agsh_c_timeout("yes 3>/dev/null | head -c1", 5)
        .expect("unsupported pipeline descriptor must fail without running forever");

    assert_ne!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unsupported redirection"),
        "stderr={stderr:?}"
    );
    assert!(!stderr.contains("capture exceeds"), "stderr={stderr:?}");
}

#[test]
fn oversized_read_lines_and_printf_fields_fail_without_unbounded_allocation() {
    let printf = agsh_c_timeout("printf '%1000000000s' x", 5)
        .expect("oversized printf field must fail promptly");
    assert_ne!(printf.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&printf.stderr).contains("printf output limit"),
        "stderr={:?}",
        printf.stderr
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["-c", "read VALUE"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn agsh");
    let mut stdin = child.stdin.take().expect("stdin");
    let writer = std::thread::spawn(move || stdin.write_all(&vec![b'x'; 2 * 1024 * 1024]));
    let output = child.wait_with_output().expect("wait agsh");
    let _ = writer.join();
    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("read input line exceeds"),
        "stderr={:?}",
        output.stderr
    );
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
fn capturing_mode_preserves_the_child_locale() {
    // Observation must never change command semantics. A localized command may
    // fall back to generic compaction, but it must see the caller's locale.
    let compact = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "compact", "-c", "printenv LC_ALL"])
        .env("LC_ALL", "de_DE.UTF-8")
        .output()
        .expect("run agsh");
    let cs = String::from_utf8_lossy(&compact.stdout);
    assert!(
        cs.contains("de_DE.UTF-8"),
        "compact changed the child locale: {cs:?}"
    );

    // Raw mode must not alter the child's locale (exact-bytes contract).
    let raw = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "raw", "-c", "printenv LC_ALL"])
        .env("LC_ALL", "de_DE.UTF-8")
        .output()
        .expect("run agsh");
    assert_eq!(String::from_utf8_lossy(&raw.stdout), "de_DE.UTF-8\n");

    let assigned = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .args(["--output", "compact", "-c", "LC_ALL=C printenv LC_ALL"])
        .env("LC_ALL", "de_DE.UTF-8")
        .output()
        .expect("run agsh");
    assert!(String::from_utf8_lossy(&assigned.stdout).contains('C'));
}

#[test]
fn capturing_mode_preserves_temporal_order_after_fd_duplication() {
    for redirection in ["2>&1", "1>&2"] {
        let source =
            format!("sh -c 'printf o1; printf e1 >&2; printf o2; printf e2 >&2' {redirection}");
        let output = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["--output", "clean", "-c", &source])
            .output()
            .expect("run ordered merged output");

        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            b"o1e1o2e2",
            "fd duplication reordered output for {redirection}: stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
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

#[cfg(unix)]
#[test]
fn one_shot_lossless_ref_recovers_large_binary_output_exactly() {
    use std::os::unix::fs::PermissionsExt;

    let base = history_test_dir("one_shot_lossless_binary");
    let history = base.join("history.jsonl");
    let mut command = isolated_agsh(&base, &history);
    let output = command
        .args([
            "--output",
            "lossless-ref",
            "-c",
            r#"sh -c 'head -c 3145728 /dev/zero; printf "\377\200END"'"#,
        ])
        .env_remove("AGSH_TRACE_DIR")
        .output()
        .expect("run one-shot lossless capture");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert!(
        output.stdout.len() < 16 * 1024,
        "observation leaked the raw stream: {} bytes",
        output.stdout.len()
    );
    let display = String::from_utf8(output.stdout).expect("observation is UTF-8");
    let stdout_path = display
        .lines()
        .find_map(|line| line.strip_prefix("raw_stdout: "))
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("missing durable stdout reference:\n{display}"));
    assert!(
        !stdout_path.to_string_lossy().starts_with("trace://"),
        "one-shot reference is process-local: {}",
        stdout_path.display()
    );

    let raw = std::fs::read(&stdout_path).expect("read durable exact trace");
    assert_eq!(raw.len(), 3_145_733);
    assert!(raw[..3_145_728].iter().all(|byte| *byte == 0));
    assert_eq!(&raw[3_145_728..], &[0xff, 0x80, b'E', b'N', b'D']);
    assert_eq!(
        std::fs::metadata(&stdout_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(stdout_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );

    let stderr_path = display
        .lines()
        .find_map(|line| line.strip_prefix("raw_stderr: "))
        .map(PathBuf::from)
        .expect("stderr trace reference");
    let _ = std::fs::remove_file(stdout_path);
    let _ = std::fs::remove_file(stderr_path);
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn lossless_external_pipeline_is_bounded_and_recovers_exact_stream() {
    let base = history_test_dir("lossless_pipeline_binary");
    let history = base.join("history.jsonl");
    let traces = base.join("traces");
    let output = isolated_agsh(&base, &history)
        .args([
            "--output",
            "lossless-ref",
            "-c",
            "head -c 5000000 /dev/zero | cat",
        ])
        .env("AGSH_TRACE_DIR", &traces)
        .output()
        .expect("run lossless external pipeline");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert!(output.stdout.len() < 16 * 1024);
    let display = String::from_utf8(output.stdout).unwrap();
    let stdout_path = display
        .lines()
        .find_map(|line| line.strip_prefix("raw_stdout: "))
        .expect("stdout reference");
    let raw = std::fs::read(stdout_path).expect("read pipeline trace");
    assert_eq!(raw.len(), 5_000_000);
    assert!(raw.iter().all(|byte| *byte == 0));
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn large_command_substitution_never_receives_observation_elision_text() {
    let base = history_test_dir("large_command_substitution");
    let history = base.join("history.jsonl");
    let output = isolated_agsh(&base, &history)
        .args(["-c", r#"value=$(seq 1 500000); printf %s "${#value}""#])
        .output()
        .expect("run large command substitution");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert_eq!(output.stdout, b"3388894");
    assert!(!output.stdout.windows(5).any(|window| window == b"agsh:"));
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn oversized_command_substitution_fails_visibly() {
    let base = history_test_dir("oversized_command_substitution");
    let history = base.join("history.jsonl");
    let output = isolated_agsh(&base, &history)
        .args(["-c", "value=$(head -c 67108865 /dev/zero)"])
        .output()
        .expect("run oversized command substitution");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("in-memory shell capture exceeds 67108864 bytes"),
        "stderr={stderr:?}"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn large_process_substitution_preserves_every_byte() {
    let base = history_test_dir("large_process_substitution");
    let history = base.join("history.jsonl");
    let output = isolated_agsh(&base, &history)
        .args(["-c", "cat <(head -c 2500000 /dev/zero) | wc -c"])
        .output()
        .expect("run large process substitution");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "2500000");
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn non_tty_agview_preserves_large_file_bytes() {
    let base = history_test_dir("large_agview");
    let history = base.join("history.jsonl");
    let input = base.join("large.bin");
    let mut bytes = vec![0u8; 3 * 1024 * 1024];
    bytes[0..4].copy_from_slice(&[0xff, 0x00, 0x80, b'A']);
    bytes.extend_from_slice(b"END");
    std::fs::write(&input, &bytes).unwrap();
    let source = format!("agview '{}'", input.display());
    let output = isolated_agsh(&base, &history)
        .args(["-c", &source])
        .output()
        .expect("run agview");

    assert_eq!(output.status.code(), Some(0), "stderr={:?}", output.stderr);
    assert_eq!(output.stdout, bytes);
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn lossless_capture_rejects_symlinked_trace_directory_before_spawn() {
    use std::os::unix::fs::symlink;

    let base = history_test_dir("lossless_symlink_dir");
    let history = base.join("history.jsonl");
    let real = base.join("real-traces");
    let link = base.join("trace-link");
    let marker = base.join("payload-ran");
    std::fs::create_dir(&real).unwrap();
    symlink(&real, &link).unwrap();
    let source = format!("sh -c 'touch {}'", marker.display());
    let output = isolated_agsh(&base, &history)
        .args(["--output", "lossless-ref", "-c", &source])
        .env("AGSH_TRACE_DIR", &link)
        .output()
        .expect("run capture with hostile trace directory");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        !marker.exists(),
        "payload ran before trace storage was safe"
    );
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
    assert!(String::from_utf8_lossy(&output.stderr).contains("symlink"));
    let _ = std::fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn unsafe_temp_parent_refuses_shim_modes_before_payload_execution() {
    use std::os::unix::fs::PermissionsExt;

    let base = history_test_dir("unsafe_shim_temp_parent");
    let history = base.join("history.jsonl");
    let unsafe_temp = base.join("shared-temp");
    std::fs::create_dir(&unsafe_temp).unwrap();
    std::fs::set_permissions(&unsafe_temp, std::fs::Permissions::from_mode(0o777)).unwrap();

    let launch_marker = base.join("launch-payload-ran");
    let launch_source = format!("printf ran >'{}'", launch_marker.display());
    let launch = isolated_agsh(&base, &history)
        .env("TMPDIR", &unsafe_temp)
        .args(["--allow", "printf", "-c", &launch_source])
        .output()
        .expect("run launch confinement with unsafe TMPDIR");
    assert_eq!(launch.status.code(), Some(1));
    assert!(!launch_marker.exists());
    assert!(
        String::from_utf8_lossy(&launch.stderr).contains("cannot install confinement shell shims")
    );

    let sticky_marker = base.join("sticky-payload-ran");
    let sticky_source = format!(
        "confine printf && printf ran >'{}'",
        sticky_marker.display()
    );
    let sticky = isolated_agsh(&base, &history)
        .env("TMPDIR", &unsafe_temp)
        .args(["-c", &sticky_source])
        .output()
        .expect("run sticky confinement with unsafe TMPDIR");
    assert_eq!(sticky.status.code(), Some(1));
    assert!(!sticky_marker.exists());
    assert!(String::from_utf8_lossy(&sticky.stderr).contains("cannot install shell shims"));

    #[cfg(target_os = "linux")]
    {
        let run_marker = base.join("best-effort-payload-ran");
        let run_source = format!("printf ran >'{}'", run_marker.display());
        let best_effort = isolated_agsh(&base, &history)
            .env("TMPDIR", &unsafe_temp)
            .args(["--allow", "printf", "--best-effort", "--run", &run_source])
            .output()
            .expect("run best-effort confinement with unsafe TMPDIR");
        assert_eq!(best_effort.status.code(), Some(1));
        assert!(!run_marker.exists());
        assert!(String::from_utf8_lossy(&best_effort.stderr)
            .contains("cannot install confinement shell shims"));
    }

    std::fs::remove_dir_all(base).unwrap();
}
