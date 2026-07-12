//! Integration tests for the keep broker: daemon lifecycle, job spawning on
//! broker-held PTYs, output logging, exit tracking, and signals — all against
//! the real `agsh` binary. Each test runs its own daemon on a private socket
//! (no env races; nothing touches the developer's real broker).

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use agsh_broker::{Client, JobKind, SpawnSpec};

fn broker_runtime_available() -> bool {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("agshb_probe_{}_{}", std::process::id(), stamp));
    if std::fs::create_dir_all(&dir).is_err() {
        return false;
    }
    let socket = dir.join("agshd.sock");
    let available = UnixListener::bind(&socket).is_ok();
    let _ = std::fs::remove_dir_all(&dir);
    if !available {
        eprintln!("skipping keep broker test: AF_UNIX sockets are unavailable in this runtime");
    }
    available
}

/// A foreground `agsh --broker-daemon` child on a private socket, killed on drop.
struct Daemon {
    child: Child,
    dir: PathBuf,
    client: Client,
}

impl Daemon {
    fn start(tag: &str) -> Self {
        // Short base dir: unix socket paths are capped (~104 bytes on macOS).
        let dir = std::env::temp_dir().join(format!("agshb_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_agsh"))
            .arg("--broker-daemon")
            .env("AGSH_BROKER_DIR", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn broker daemon");
        let client = Client::at(dir.join("agshd.sock"));
        let deadline = Instant::now() + Duration::from_secs(5);
        while client.ping().is_err() {
            assert!(Instant::now() < deadline, "broker did not come up");
            std::thread::sleep(Duration::from_millis(25));
        }
        Self { child, dir, client }
    }

    fn spawn_sh(&self, script: &str) -> agsh_broker::JobInfo {
        self.client
            .spawn_job(SpawnSpec {
                cmd: vec!["sh".into(), "-c".into(), script.into()],
                cwd: std::env::temp_dir().display().to_string(),
                env: vec![(
                    "PATH".into(),
                    std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
                )],
                opaque_env: Vec::new(),
                rows: 24,
                cols: 80,
                kind: JobKind::Job,
                title: script.into(),
            })
            .expect("spawn job")
    }

    fn wait_exit(&self, id: &str) -> i32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let info = self.client.status(id).expect("status");
            if !info.running {
                return info.exit_code.expect("exit code");
            }
            assert!(Instant::now() < deadline, "job {id} never exited");
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn broker_spawns_lists_logs_and_tracks_exit() {
    if !broker_runtime_available() {
        return;
    }
    let daemon = Daemon::start("basic");

    let info = daemon.spawn_sh("echo keep-probe; exit 7");
    assert!(info.pid > 0);
    assert_eq!(info.kind, JobKind::Job);

    // Exit is tracked with the real code.
    assert_eq!(daemon.wait_exit(&info.id), 7);

    // Output was journaled to the log (readable via tail).
    let tail = daemon.client.tail(&info.id, 4096).expect("tail");
    let text = String::from_utf8_lossy(&tail);
    assert!(text.contains("keep-probe"), "log tail: {text}");
    assert!(text.contains("exited: code 7"), "log tail: {text}");

    // Listed among jobs; removable once finished.
    let jobs = daemon.client.list().expect("list");
    assert!(jobs.iter().any(|j| j.id == info.id));
    daemon.client.remove(&info.id).expect("remove");
    let jobs = daemon.client.list().expect("list");
    assert!(!jobs.iter().any(|j| j.id == info.id));
}

#[test]
fn broker_rejects_a_missing_requested_cwd_without_spawning_a_job() {
    if !broker_runtime_available() {
        return;
    }
    let daemon = Daemon::start("missing-cwd");
    let missing = daemon.dir.join("does-not-exist");

    let error = daemon
        .client
        .spawn_job(SpawnSpec {
            cmd: vec!["true".into()],
            cwd: missing.display().to_string(),
            env: vec![(
                "PATH".into(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )],
            opaque_env: Vec::new(),
            rows: 24,
            cols: 80,
            kind: JobKind::Job,
            title: "invalid cwd".into(),
        })
        .unwrap_err();

    assert!(error.to_string().contains("spawn:"), "{error}");
    assert!(daemon.client.list().unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn broker_preserves_non_utf8_environment_values() {
    if !broker_runtime_available() {
        return;
    }
    let daemon = Daemon::start("opaque-env");
    let info = daemon
        .client
        .spawn_job(SpawnSpec {
            cmd: vec!["sh".into(), "-c".into(), "env".into()],
            cwd: std::env::temp_dir().display().to_string(),
            env: vec![(
                "PATH".into(),
                std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            )],
            opaque_env: vec![(b"AGSH_KEEP_OPAQUE".to_vec(), vec![b'a', 0xff, b'z'])],
            rows: 24,
            cols: 80,
            kind: JobKind::Job,
            title: "opaque env".into(),
        })
        .expect("spawn opaque environment job");
    assert_eq!(daemon.wait_exit(&info.id), 0);
    let tail = daemon
        .client
        .tail(&info.id, 64 * 1024)
        .expect("read opaque environment output");
    assert!(
        tail.windows(b"AGSH_KEEP_OPAQUE=a\xffz".len())
            .any(|window| window == b"AGSH_KEEP_OPAQUE=a\xffz"),
        "broker dropped or changed opaque environment bytes: {tail:?}"
    );
}

#[test]
fn kept_job_runs_on_a_real_pty_and_survives_clients() {
    if !broker_runtime_available() {
        return;
    }
    let daemon = Daemon::start("pty");

    // The job sees a TTY on stdio (that's the whole point of the PTY broker).
    let info = daemon.spawn_sh("if [ -t 0 ] && [ -t 1 ]; then echo is-a-tty; else echo no-tty; fi");
    assert_eq!(daemon.wait_exit(&info.id), 0);
    let text =
        String::from_utf8_lossy(&daemon.client.tail(&info.id, 4096).expect("tail")).into_owned();
    assert!(text.contains("is-a-tty"), "job must run on a PTY: {text}");

    // A long-running job stays alive across many short-lived clients — every
    // Client request here is its own connect/act/disconnect, which is exactly
    // what "shell died, new shell asks the broker" looks like on the wire.
    let long = daemon.spawn_sh("sleep 30");
    for _ in 0..3 {
        let fresh = Client::at(daemon.dir.join("agshd.sock"));
        let listed = fresh.list().expect("list from fresh client");
        let job = listed.iter().find(|j| j.id == long.id).expect("job listed");
        assert!(job.running, "job must keep running between clients");
    }

    // Signals go to the job's process group.
    daemon.client.signal(&long.id, "TERM").expect("signal");
    let code = daemon.wait_exit(&long.id);
    assert_eq!(code, 128 + 15, "sh killed by SIGTERM reports 143");
}

#[test]
fn attach_streams_output_and_forwards_input() {
    if !broker_runtime_available() {
        return;
    }
    use std::io::{Read, Write};

    let daemon = Daemon::start("attach");
    // `cat` echoes stdin to stdout through the PTY until EOF.
    let info = daemon.spawn_sh("echo ready; exec cat");

    // Wait for startup output to reach the log, then attach with replay.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !String::from_utf8_lossy(&daemon.client.tail(&info.id, 4096).unwrap_or_default())
        .contains("ready")
    {
        assert!(Instant::now() < deadline, "job produced no output");
        std::thread::sleep(Duration::from_millis(25));
    }

    let (stream, attached) = daemon
        .client
        .attach_stream(&info.id, 24, 80, 64 * 1024)
        .expect("attach");
    assert_eq!(attached.id, info.id);

    let mut reader = stream.try_clone().expect("clone");
    reader
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let mut writer = stream.try_clone().expect("clone");

    // Scrollback replay delivers the pre-attach output.
    let mut seen = String::new();
    let mut chunk = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.contains("ready") {
        assert!(Instant::now() < deadline, "no replay: {seen:?}");
        if let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }
    assert!(seen.contains("ready"), "replay missing: {seen:?}");

    // Input typed by the attached client reaches the job (cat echoes it back).
    writer.write_all(b"hello-from-attach\r").expect("write");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !seen.contains("hello-from-attach") {
        assert!(Instant::now() < deadline, "echo never arrived: {seen:?}");
        if let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            seen.push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }
    assert!(seen.contains("hello-from-attach"), "echo missing: {seen:?}");

    // Detaching (dropping the connection) leaves the job running.
    drop(reader);
    drop(writer);
    drop(stream);
    std::thread::sleep(Duration::from_millis(100));
    let status = daemon.client.status(&info.id).expect("status");
    assert!(status.running, "job must survive detach");
    assert!(!status.attached, "attach slot must be cleared");

    daemon.client.signal(&info.id, "KILL").expect("kill");
    daemon.wait_exit(&info.id);
}

#[test]
fn autostart_launches_a_daemon_on_demand() {
    if !broker_runtime_available() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("agshb_auto_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // `--broker-launch` under an isolated broker dir: run it as agsh would.
    let status = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .arg("--broker-launch")
        .env("AGSH_BROKER_DIR", &dir)
        .status()
        .expect("run broker-launch");
    assert!(status.success(), "launcher must exit 0");

    let client = Client::at(dir.join("agshd.sock"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while client.ping().is_err() {
        assert!(
            Instant::now() < deadline,
            "autostarted broker never answered"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // A second launch must not spawn a second daemon (bind fails; original
    // still answers).
    let status = Command::new(env!("CARGO_BIN_EXE_agsh"))
        .arg("--broker-launch")
        .env("AGSH_BROKER_DIR", &dir)
        .status()
        .expect("run broker-launch again");
    assert!(status.success());
    std::thread::sleep(Duration::from_millis(200));
    client.ping().expect("original daemon still answers");

    client.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Phase-2 property, end to end through the builtin: a `keep`-spawned job
/// outlives the shell process that started it, and later shells manage it.
#[test]
fn keep_builtin_job_survives_the_spawning_shell() {
    if !broker_runtime_available() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("agshb_bi_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let agsh = |cmd: &str| {
        Command::new(env!("CARGO_BIN_EXE_agsh"))
            .args(["-c", cmd])
            .env("AGSH_BROKER_DIR", &dir)
            .output()
            .expect("run agsh")
    };

    // Shell #1 spawns a kept job and EXITS (non-TTY ⇒ spawn detached).
    let out = agsh("keep -- sh -c 'echo builtin-probe; sleep 30'");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "spawn failed: {stdout}");
    assert!(stdout.contains("running detached"), "spawn: {stdout}");
    let id = stdout
        .split('[')
        .nth(1)
        .and_then(|s| s.split(']').next())
        .expect("job id in output")
        .to_string();

    // The spawning shell is gone; a NEW shell still sees the job running.
    let out = agsh("keep list");
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        listing.contains(&id) && listing.contains("running"),
        "job must survive its spawning shell: {listing}"
    );

    // Its output was journaled and is readable from yet another shell.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = agsh(&format!("keep tail {id}"));
        if String::from_utf8_lossy(&out.stdout).contains("builtin-probe") {
            break;
        }
        assert!(Instant::now() < deadline, "log never got the probe output");
        std::thread::sleep(Duration::from_millis(25));
    }

    // Kill it, confirm the exit is tracked, and clean up.
    let out = agsh(&format!("keep kill {id} KILL"));
    assert_eq!(out.status.code(), Some(0));
    let client = Client::at(dir.join("agshd.sock"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let info = client.status(&id).expect("status");
        if !info.running {
            assert_eq!(info.exit_code, Some(128 + 9));
            break;
        }
        assert!(Instant::now() < deadline, "job never died");
        std::thread::sleep(Duration::from_millis(25));
    }
    let out = agsh("keep stop");
    assert_eq!(out.status.code(), Some(0));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Last attach wins: a second client taking over hangs up the first WITHOUT
/// disturbing the job, and the first client can tell (job still running).
#[test]
fn attach_takeover_hangs_up_the_previous_client_only() {
    if !broker_runtime_available() {
        return;
    }
    use std::io::{ErrorKind, Read};

    let daemon = Daemon::start("steal");
    let info = daemon.spawn_sh("echo takeover-ready; exec cat");

    let (first, _) = daemon
        .client
        .attach_stream(&info.id, 24, 80, 4096)
        .expect("first attach");
    let mut first_reader = first.try_clone().expect("clone");
    first_reader
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("timeout");

    let (_second, attached) = daemon
        .client
        .attach_stream(&info.id, 24, 80, 4096)
        .expect("second attach");
    assert_eq!(attached.id, info.id);

    // The first client's stream is hung up (EOF after any buffered bytes)…
    let mut buf = [0u8; 4096];
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match first_reader.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {} // replayed scrollback before the hangup
            Err(ref e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("first client should see EOF, got {e}"),
        }
        assert!(Instant::now() < deadline, "first client never hung up");
    }
    // …while the job is untouched and the second client owns the attach.
    let status = daemon.client.status(&info.id).expect("status");
    assert!(status.running, "takeover must not disturb the job");
    assert!(status.attached, "second client must hold the attach");

    daemon.client.signal(&info.id, "KILL").expect("kill");
    daemon.wait_exit(&info.id);
}

/// Daemon shutdown must hang up its kept jobs (their PTY controllers close ⇒
/// SIGHUP). Regression for the CLOEXEC leak: a job that inherits its own
/// controller fd can never be hung up, silently orphaning shells forever.
#[test]
fn broker_shutdown_hangs_up_kept_jobs() {
    if !broker_runtime_available() {
        return;
    }
    let daemon = Daemon::start("hup");
    let info = daemon.spawn_sh("sleep 30");
    std::thread::sleep(Duration::from_millis(150)); // let setsid+exec settle

    daemon.client.shutdown().expect("shutdown");

    // The job's process must die of SIGHUP shortly after.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let alive = pid_probe(info.pid);
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "job {} survived broker shutdown (controller fd leak?)",
            info.pid
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `kill -0` probe: /bin/kill -0 exits 0 iff the process exists.
fn pid_probe(pid: i32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
