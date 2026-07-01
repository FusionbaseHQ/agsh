//! Container / orchestration family compactor.
//!
//! Handles `docker`, `docker-compose` (and the `docker compose` subcommand),
//! `podman`, `nerdctl`, `kubectl`, `helm` and friends. The goal is to collapse
//! noisy build / rollout output into a small, structured observation:
//!
//! - `docker build`: collapse `Step X/Y` progress and `--->` layer lines into
//!   counts; keep `Successfully built`/`Successfully tagged` as notes; surface
//!   `ERROR:` / `failed to …` / non-zero-code lines as failures.
//! - `docker ps` / `docker images`: count table rows (excluding the header).
//! - `docker run`/`exec`/…: surface errors and the exit status.
//! - `docker compose up`: count `Started`/`Created` services, errors -> failures.
//! - `kubectl get`: count resource rows; flag non-healthy `STATUS` rows.
//! - `kubectl apply`: count `created`/`configured`/`unchanged`; server errors
//!   -> failures.
//! - `helm`: keep `STATUS:` / `NAME:` notes; errors -> failures.

use crate::summary::{CommandContext, SemanticSummary};
use crate::util::{clip, command_basename};
use regex::Regex;

/// Maximum characters kept for any single captured line.
const MAX_LINE: usize = 200;
/// Soft cap on entries collected into a single detail section.
const MAX_DETAIL: usize = 50;

/// Produce a semantic summary for a container / orchestration command.
pub fn summarize(cx: &CommandContext) -> SemanticSummary {
    match command_basename(cx.argv) {
        "kubectl" | "kustomize" => kubectl(cx),
        "helm" => helm(cx),
        // docker, docker-compose, podman, nerdctl, k9s, anything else routed here.
        _ => docker(cx),
    }
}

// ---------------------------------------------------------------------------
// docker / podman / nerdctl / compose
// ---------------------------------------------------------------------------

fn docker(cx: &CommandContext) -> SemanticSummary {
    let mut s = SemanticSummary::new(cx, "container");
    s.family = "docker".to_string();

    let args = nonflag_args(cx.argv);
    let is_compose_bin = command_basename(cx.argv) == "docker-compose";
    let compose = is_compose_bin || args.first().copied() == Some("compose");

    if compose {
        let op = if is_compose_bin {
            args.first().copied().unwrap_or("")
        } else {
            args.get(1).copied().unwrap_or("")
        };
        compose_summary(cx, &mut s, op);
        return s;
    }

    let op = args.first().copied().unwrap_or("");
    let op2 = args.get(1).copied();

    match op {
        "build" | "buildx" => docker_build(cx, &mut s),
        "ps" => docker_ps(cx, &mut s),
        "images" => docker_images(cx, &mut s),
        "container" if op2 == Some("ls") || op2 == Some("ps") => docker_ps(cx, &mut s),
        "image" if op2 == Some("ls") => docker_images(cx, &mut s),
        "run" | "create" | "start" | "exec" => docker_run(cx, &mut s, op),
        _ => docker_generic(cx, &mut s, op),
    }
    s
}

fn docker_build(cx: &CommandContext, s: &mut SemanticSummary) {
    let step_re = Regex::new(r"^Step (\d+)/(\d+)").unwrap();
    let mut steps = 0i64;
    let mut total_steps = 0i64;
    let mut layers = 0i64;

    for line in cx.all_lines() {
        let t = line.trim_start();
        if let Some(caps) = step_re.captures(t) {
            steps += 1;
            if let Ok(n) = caps[2].parse::<i64>() {
                total_steps = total_steps.max(n);
            }
            continue;
        }
        // `--->` (and the rarer `-->`) layer / cache progress lines.
        if t.starts_with("--->") || t.starts_with("-->") {
            layers += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("Successfully built ") {
            add_note_capped(s, format!("built {}", rest.trim()));
            continue;
        }
        if let Some(rest) = t.strip_prefix("Successfully tagged ") {
            add_note_capped(s, format!("tagged {}", rest.trim()));
            continue;
        }
        if is_error_line(line) {
            add_failure_capped(s, clip(line, MAX_LINE));
        }
    }

    s.set_count("steps", steps);
    if total_steps > 0 {
        s.set_count("total_steps", total_steps);
    }
    if layers > 0 {
        s.set_count("layers", layers);
    }

    let headline = if s.status == "ok" {
        if total_steps > 0 {
            format!("docker build: {steps}/{total_steps} steps completed")
        } else {
            format!("docker build: {steps} step(s)")
        }
    } else {
        format!("docker build failed after {steps} step(s)")
    };
    s.set_headline(headline);
}

fn docker_ps(cx: &CommandContext, s: &mut SemanticSummary) {
    let count = count_rows(cx.stdout, "CONTAINER ID");
    s.set_count("containers", count);
    for row in cx.stdout.lines() {
        if row.trim().is_empty() || row.contains("CONTAINER ID") {
            continue;
        }
        if row.contains("Exited") || row.contains("Dead") || row.contains("Restarting") {
            add_warning_capped(s, clip(row, MAX_LINE));
        }
    }
    scan_stderr_errors(cx, s);
    let headline = if s.status == "ok" {
        format!("docker ps: {count} container(s)")
    } else {
        format!("docker ps failed (exit {})", cx.exit_code)
    };
    s.set_headline(headline);
}

fn docker_images(cx: &CommandContext, s: &mut SemanticSummary) {
    let count = count_rows(cx.stdout, "REPOSITORY");
    s.set_count("images", count);
    scan_stderr_errors(cx, s);
    let headline = if s.status == "ok" {
        format!("docker images: {count} image(s)")
    } else {
        format!("docker images failed (exit {})", cx.exit_code)
    };
    s.set_headline(headline);
}

fn docker_run(cx: &CommandContext, s: &mut SemanticSummary, op: &str) {
    for line in cx.all_lines() {
        if is_error_line(line) {
            add_failure_capped(s, clip(line, MAX_LINE));
        }
    }
    let headline = if s.status == "ok" {
        format!("docker {op}: ok")
    } else {
        format!(
            "docker {op}: exit {} ({} error line(s))",
            cx.exit_code,
            s.failures.len()
        )
    };
    s.set_headline(headline);
}

fn docker_generic(cx: &CommandContext, s: &mut SemanticSummary, op: &str) {
    for line in cx.all_lines() {
        if is_error_line(line) {
            add_failure_capped(s, clip(line, MAX_LINE));
        }
    }
    let label = if op.is_empty() {
        "docker".to_string()
    } else {
        format!("docker {op}")
    };
    let headline = if s.status == "ok" {
        format!("{label}: ok")
    } else {
        format!("{label} failed (exit {})", cx.exit_code)
    };
    s.set_headline(headline);
}

fn compose_summary(cx: &CommandContext, s: &mut SemanticSummary, op: &str) {
    let mut started = 0i64;
    let mut created = 0i64;
    let mut removed = 0i64;

    for line in cx.all_lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Compose v2 status lines end in a capitalised verb; daemon errors are
        // caught by is_error_line.
        if is_error_line(line) || t.ends_with("Error") || t.ends_with("Failed") {
            add_failure_capped(s, clip(line, MAX_LINE));
            continue;
        }
        if t.ends_with("Started")
            || t.ends_with("Running")
            || (t.contains("Starting") && t.contains("done"))
        {
            started += 1;
        } else if t.ends_with("Created") || (t.contains("Creating") && t.contains("done")) {
            created += 1;
        } else if t.ends_with("Removed") || (t.contains("Removing") && t.contains("done")) {
            removed += 1;
        }
    }

    s.set_count("started", started);
    s.set_count("created", created);
    if removed > 0 {
        s.set_count("removed", removed);
    }

    let label = if op.is_empty() {
        "compose".to_string()
    } else {
        format!("compose {op}")
    };
    let headline = if s.status == "ok" {
        format!("{label}: {started} started, {created} created")
    } else {
        format!("{label} failed: {} error(s)", s.failures.len())
    };
    s.set_headline(headline);
}

// ---------------------------------------------------------------------------
// kubectl / kustomize
// ---------------------------------------------------------------------------

fn kubectl(cx: &CommandContext) -> SemanticSummary {
    let mut s = SemanticSummary::new(cx, "container");
    s.family = "kubectl".to_string();

    let args = nonflag_args(cx.argv);
    let op = args.first().copied().unwrap_or("");
    match op {
        "get" => kubectl_get(cx, &mut s),
        "apply" | "create" | "replace" | "delete" | "patch" => kubectl_apply(cx, &mut s, op),
        _ => kubectl_generic(cx, &mut s, op),
    }
    s
}

fn kubectl_get(cx: &CommandContext, s: &mut SemanticSummary) {
    // Server-side problems are reported on stderr; surface them either way.
    for line in cx.stderr.lines() {
        let t = line.trim();
        if t.contains("Error from server") || is_error_line(line) {
            add_failure_capped(s, clip(line, MAX_LINE));
        } else if t.contains("No resources found") {
            add_note_capped(s, clip(line, MAX_LINE));
        }
    }

    let lines: Vec<&str> = cx.stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        s.set_count("resources", 0);
        let headline = if s.status == "ok" {
            "kubectl get: 0 resources".to_string()
        } else {
            format!("kubectl get failed (exit {})", cx.exit_code)
        };
        s.set_headline(headline);
        return;
    }

    let header = lines[0];
    let has_header = header.contains("NAME");
    let status_idx = if has_header {
        header.split_whitespace().position(|c| c == "STATUS")
    } else {
        None
    };
    let data: &[&str] = if has_header { &lines[1..] } else { &lines[..] };

    let mut count = 0i64;
    let mut unhealthy = 0i64;
    for row in data {
        count += 1;
        if let Some(idx) = status_idx {
            let cols: Vec<&str> = row.split_whitespace().collect();
            if let Some(status) = cols.get(idx) {
                if !is_healthy_status(status) {
                    unhealthy += 1;
                    add_warning_capped(s, clip(row, MAX_LINE));
                }
            }
        }
    }

    s.set_count("resources", count);
    if unhealthy > 0 {
        s.set_count("unhealthy", unhealthy);
    }
    let headline = if s.status == "ok" {
        format!("kubectl get: {count} resource(s), {unhealthy} unhealthy")
    } else {
        format!("kubectl get failed (exit {})", cx.exit_code)
    };
    s.set_headline(headline);
}

fn kubectl_apply(cx: &CommandContext, s: &mut SemanticSummary, op: &str) {
    let mut created = 0i64;
    let mut configured = 0i64;
    let mut unchanged = 0i64;
    let mut deleted = 0i64;

    for line in cx.all_lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.contains("Error from server") || is_error_line(line) {
            add_failure_capped(s, clip(line, MAX_LINE));
            continue;
        }
        // Lines look like `resource/name created`, optionally `(dry run)`; match
        // the verb token so trailing suffixes don't break counting.
        let toks: Vec<&str> = t.split_whitespace().collect();
        if toks.contains(&"created") {
            created += 1;
        } else if toks.contains(&"configured") {
            configured += 1;
        } else if toks.contains(&"unchanged") {
            unchanged += 1;
        } else if toks.contains(&"deleted") {
            deleted += 1;
        }
    }

    s.set_count("created", created);
    s.set_count("configured", configured);
    s.set_count("unchanged", unchanged);
    if deleted > 0 {
        s.set_count("deleted", deleted);
    }

    let label = if op.is_empty() {
        "kubectl".to_string()
    } else {
        format!("kubectl {op}")
    };
    let headline = if s.status == "ok" {
        format!("{label}: {created} created, {configured} configured, {unchanged} unchanged")
    } else {
        format!("{label} failed: {} error(s)", s.failures.len())
    };
    s.set_headline(headline);
}

fn kubectl_generic(cx: &CommandContext, s: &mut SemanticSummary, op: &str) {
    for line in cx.all_lines() {
        let t = line.trim();
        if t.contains("Error from server") || is_error_line(line) {
            add_failure_capped(s, clip(line, MAX_LINE));
        }
    }
    let label = if op.is_empty() {
        "kubectl".to_string()
    } else {
        format!("kubectl {op}")
    };
    let headline = if s.status == "ok" {
        format!("{label}: ok")
    } else {
        format!("{label} failed (exit {})", cx.exit_code)
    };
    s.set_headline(headline);
}

// ---------------------------------------------------------------------------
// helm
// ---------------------------------------------------------------------------

fn helm(cx: &CommandContext) -> SemanticSummary {
    let mut s = SemanticSummary::new(cx, "container");
    s.family = "helm".to_string();

    let args = nonflag_args(cx.argv);
    let op = args.first().copied().unwrap_or("");
    let mut deployed = false;

    for line in cx.all_lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("STATUS:") {
            let st = rest.trim();
            add_note_capped(&mut s, format!("status: {st}"));
            if st.eq_ignore_ascii_case("deployed") {
                deployed = true;
            }
            continue;
        }
        if t.starts_with("NAME:") {
            add_note_capped(&mut s, clip(t, MAX_LINE));
            continue;
        }
        if is_error_line(line) {
            add_failure_capped(&mut s, clip(line, MAX_LINE));
        }
    }

    let label = if op.is_empty() {
        "helm".to_string()
    } else {
        format!("helm {op}")
    };
    let headline = if s.status != "ok" {
        format!("{label} failed")
    } else if deployed {
        format!("{label}: deployed")
    } else {
        format!("{label}: ok")
    };
    s.set_headline(headline);
    s
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Non-flag arguments after argv[0], in order (e.g. `["compose", "up"]`).
fn nonflag_args(argv: &[String]) -> Vec<&str> {
    argv.iter()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect()
}

/// Count non-empty rows in tabular output, dropping the header line when the
/// first row contains the expected column marker.
fn count_rows(stdout: &str, header_needle: &str) -> i64 {
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return 0;
    }
    let has_header = lines[0].contains(header_needle);
    (lines.len() as i64 - i64::from(has_header)).max(0)
}

/// kubectl resource statuses that should not be flagged.
fn is_healthy_status(status: &str) -> bool {
    matches!(
        status,
        "Running"
            | "Completed"
            | "Succeeded"
            | "Ready"
            | "Active"
            | "Bound"
            | "Available"
            | "Healthy"
    )
}

/// Heuristic: does a line look like a container/orchestration error?
fn is_error_line(line: &str) -> bool {
    let lower = line.trim().to_ascii_lowercase();
    lower.starts_with("error")
        || lower.starts_with("err:")
        || lower.contains("error response from daemon")
        || lower.contains("error from server")
        || lower.contains("failed to ")
        || lower.contains("failed:")
        || lower.contains("returned a non-zero code")
        || lower.contains("access denied")
        || lower.contains("no such ")
        || lower.contains("panic:")
}

fn scan_stderr_errors(cx: &CommandContext, s: &mut SemanticSummary) {
    for line in cx.stderr.lines() {
        if is_error_line(line) {
            add_failure_capped(s, clip(line, MAX_LINE));
        }
    }
}

fn add_failure_capped(s: &mut SemanticSummary, line: String) {
    if s.failures.len() < MAX_DETAIL {
        s.add_failure(line);
    }
}

fn add_warning_capped(s: &mut SemanticSummary, line: String) {
    if s.warnings.len() < MAX_DETAIL {
        s.add_warning(line);
    }
}

fn add_note_capped(s: &mut SemanticSummary, line: String) {
    if s.notes.len() < MAX_DETAIL {
        s.add_note(line);
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    fn cx<'a>(
        argv: &'a [String],
        exit: i32,
        stdout: &'a str,
        stderr: &'a str,
    ) -> CommandContext<'a> {
        CommandContext {
            cmd_id: "cmd_1".to_string(),
            argv,
            exit_code: exit,
            stdout,
            stderr,
        }
    }

    #[test]
    fn docker_build_success() {
        let a = argv(&["docker", "build", "-t", "myimage:latest", "."]);
        let out = "Sending build context to Docker daemon  3.072kB\n\
Step 1/3 : FROM alpine:3.18\n \
---> 8ca4688f4f35\n\
Step 2/3 : RUN apk add --no-cache curl\n \
---> Running in 1234567890ab\n \
---> abcdef123456\n\
Step 3/3 : CMD [\"sh\"]\n \
---> Running in 0a1b2c3d4e5f\n \
---> 2468013579bd\n\
Successfully built 2468013579bd\n\
Successfully tagged myimage:latest\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.family, "docker");
        assert_eq!(s.status, "ok");
        assert_eq!(s.counts.get("steps"), Some(&3));
        assert_eq!(s.counts.get("total_steps"), Some(&3));
        assert_eq!(s.counts.get("layers"), Some(&5));
        assert!(s.notes.iter().any(|n| n.contains("built 2468013579bd")));
        assert!(s.notes.iter().any(|n| n.contains("tagged myimage:latest")));
        assert!(s.headline.contains("3/3"));
    }

    #[test]
    fn docker_build_failure() {
        let a = argv(&["docker", "build", "."]);
        let out = "Step 1/3 : FROM alpine:3.18\n \
---> 8ca4688f4f35\n\
Step 2/3 : RUN false\n \
---> Running in 1234567890ab\n\
The command '/bin/sh -c false' returned a non-zero code: 1\n";
        let s = summarize(&cx(&a, 1, out, ""));
        assert_eq!(s.status, "failed");
        assert_eq!(s.counts.get("steps"), Some(&2));
        assert!(s.failures.iter().any(|f| f.contains("non-zero code")));
        assert!(s.headline.contains("failed"));
    }

    #[test]
    fn docker_ps_counts_and_warns() {
        let a = argv(&["docker", "ps", "-a"]);
        let out = "CONTAINER ID   IMAGE          COMMAND                  CREATED         STATUS                      PORTS     NAMES\n\
abc123def456   nginx:latest   \"/docker-entrypoint.…\"   2 hours ago     Up 2 hours                  80/tcp    web\n\
def456abc123   redis:7        \"docker-entrypoint.s…\"   3 hours ago     Exited (0) 5 minutes ago              cache\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts.get("containers"), Some(&2));
        assert!(s.warnings.iter().any(|w| w.contains("Exited")));
        assert_eq!(s.status, "ok");
    }

    #[test]
    fn docker_images_counts_rows() {
        let a = argv(&["docker", "images"]);
        let out = "REPOSITORY   TAG       IMAGE ID       CREATED        SIZE\n\
nginx        latest    abc123def456   2 days ago     142MB\n\
redis        7         def456abc123   3 days ago     117MB\n\
alpine       3.18      0123456789ab   4 days ago     7.34MB\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts.get("images"), Some(&3));
        assert_eq!(s.status, "ok");
    }

    #[test]
    fn docker_run_failure() {
        let a = argv(&["docker", "run", "nope"]);
        let err = "docker: Error response from daemon: pull access denied for nope, repository does not exist or may require 'docker login'.\n";
        let s = summarize(&cx(&a, 125, "", err));
        assert_eq!(s.status, "failed");
        assert!(s
            .failures
            .iter()
            .any(|f| f.contains("Error response from daemon")));
    }

    #[test]
    fn compose_up_old_format_with_error() {
        let a = argv(&["docker-compose", "up", "-d"]);
        let out = "Creating network \"myapp_default\" with the default driver\n\
Creating myapp_db_1  ... done\n\
Creating myapp_web_1 ... done\n\
Starting myapp_db_1  ... done\n";
        let err = "ERROR: for myapp_web_1  Cannot start service web: driver failed programming external connectivity\n";
        let s = summarize(&cx(&a, 1, out, err));
        assert_eq!(s.family, "docker");
        assert_eq!(s.counts.get("created"), Some(&2));
        assert_eq!(s.counts.get("started"), Some(&1));
        assert_eq!(s.status, "failed");
        assert!(!s.failures.is_empty());
    }

    #[test]
    fn compose_v2_started_created() {
        let a = argv(&["docker", "compose", "up", "-d"]);
        let out = " \u{2714} Network myapp_default      Created\n \
\u{2714} Container myapp-db-1       Started\n \
\u{2714} Container myapp-web-1      Started\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.counts.get("started"), Some(&2));
        assert_eq!(s.counts.get("created"), Some(&1));
        assert_eq!(s.status, "ok");
        assert!(s.headline.contains("2 started"));
    }

    #[test]
    fn kubectl_get_counts_and_flags_unhealthy() {
        let a = argv(&["kubectl", "get", "pods"]);
        let out = "NAME                      READY   STATUS             RESTARTS   AGE\n\
web-5d8f9c-abcde          1/1     Running            0          2d\n\
web-5d8f9c-fghij          0/1     CrashLoopBackOff   5          10m\n\
db-0                      1/1     Running            0          5d\n\
job-xyz-12345             0/1     Error              0          1m\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.family, "kubectl");
        assert_eq!(s.counts.get("resources"), Some(&4));
        assert_eq!(s.counts.get("unhealthy"), Some(&2));
        assert!(s.warnings.iter().any(|w| w.contains("CrashLoopBackOff")));
        assert!(s.warnings.iter().any(|w| w.contains("Error")));
        assert_eq!(s.status, "ok");
    }

    #[test]
    fn kubectl_apply_counts_actions_and_errors() {
        let a = argv(&["kubectl", "apply", "-f", "manifests/"]);
        let out = "namespace/foo created\n\
deployment.apps/web created\n\
service/web configured\n\
configmap/app-config unchanged\n";
        let err = "Error from server (NotFound): error when creating \"manifests/bad.yaml\": namespaces \"missing\" not found\n";
        let s = summarize(&cx(&a, 1, out, err));
        assert_eq!(s.counts.get("created"), Some(&2));
        assert_eq!(s.counts.get("configured"), Some(&1));
        assert_eq!(s.counts.get("unchanged"), Some(&1));
        assert_eq!(s.status, "failed");
        assert!(s.failures.iter().any(|f| f.contains("Error from server")));
    }

    #[test]
    fn helm_install_deployed() {
        let a = argv(&["helm", "install", "myapp", "./chart"]);
        let out = "NAME: myapp\n\
LAST DEPLOYED: Mon Jun 28 10:00:00 2026\n\
NAMESPACE: default\n\
STATUS: deployed\n\
REVISION: 1\n\
TEST SUITE: None\n";
        let s = summarize(&cx(&a, 0, out, ""));
        assert_eq!(s.family, "helm");
        assert_eq!(s.status, "ok");
        assert!(s.notes.iter().any(|n| n.contains("status: deployed")));
        assert!(s.notes.iter().any(|n| n.contains("NAME: myapp")));
        assert!(s.headline.contains("deployed"));
    }

    #[test]
    fn helm_install_failure() {
        let a = argv(&["helm", "install", "myapp", "./chart"]);
        let err = "Error: INSTALLATION FAILED: cannot re-use a name that is still in use\n";
        let s = summarize(&cx(&a, 1, "", err));
        assert_eq!(s.family, "helm");
        assert_eq!(s.status, "failed");
        assert!(s.failures.iter().any(|f| f.contains("INSTALLATION FAILED")));
        assert!(s.headline.contains("failed"));
    }
}
