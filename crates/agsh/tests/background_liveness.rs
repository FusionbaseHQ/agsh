use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn isolate_command_environment(command: &mut Command, base: &Path) {
    let home = base.join("home");
    std::fs::create_dir_all(&home).expect("create isolated HOME");
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
}

#[test]
fn trailing_background_item_returns_before_the_job_in_raw_and_semantic_modes() {
    let root =
        std::env::temp_dir().join(format!("agsh-background-liveness-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create background liveness directory");

    let mut markers = Vec::new();
    for mode in ["raw", "semantic"] {
        let base = root.join(mode);
        std::fs::create_dir_all(&base).expect("create per-mode directory");
        let marker = base.join("finished");
        let stderr_path = base.join("stderr.log");
        let source = format!(
            "sh -c 'sleep 6; printf survived || exit $?; touch {}' &",
            marker.display()
        );
        let mut command = Command::new(env!("CARGO_BIN_EXE_agsh"));
        isolate_command_environment(&mut command, &base);
        if mode == "semantic" {
            command.args(["--output", "semantic"]);
        }

        let stderr = std::fs::File::create(&stderr_path).expect("create stderr log");
        let started = Instant::now();
        let status = command
            .args(["-c", &source])
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .status()
            .unwrap_or_else(|error| panic!("run trailing {mode} background item: {error}"));
        let elapsed = started.elapsed();
        let diagnostic = std::fs::read_to_string(&stderr_path).unwrap_or_default();

        assert!(
            status.success(),
            "{mode} mode returned {status:?} after {elapsed:?}; stderr={diagnostic:?}"
        );
        assert!(
            elapsed < Duration::from_millis(3500),
            "{mode} mode waited {elapsed:?} for the asynchronous payload; stderr={diagnostic:?}"
        );
        markers.push(marker);
    }

    let deadline = Instant::now() + Duration::from_secs(8);
    while markers.iter().any(|marker| !marker.exists()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    for marker in markers {
        assert!(
            marker.exists(),
            "background payload did not finish: {marker:?}"
        );
    }
    let _ = std::fs::remove_dir_all(root);
}
