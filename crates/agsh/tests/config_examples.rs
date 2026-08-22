use std::path::{Path, PathBuf};
use std::process::Command;

fn test_dir(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("agsh_config_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).expect("create isolated HOME");
    root
}

fn isolated_agsh(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agsh"));
    command
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("xdg-config"))
        .env("XDG_DATA_HOME", root.join("xdg-data"))
        .env("XDG_STATE_HOME", root.join("xdg-state"))
        .env("AGSH_HISTORY_FILE", root.join("history.jsonl"))
        .env("AGSH_TRUST_FILE", root.join("trust.jsonl"))
        .env("AGSH_SESSION_DIR", root.join("sessions"))
        .env("AGSH_BROKER_DIR", root.join("broker"))
        .env("AGSH_TRACE_DIR", root.join("traces"))
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

#[test]
fn shipped_agshrc_is_executable_and_does_not_replace_positionals() {
    let root = test_dir("rc_template");
    let template = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../configs/agsh/agshrc.example")
        .canonicalize()
        .expect("canonical example rc path");

    let output = isolated_agsh(&root)
        .env("AGSH_TEST_RC", template)
        .args([
            "-c",
            "set -- sentinel; source \"$AGSH_TEST_RC\"; printf '%s|%s\\n' \"$#\" \"$1\"; mode:output",
        ])
        .output()
        .expect("run shipped rc template");

    let _ = std::fs::remove_dir_all(&root);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"1|sentinel\nraw\n");
}

#[test]
fn inactive_configuration_examples_are_labeled_as_design_references() {
    let general = include_str!("../../../configs/agsh/config.toml");
    let policy = include_str!("../../../configs/agsh/policies/agent.workspace.toml");

    assert!(
        general
            .lines()
            .take(3)
            .any(|line| line.contains("NOT LOADED")),
        "config.toml must not look active before its runtime loader exists"
    );
    assert!(
        policy
            .lines()
            .take(3)
            .any(|line| line.contains("NOT LOADED")),
        "policy example must not look active before policy loading exists"
    );
}
