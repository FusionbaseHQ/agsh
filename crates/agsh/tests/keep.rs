//! Integration tests for the keep broker: daemon lifecycle, job spawning on
//! broker-held PTYs, output logging, exit tracking, and signals — all against
//! the real `agsh` binary. Each test runs its own daemon on a private socket
//! (no env races; nothing touches the developer's real broker).

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agsh_broker::{Client, JobKind, SpawnSpec};

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
fn kept_job_runs_on_a_real_pty_and_survives_clients() {
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
