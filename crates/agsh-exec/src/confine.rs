//! OS-enforced `confine` for **leaf payloads** (`docs/MILESTONE_CONFINE_OS.md`).
//!
//! Unlike the best-effort shell shims (`install_confine_shims`), this enforces a
//! command allowlist at the **kernel** so the payload and every process it spawns
//! — by any means (`execve`, absolute paths, interpreters, `os.system`) — can only
//! execute the allowlisted binaries. It uses only OS-native facilities:
//!
//! * macOS — `sandbox-exec` (Seatbelt): `(deny process-exec*)` + an allowlist.
//! * Linux — Landlock (`LANDLOCK_ACCESS_FS_EXECUTE`). **Not built yet**: the
//!   `landlock` crate is not available in the offline cargo cache and agsh forbids
//!   `unsafe`, so the Linux backend currently reports *unavailable* (fail-closed).
//!   The seam is here for when the crate can be vendored.
//!
//! Two deliberate policies from the milestone:
//! * **Fail closed** — if no real backend is available, refuse (the caller may
//!   pass `best_effort` to fall back to the shim layer instead).
//! * **Refuse self-managing agents** — an agent like Claude Code needs a broad,
//!   open-ended runtime (node, keychain, network) that cannot be reduced to a
//!   small allowlist, and the kernel cannot separate its runtime from the commands
//!   it runs for you. So `confine … -- claude` errors with guidance instead of
//!   pretending to cage it.

use agsh_compat::{CommandResolution, Resolver};

use crate::state::ShellState;

/// Self-managing agent runtimes that cannot be reduced to a command allowlist.
/// Matched on the payload's basename; extend via `AGSH_CONFINE_AGENTS`, opt out
/// via `AGSH_CONFINE_ALLOW_AGENTS` (both comma/space-separated).
const KNOWN_AGENTS: &[&str] = &[
    "claude", "cursor", "aider", "codex", "goose", "copilot", "gemini", "qwen", "cody", "cline",
    "continue", "amp", "opencode",
];

/// Interpreter/loader environment variables an untrusted payload could use to
/// inject code (DYLD/LD preload, interpreter startup hooks). Scrubbed by the
/// confining presets so a sandboxed script can't subvert the tools it runs.
const INJECTION_ENV_VARS: &[&str] = &[
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "DYLD_FRAMEWORK_PATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "PYTHONPATH",
    "PYTHONSTARTUP",
    "BASH_ENV",
    "ENV",
    "NODE_OPTIONS",
    "PERL5LIB",
    "PERL5OPT",
    "RUBYOPT",
    "RUBYLIB",
];

/// Credential-bearing environment variables scrubbed by the confining presets,
/// so a sandboxed payload can't read a cloud/API token straight out of the
/// inherited environment. NOT exhaustive — the stronger long-term model is an
/// allowlist (pass only PATH/HOME/TERM/LANG + explicit `--env`); see
/// docs/CONFINE.md — but this covers the common high-value secrets. `SSH_AUTH_SOCK`
/// is included so the ssh-agent socket path is gone even before the network deny.
const CREDENTIAL_ENV_VARS: &[&str] = &[
    "SSH_AUTH_SOCK",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_SECURITY_TOKEN",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITLAB_TOKEN",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "HF_TOKEN",
    "HUGGING_FACE_HUB_TOKEN",
    "NPM_TOKEN",
    "PYPI_TOKEN",
    "VAULT_TOKEN",
    "DATABASE_URL",
    "SLACK_TOKEN",
    "CLOUDFLARE_API_TOKEN",
    "DIGITALOCEAN_ACCESS_TOKEN",
    "DOCKER_PASSWORD",
];

/// Credential locations denied for reading under the confining presets (relative
/// to `$HOME` unless absolute). Closes the read-then-exfiltrate path; pair with
/// network-deny so even an in-process read can't leave the host.
const SECRET_SUBPATHS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".config/gcloud",
    ".kube",
    ".docker/config.json",
    ".npmrc",
    ".pypirc",
    ".netrc",
    ".git-credentials",
    "Library/Keychains",
];

/// A named capability preset for `confine`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    /// v1 behavior: exec-allowlist only; filesystem, network, env all open.
    #[default]
    ExecOnly,
    /// No filesystem writes (except a private scratch + /dev/null), network and
    /// secret reads denied, injection env vars scrubbed. For untrusted scripts.
    ReadOnly,
    /// Writes only within the working directory (+ scratch); otherwise read-only.
    Workspace,
}

/// Options parsed from `confine` flags / launch env.
#[derive(Debug, Default, Clone)]
pub struct ConfineOpts {
    /// Bypass the self-managing-agent refusal (`--force`).
    pub force: bool,
    /// Fall back to the best-effort shim layer when no OS backend is available
    /// (`--best-effort`), instead of refusing.
    pub best_effort: bool,
    /// Capability preset (default `ExecOnly` = v1).
    pub preset: Preset,
    /// Network override: `Some(true)` = allow (`--net`), `Some(false)` = deny
    /// (`offline`/`--no-net`), `None` = preset default.
    pub net: Option<bool>,
    /// Extra writable roots (`--rw PATH`).
    pub writable: Vec<String>,
    /// Drop the secret-read-deny baseline (`--allow-secrets`).
    pub allow_secrets: bool,
    /// Print the resolved capabilities and still run (`--explain`).
    pub explain: bool,
    /// Print the generated profile + command and do NOT run (`--dry-run`).
    pub dry_run: bool,
}

/// Which kernel enforcement backend is available on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// macOS Seatbelt via `/usr/bin/sandbox-exec`.
    SandboxExec,
    /// No real OS enforcement available (Linux without the Landlock impl, etc.).
    Unavailable,
}

/// The trusted macOS Seatbelt launcher. Backend detection and execution must use
/// this same absolute path so a confined command can never select a PATH-provided
/// `sandbox-exec` replacement after the backend probe succeeds.
const SANDBOX_EXEC_PATH: &str = "/usr/bin/sandbox-exec";

/// What the caller should do with a confined payload.
pub enum ConfinePlan {
    /// Refuse: print `message` to stderr and exit with `code`.
    Refuse { message: String, code: i32 },
    /// Run this (already kernel-wrapped) command; enforcement is guaranteed.
    /// `cleanup` lists per-invocation temp paths (profile file, scratch dir) the
    /// caller must remove after the run. `explain` is a human summary to print
    /// first when `--explain`/`--dry-run`.
    Sandboxed {
        command: String,
        cleanup: Vec<std::path::PathBuf>,
        explain: Option<String>,
        /// Exported variables the caller must remove before starting the trusted
        /// sandbox launcher. Removing them inside the payload is too late for
        /// dynamic-loader injection variables.
        env_remove: Vec<String>,
    },
    /// No OS backend, but `best_effort` was set: install shims and run the
    /// original payload (caller does this — see `install_confine_shims`).
    BestEffort,
}

/// Effective filesystem-write policy lowered from a preset + flags.
#[derive(Debug, Clone)]
enum WritePolicy {
    /// Writes unrestricted (the bins-tamper deny still applies).
    Open,
    /// Writes denied everywhere except these canonical roots (+ scratch).
    DenyExcept(Vec<String>),
    /// Writes denied everywhere except scratch + /dev/null.
    DenyAll,
}

/// The resolved capability set for one confine invocation.
#[derive(Debug, Clone)]
struct Caps {
    write: WritePolicy,
    net_deny: bool,
    deny_secrets: bool,
    scrub_env: bool,
    /// Provision a private writable scratch dir (and point TMPDIR at it).
    scratch: bool,
}

/// Variables that must be absent before the trusted sandbox launcher starts.
/// Removing them inside the payload shell is too late for loader variables such
/// as `DYLD_INSERT_LIBRARIES`: they can execute code while starting
/// `sandbox-exec` itself, before Seatbelt has applied the requested profile.
fn prelaunch_env_remove(state: &ShellState, caps: &Caps) -> Vec<String> {
    if !caps.scrub_env {
        return Vec::new();
    }
    let mut names = std::collections::BTreeSet::new();
    names.extend(INJECTION_ENV_VARS.iter().map(|name| (*name).to_string()));
    names.extend(CREDENTIAL_ENV_VARS.iter().map(|name| (*name).to_string()));
    names.extend(
        state
            .exported_env_names()
            .into_iter()
            .filter(|name| {
                agsh_output::is_sensitive_env_name(name)
                    || name.starts_with("DYLD_")
                    || name.starts_with("LD_")
            })
            .map(str::to_string),
    );
    names.into_iter().collect()
}

impl Caps {
    /// Lower a preset + flags to a capability set, given the working directory.
    fn resolve(opts: &ConfineOpts, cwd: &std::path::Path) -> Caps {
        // Network: explicit flag wins; otherwise the confining presets deny.
        let preset_net_deny = !matches!(opts.preset, Preset::ExecOnly);
        let net_deny = match opts.net {
            Some(allow) => !allow,
            None => preset_net_deny,
        };
        let extra_writable = || -> Vec<String> {
            opts.writable
                .iter()
                .filter_map(|p| canonical_root(p, cwd))
                .collect()
        };
        match opts.preset {
            Preset::ExecOnly => Caps {
                // exec-only never restricts writes broadly; --rw still scopes them.
                write: if opts.writable.is_empty() {
                    WritePolicy::Open
                } else {
                    WritePolicy::DenyExcept(extra_writable())
                },
                net_deny,
                deny_secrets: false,
                scrub_env: false,
                scratch: !opts.writable.is_empty(),
            },
            Preset::ReadOnly => {
                let roots = extra_writable();
                Caps {
                    write: if roots.is_empty() {
                        WritePolicy::DenyAll
                    } else {
                        WritePolicy::DenyExcept(roots)
                    },
                    net_deny,
                    deny_secrets: !opts.allow_secrets,
                    scrub_env: true,
                    scratch: true,
                }
            }
            Preset::Workspace => {
                let mut roots = extra_writable();
                if let Some(c) = canonical_root(".", cwd) {
                    roots.push(c);
                }
                Caps {
                    write: WritePolicy::DenyExcept(roots),
                    net_deny,
                    deny_secrets: !opts.allow_secrets,
                    scrub_env: true,
                    scratch: true,
                }
            }
        }
    }
}

/// Probe for a real OS enforcement backend.
pub fn detect_backend() -> Backend {
    if cfg!(target_os = "macos") && std::path::Path::new(SANDBOX_EXEC_PATH).exists() {
        return Backend::SandboxExec;
    }
    // Linux Landlock would be selected here once the `landlock` crate is vendored
    // (offline) and an unsafe-free applier (self-exec launcher) is wired in. Until
    // then we report Unavailable so confine fails closed rather than under-enforce.
    Backend::Unavailable
}

/// The final path component of a command (handles `/` and `\\`).
fn basename(cmd: &str) -> &str {
    cmd.rsplit(['/', '\\']).next().unwrap_or(cmd)
}

/// Commands that transparently run another command — the agent could hide behind
/// them (`env claude`, `nice -n5 claude`, `timeout 5 claude`).
const WRAPPER_COMMANDS: &[&str] = &[
    "env", "nice", "timeout", "nohup", "stdbuf", "setsid", "ionice", "chrt", "sudo", "doas",
];

/// Whether `payload0` is a self-managing agent we refuse to confine. The compare
/// is **case-insensitive** because macOS/APFS resolves `Claude` → `claude`.
pub fn is_self_managing_agent(payload0: &str, state: &ShellState) -> bool {
    let name = basename(payload0).to_ascii_lowercase();
    let listed = |var: &str| -> Vec<String> {
        state
            .lookup(var)
            .unwrap_or("")
            .split([',', ' ', '\t'])
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    if listed("AGSH_CONFINE_ALLOW_AGENTS")
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&name))
    {
        return false;
    }
    KNOWN_AGENTS.contains(&name.as_str())
        || listed("AGSH_CONFINE_AGENTS")
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&name))
}

/// Scan a payload's argv for a self-managing agent in command position, seeing
/// through leading `VAR=val` assignments and transparent wrappers (`env`, `nice`,
/// `timeout`, …). Returns the agent's name if found. This makes the refusal robust
/// against `env claude`, `nice -n5 claude`, `VAR=x claude`, etc.
pub fn agent_in_payload(tokens: &[String], state: &ShellState) -> Option<String> {
    // Skip leading environment assignments (`NAME=value`).
    let is_assignment = |t: &str| {
        !t.starts_with('-')
            && t.split_once('=').is_some_and(|(k, _)| {
                !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
    };
    let mut i = 0;
    while tokens.get(i).is_some_and(|t| is_assignment(t)) {
        i += 1;
    }
    let head = tokens.get(i)?;
    if is_self_managing_agent(head, state) {
        return Some(basename(head).to_string());
    }
    // See through a transparent wrapper to the command it runs. Wrappers have
    // irregular argument shapes (`timeout 5 cmd`, `nice -n5 cmd`, `env X=1 cmd`),
    // so conservatively refuse if ANY following token names an agent.
    if WRAPPER_COMMANDS.contains(&basename(head).to_ascii_lowercase().as_str()) {
        return tokens[i + 1..]
            .iter()
            .find(|t| is_self_managing_agent(t, state))
            .map(|t| basename(t).to_string());
    }
    None
}

fn agent_refusal(payload0: &str) -> String {
    let name = basename(payload0);
    format!(
        "confine: '{name}' is a self-managing agent and cannot be reduced to a command allowlist \
         (it needs its own runtime — node, keychain, network, …). Use its own permission system \
         instead, e.g.:  {name} --allowedTools \"Bash(ls *)\" \"Bash(df *)\"\n\
         (confine targets leaf commands; pass --force to override at your own risk.)\n"
    )
}

fn no_backend_refusal() -> String {
    let detail = if cfg!(target_os = "macos") {
        "sandbox-exec was not found"
    } else if cfg!(target_os = "linux") {
        "Landlock enforcement is not built into this agsh (needs the landlock crate)"
    } else {
        "no OS sandbox is available on this platform"
    };
    format!(
        "confine: cannot enforce a real command allowlist here — {detail}. \
         Re-run with --best-effort to use the (weaker) shell-shim layer instead.\n"
    )
}

/// Single-quote a string so it survives one more pass of shell parsing.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn split_list(s: &str) -> Vec<String> {
    s.split([',', ' ', '\t'])
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

fn preset_from_name(name: &str) -> Result<Preset, String> {
    match name {
        "exec-only" | "exec" => Ok(Preset::ExecOnly),
        "read-only" | "readonly" => Ok(Preset::ReadOnly),
        "workspace" => Ok(Preset::Workspace),
        other => Err(format!("confine: unknown preset '{other}'")),
    }
}

fn select_preset(
    opts: &mut ConfineOpts,
    selected: &mut Option<Preset>,
    preset: Preset,
) -> Result<(), String> {
    if selected.is_some_and(|existing| existing != preset) {
        return Err("confine: conflicting presets".to_string());
    }
    *selected = Some(preset);
    opts.preset = preset;
    Ok(())
}

fn select_network(opts: &mut ConfineOpts, allow: bool) -> Result<(), String> {
    if opts.net.is_some_and(|existing| existing != allow) {
        return Err("confine: conflicting network options".to_string());
    }
    opts.net = Some(allow);
    Ok(())
}

/// Parse confine spec tokens (everything before `--`) into an exec-allowlist and
/// `ConfineOpts`. Tokens are classified by **shape**: `--flag` flags, known preset
/// keywords, else exec-allowlist entries — so order is free and the bare
/// `confine LIST` form still works.
pub fn parse_spec(tokens: &[String]) -> Result<(Vec<String>, ConfineOpts), String> {
    let mut allow: Vec<String> = Vec::new();
    let mut opts = ConfineOpts::default();
    let mut selected_preset = None;
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        match t {
            "--force" => opts.force = true,
            "--best-effort" => opts.best_effort = true,
            "--net" => select_network(&mut opts, true)?,
            "--no-net" => select_network(&mut opts, false)?,
            "--allow-secrets" => opts.allow_secrets = true,
            "--explain" => opts.explain = true,
            "--dry-run" => opts.dry_run = true,
            "--cwd" => opts.writable.push(".".to_string()),
            "--rw" | "-w" => {
                i += 1;
                let p = tokens.get(i).ok_or("confine: --rw needs a PATH")?;
                opts.writable.push(p.clone());
            }
            "--allow" => {
                i += 1;
                let l = tokens.get(i).ok_or("confine: --allow needs a LIST")?;
                allow.extend(split_list(l));
            }
            "--preset" => {
                i += 1;
                let name = tokens.get(i).ok_or("confine: --preset needs a NAME")?;
                let preset = preset_from_name(name)?;
                select_preset(&mut opts, &mut selected_preset, preset)?;
            }
            "read-only" | "readonly" => {
                select_preset(&mut opts, &mut selected_preset, Preset::ReadOnly)?;
            }
            "workspace" => {
                select_preset(&mut opts, &mut selected_preset, Preset::Workspace)?;
            }
            "offline" | "no-net" => select_network(&mut opts, false)?,
            _ if t.starts_with('-') => return Err(format!("confine: unknown option '{t}'")),
            _ => allow.extend(split_list(t)),
        }
        i += 1;
    }
    Ok((allow, opts))
}

/// Decide how to confine a payload to `allowed`, per the milestone policy.
///
/// `payload_tokens` are the raw argv tokens (for agent detection: program +
/// wrapper/assignment scanning); `payload_shell` is the ready-to-run shell
/// command handed to `/bin/sh -c` (callers shell-quote tokens so quoting is
/// preserved). The first token is the program for the allowed-runtime set.
pub fn plan(
    state: &ShellState,
    allowed: &[String],
    payload_tokens: &[String],
    payload_shell: &str,
    opts: &ConfineOpts,
) -> ConfinePlan {
    if !opts.force {
        if let Some(agent) = agent_in_payload(payload_tokens, state) {
            return ConfinePlan::Refuse {
                message: agent_refusal(&agent),
                code: 2,
            };
        }
    }
    let payload0 = payload_tokens.first().map(String::as_str).unwrap_or("");

    match detect_backend() {
        Backend::SandboxExec => {
            match sandbox_exec_wrap(state, allowed, payload0, payload_shell, opts) {
                Some(plan) => plan,
                None if opts.best_effort => ConfinePlan::BestEffort,
                None => ConfinePlan::Refuse {
                    message: "confine: failed to build the sandbox profile\n".to_string(),
                    code: 2,
                },
            }
        }
        Backend::Unavailable if opts.best_effort => ConfinePlan::BestEffort,
        Backend::Unavailable => ConfinePlan::Refuse {
            message: no_backend_refusal(),
            code: 2,
        },
    }
}

/// Resolve a command name (or path) to an absolute executable path.
fn resolve(name: &str, path_value: &str) -> Option<String> {
    match Resolver::default().resolve_external_only(name, Some(path_value)) {
        Some(CommandResolution::External(p)) => Some(p.display().to_string()),
        _ => None,
    }
}

/// If `path` is a script with a `#!interp …` shebang, return executable names
/// needed by that shebang. The kernel only accepts a short shebang line; cap the
/// read so confining a multi-gigabyte payload cannot allocate its full size.
/// For `/usr/bin/env python`, include both `env` and `python`.
fn shebang_commands(path: &str) -> Vec<String> {
    use std::io::Read as _;

    const MAX_SHEBANG_BYTES: u64 = 4096;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut data = Vec::with_capacity(MAX_SHEBANG_BYTES as usize);
    if file.take(MAX_SHEBANG_BYTES).read_to_end(&mut data).is_err() {
        return Vec::new();
    }
    let Some(first) = data.split(|&b| b == b'\n').next() else {
        return Vec::new();
    };
    let Ok(line) = std::str::from_utf8(first) else {
        return Vec::new();
    };
    let Some(rest) = line.trim().strip_prefix("#!") else {
        return Vec::new();
    };
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let Some(interpreter) = parts.first() else {
        return Vec::new();
    };
    let mut commands = vec![(*interpreter).to_string()];
    if basename(interpreter).eq_ignore_ascii_case("env") {
        if let Some(command) = env_shebang_command(&parts[1..]) {
            commands.push(command.to_string());
        }
    }
    commands
}

fn env_shebang_command<'a>(parts: &'a [&str]) -> Option<&'a str> {
    let mut index = 0usize;
    while let Some(part) = parts.get(index).copied() {
        match part {
            "--" | "-S" | "--split-string" => index += 1,
            "-u" | "--unset" | "-C" | "--chdir" => index += 2,
            "-i" | "--ignore-environment" | "-0" | "--null" | "-v" | "--debug" => index += 1,
            _ if part.starts_with("--unset=") || part.starts_with("--chdir=") => index += 1,
            // Unknown options may consume the next token. Never guess that an
            // option value is the executable and accidentally widen the profile.
            _ if part.starts_with('-') => return None,
            _ if is_env_assignment(part) => index += 1,
            _ => return Some(part),
        }
    }
    None
}

fn is_env_assignment(value: &str) -> bool {
    value.split_once('=').is_some_and(|(name, _)| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
            && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    })
}

/// Canonicalize `p` then insert both the canonical and original path so a
/// symlinked binary (Homebrew `python`) is allowed and `subpath` rules can't be
/// silently bypassed via a symlinked path.
fn insert_exec(set: &mut std::collections::BTreeSet<String>, p: String) {
    if let Ok(real) = std::fs::canonicalize(&p) {
        set.insert(real.display().to_string());
    }
    set.insert(p);
}

/// Build a macOS Seatbelt profile lowering the capability set (exec allowlist +
/// optional filesystem-write / secret-read / network / cross-process denies),
/// write it to a securely-created temp file, provision a private scratch dir, and
/// return the `sandbox-exec -f PROFILE /bin/sh -c PAYLOAD` command to run.
pub fn sandbox_exec_wrap(
    state: &ShellState,
    allowed: &[String],
    payload0: &str,
    payload_shell: &str,
    opts: &ConfineOpts,
) -> Option<ConfinePlan> {
    let path_value = state.lookup("PATH").unwrap_or_default().to_string();
    let caps = Caps::resolve(opts, state.cwd());

    // --- exec allowlist + leaf runtime (canonicalized) ---
    let mut execs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for name in allowed {
        if let Some(p) = resolve(name, &path_value) {
            insert_exec(&mut execs, p);
        }
    }
    // `/bin/sh` is the launcher used below. Do not grant other shells unless a
    // payload/shebang or the explicit allowlist actually needs them.
    if std::path::Path::new("/bin/sh").exists() {
        insert_exec(&mut execs, "/bin/sh".to_string());
    }
    // On macOS `/bin/sh` selects `/bin/bash` as its executable variant at
    // startup. Seatbelt reports "Failed to exec /bin/bash as variant" unless
    // both are present, even though `/bin/sh` is a distinct regular file.
    if cfg!(target_os = "macos") && std::path::Path::new("/bin/bash").exists() {
        insert_exec(&mut execs, "/bin/bash".to_string());
    }
    if let Some(p) = resolve(payload0, &path_value) {
        for interp in shebang_commands(&p) {
            if let Some(ip) = resolve(&interp, &path_value) {
                insert_exec(&mut execs, ip);
            }
        }
        insert_exec(&mut execs, p);
    }

    // --- scratch dir (writable temp), if the preset wants one ---
    let scratch = if caps.scratch { make_scratch() } else { None };
    if caps.scratch && scratch.is_none() {
        return None; // a preset that needs scratch but couldn't make one: fail closed
    }

    // --- profile ---
    let mut profile = String::from("(version 1)\n(allow default)\n");
    profile.push_str("(deny process-exec*)\n(allow process-exec*\n");
    for e in &execs {
        profile.push_str(&format!("  (literal {})\n", sbpl_string(e)));
    }
    profile.push_str(")\n");

    // Filesystem-write policy (deny then re-allow writable roots + scratch).
    let restricted = !matches!(caps.write, WritePolicy::Open);
    if restricted {
        profile.push_str("(deny file-write*)\n(allow file-write*\n");
        let mut roots: Vec<String> = Vec::new();
        if let WritePolicy::DenyExcept(rs) = &caps.write {
            roots.extend(rs.iter().cloned());
        }
        if let Some(s) = &scratch {
            roots.push(s.display().to_string());
        }
        for r in &roots {
            profile.push_str(&format!("  (subpath {})\n", sbpl_string(r)));
        }
        for dev in ["/dev/null", "/dev/dtracehelper", "/dev/tty"] {
            profile.push_str(&format!("  (literal {})\n", sbpl_string(dev)));
        }
        profile.push_str("  (regex #\"^/dev/(tty|fd/|std)\")\n");
        profile.push_str(")\n");
    }
    // Tamper protection: the allowlisted binaries are never writable (emitted LAST
    // so it beats any scratch/root allow above).
    profile.push_str("(deny file-write*\n");
    for e in &execs {
        profile.push_str(&format!("  (literal {})\n", sbpl_string(e)));
    }
    profile.push_str(")\n");

    // Secret-read denial.
    if caps.deny_secrets {
        let home = state.lookup("HOME").unwrap_or("").to_string();
        profile.push_str("(deny file-read*\n");
        for sub in SECRET_SUBPATHS {
            let p = if sub.starts_with('/') {
                sub.to_string()
            } else {
                format!("{home}/{sub}")
            };
            profile.push_str(&format!("  (subpath {})\n", sbpl_string(&p)));
        }
        profile.push_str("  (subpath \"/Library/Keychains\")\n");
        profile.push_str(")\n");
    }

    // Network denial. Deny *all* outbound/inbound network, including AF_UNIX
    // sockets: previously `(allow … remote unix-socket)` re-opened them, which let
    // a confined payload connect to `/var/run/docker.sock` (→ full host root) or
    // `$SSH_AUTH_SOCK`, defeating the network, write, and secret-read denies at
    // once. Basic IPC is not worth that escape; per-socket opt-in can be added
    // later if a concrete need appears.
    if caps.net_deny {
        profile.push_str("(deny network-outbound)\n(deny network-inbound)\n");
    }

    // Cross-process hardening for the confining presets: a payload can't inspect,
    // debug, or signal the agent or sibling processes.
    if !matches!(opts.preset, Preset::ExecOnly) {
        profile.push_str("(deny process-info* (target others))\n");
        profile.push_str("(deny signal (target others))\n");
        profile.push_str("(deny mach-priv-task-port)\n");
    }

    let Some(profile_path) = secure_temp_profile(&profile) else {
        if let Some(path) = &scratch {
            let _ = std::fs::remove_dir(path);
        }
        return None;
    };

    // --- payload prefix: point TMPDIR at scratch ---
    // Injection and credential variables are removed by the caller before
    // sandbox-exec starts; doing that inside this shell prefix is too late.
    let env_remove = prelaunch_env_remove(state, &caps);
    let mut prefix = String::new();
    if let Some(s) = &scratch {
        let q = shell_quote(&s.display().to_string());
        prefix.push_str(&format!("export TMPDIR={q} TMP={q} TEMP={q}; "));
    }
    let full_payload = format!("{prefix}{payload_shell}");

    let command = format!(
        "{} -f {} /bin/sh -c {}",
        shell_quote(SANDBOX_EXEC_PATH),
        shell_quote(&profile_path.display().to_string()),
        shell_quote(&full_payload)
    );

    let mut cleanup = vec![profile_path];
    if let Some(s) = scratch {
        cleanup.push(s);
    }
    let explain = (opts.explain || opts.dry_run).then(|| explain_caps(&caps, &execs));
    Some(ConfinePlan::Sandboxed {
        command,
        cleanup,
        explain,
        env_remove,
    })
}

/// Resolve a writable-root spec to a canonical absolute path under `cwd`.
fn canonical_root(p: &str, cwd: &std::path::Path) -> Option<String> {
    let path = if std::path::Path::new(p).is_absolute() {
        std::path::PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    std::fs::canonicalize(&path)
        .ok()
        .map(|c| c.display().to_string())
}

/// A monotonic-ish unique suffix (no `Math.random`/argless `Instant`): wall-clock
/// nanos. Only used for unique temp names, guarded by `O_EXCL`/`create_dir`.
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Create the profile file with `O_EXCL` + mode 0600 in the temp dir (no symlink
/// following, fails if a file is already there) — closes the v1 TOCTOU.
fn secure_temp_profile(content: &str) -> Option<std::path::PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let base = std::env::temp_dir();
    for _ in 0..16 {
        let path = base.join(format!(
            "agsh-confine-{}-{}.sb",
            std::process::id(),
            unique_suffix()
        ));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(content.as_bytes()).ok()?;
                return Some(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Create a fresh private 0700 scratch dir under the temp base, canonicalized.
fn make_scratch() -> Option<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;

    let base = std::env::temp_dir();
    for _ in 0..16 {
        let path = base.join(format!(
            "agsh-scratch-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => match std::fs::canonicalize(&path) {
                Ok(canonical) => return Some(canonical),
                Err(_) => {
                    let _ = std::fs::remove_dir(&path);
                    return None;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        }
    }
    None
}

/// A human summary of the resolved capabilities (for `--explain` / `--dry-run`).
fn explain_caps(caps: &Caps, execs: &std::collections::BTreeSet<String>) -> String {
    let write = match &caps.write {
        WritePolicy::Open => "anywhere".to_string(),
        WritePolicy::DenyAll => "scratch only".to_string(),
        WritePolicy::DenyExcept(r) => format!("scratch + {}", r.join(", ")),
    };
    format!(
        "confine: exec={} binaries | write={} | network={} | secret-reads={} | env={}\n",
        execs.len(),
        write,
        if caps.net_deny { "denied" } else { "allowed" },
        if caps.deny_secrets {
            "denied"
        } else {
            "allowed"
        },
        if caps.scrub_env {
            "scrubbed"
        } else {
            "inherited"
        },
    )
}

/// Encode a string as an SBPL (Seatbelt) double-quoted literal, escaping `\` and
/// `"` so attacker-influenced paths cannot break out of `(literal "…")`.
fn sbpl_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ShellState {
        ShellState::from_current_process()
    }

    fn toks(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn refuses_known_agents_without_force() {
        let s = state();
        match plan(
            &s,
            &["ls".to_string()],
            &toks(&["claude"]),
            "claude",
            &ConfineOpts::default(),
        ) {
            ConfinePlan::Refuse { message, code } => {
                assert_eq!(code, 2);
                assert!(message.contains("self-managing agent"));
                assert!(message.contains("allowedTools"));
            }
            _ => panic!("expected refusal for an agent"),
        }
    }

    #[test]
    fn refuses_wrapped_and_uppercase_agents() {
        let s = state();
        let refused = |t: &[&str]| {
            matches!(
                plan(
                    &s,
                    &["ls".to_string()],
                    &toks(t),
                    "x",
                    &ConfineOpts::default()
                ),
                ConfinePlan::Refuse { .. }
            )
        };
        assert!(refused(&["env", "claude"]), "env claude");
        assert!(refused(&["nice", "-n5", "claude"]), "nice claude");
        assert!(refused(&["FOO=bar", "claude"]), "VAR=x claude");
        assert!(refused(&["Claude"]), "case-insensitive");
        assert!(refused(&["timeout", "5", "cursor"]), "timeout cursor");
    }

    #[test]
    fn force_bypasses_agent_refusal() {
        let s = state();
        let opts = ConfineOpts {
            force: true,
            ..Default::default()
        };
        // With --force it proceeds to backend selection (not a refusal-for-agent).
        match plan(
            &s,
            &["ls".to_string()],
            &toks(&["claude", "--foo"]),
            "claude --foo",
            &opts,
        ) {
            ConfinePlan::Refuse { message, .. } => {
                assert!(
                    !message.contains("self-managing agent"),
                    "still refused as agent"
                );
            }
            ConfinePlan::Sandboxed { cleanup, .. } => {
                for path in cleanup {
                    let _ = if path.is_dir() {
                        std::fs::remove_dir_all(path)
                    } else {
                        std::fs::remove_file(path)
                    };
                }
            }
            ConfinePlan::BestEffort => {}
        }
    }

    #[test]
    fn sbpl_escaping_neutralizes_quotes_and_backslashes() {
        assert_eq!(sbpl_string("/bin/ls"), "\"/bin/ls\"");
        assert_eq!(sbpl_string("/a\"b"), "\"/a\\\"b\"");
        assert_eq!(sbpl_string("/a\\b"), "\"/a\\\\b\"");
    }

    #[test]
    fn shell_quote_neutralizes_substitution() {
        // The wrapper is re-parsed by agsh; $()/backtick must stay inert.
        let q = shell_quote("/tmp/a$(touch /tmp/pwn)b");
        assert_eq!(q, "'/tmp/a$(touch /tmp/pwn)b'");
        assert!(!q.contains('"')); // never double-quoted (would re-expand)
    }

    #[test]
    fn parse_spec_rejects_conflicting_security_options() {
        assert!(parse_spec(&toks(&["read-only", "workspace"])).is_err());
        assert!(parse_spec(&toks(&["--preset", "exec", "read-only"])).is_err());
        assert!(parse_spec(&toks(&["--net", "--no-net"])).is_err());
        assert!(parse_spec(&toks(&["offline", "--net"])).is_err());

        assert!(parse_spec(&toks(&["read-only", "readonly", "--no-net"])).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn executable_resolution_rejects_directories_and_non_executable_files() {
        let dir = std::env::temp_dir().join(format!(
            "agsh-confine-resolve-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("plain");
        std::fs::write(&file, b"#!/bin/sh\n").unwrap();
        assert!(resolve(dir.to_str().unwrap(), "").is_none());
        assert!(resolve(file.to_str().unwrap(), "").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sandbox_wrapper_uses_the_absolute_detected_launcher() {
        let mut s = state();
        // A hostile PATH entry must not affect the launcher chosen after the
        // fixed-path backend probe has succeeded.
        s.set_var("PATH", "/tmp/agsh-hostile-path");
        let plan = sandbox_exec_wrap(
            &s,
            &[],
            "/bin/echo",
            "/bin/echo ok",
            &ConfineOpts::default(),
        )
        .expect("build sandbox plan");
        let ConfinePlan::Sandboxed {
            command, cleanup, ..
        } = plan
        else {
            panic!("expected sandboxed plan");
        };
        for path in cleanup {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }

        assert!(
            command.starts_with("'/usr/bin/sandbox-exec' -f "),
            "sandbox launcher must be absolute, got {command:?}"
        );
        assert!(!command.starts_with("sandbox-exec "));
    }

    #[test]
    fn agent_detection_matches_basename_and_config() {
        let mut s = state();
        assert!(is_self_managing_agent("/opt/bin/claude", &s));
        assert!(!is_self_managing_agent("ls", &s));
        s.export_var("AGSH_CONFINE_AGENTS", "mytool");
        assert!(is_self_managing_agent("mytool", &s));
        s.export_var("AGSH_CONFINE_ALLOW_AGENTS", "claude");
        assert!(!is_self_managing_agent("claude", &s));
    }

    #[test]
    fn shebang_detection_is_bounded_and_finds_env_target() {
        let dir = std::env::temp_dir().join(format!("agsh-confine-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("s.sh");
        std::fs::write(&script, "#!/bin/bash\necho hi\n").unwrap();
        assert_eq!(
            shebang_commands(script.to_str().unwrap()),
            vec!["/bin/bash"]
        );
        std::fs::write(&script, "#!/usr/bin/env -S python3 -O\nprint('ok')\n").unwrap();
        assert_eq!(
            shebang_commands(script.to_str().unwrap()),
            vec!["/usr/bin/env", "python3"]
        );
        std::fs::write(&script, "#!/usr/bin/env --unknown /bin/rm python3\n").unwrap();
        assert_eq!(
            shebang_commands(script.to_str().unwrap()),
            vec!["/usr/bin/env"]
        );

        let mut oversized = b"#!/bin/sh ".to_vec();
        oversized.extend(std::iter::repeat_n(b'x', 1024 * 1024));
        std::fs::write(&script, oversized).unwrap();
        assert_eq!(shebang_commands(script.to_str().unwrap()), vec!["/bin/sh"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sandbox_profile_grants_only_platform_shell_runtime_not_env_backdoor() {
        let mut s = state();
        s.export_var("AGSH_CONFINE_RUNTIME", "/bin/cat");
        let plan = sandbox_exec_wrap(
            &s,
            &[],
            "/bin/echo",
            "/bin/echo ok",
            &ConfineOpts::default(),
        )
        .expect("build sandbox plan");
        let ConfinePlan::Sandboxed { cleanup, .. } = plan else {
            panic!("expected sandboxed plan");
        };
        let profile_path = cleanup.first().expect("profile path");
        let profile = std::fs::read_to_string(profile_path).unwrap();
        assert!(profile.contains("/bin/sh"));
        assert_eq!(profile.contains("/bin/bash"), cfg!(target_os = "macos"));
        assert!(!profile.contains("/bin/cat"));
        for path in cleanup {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    }

    #[test]
    fn named_presets_remove_injection_and_generic_secret_env_before_launch() {
        let mut s = state();
        s.export_var("DYLD_INSERT_LIBRARIES", "/tmp/evil.dylib");
        s.export_var("DYLD_PRINT_TO_FILE", "/tmp/loader-output");
        s.export_var("SERVICE_PASSWORD", "secret");
        s.export_var("MY_PRIVATE_TOKEN", "secret");
        s.export_var("TOKENIZERS_PARALLELISM", "true");
        let caps = Caps::resolve(
            &ConfineOpts {
                preset: Preset::ReadOnly,
                ..Default::default()
            },
            s.cwd(),
        );
        let removed = prelaunch_env_remove(&s, &caps);
        assert!(removed.iter().any(|name| name == "DYLD_INSERT_LIBRARIES"));
        assert!(removed.iter().any(|name| name == "DYLD_PRINT_TO_FILE"));
        assert!(removed.iter().any(|name| name == "SERVICE_PASSWORD"));
        assert!(removed.iter().any(|name| name == "MY_PRIVATE_TOKEN"));
        assert!(!removed.iter().any(|name| name == "TOKENIZERS_PARALLELISM"));

        let plan = sandbox_exec_wrap(
            &s,
            &[],
            "/bin/echo",
            "/bin/echo ok",
            &ConfineOpts {
                preset: Preset::ReadOnly,
                ..Default::default()
            },
        )
        .expect("build sandbox plan");
        let ConfinePlan::Sandboxed {
            env_remove,
            cleanup,
            ..
        } = plan
        else {
            panic!("expected sandboxed plan");
        };
        assert!(env_remove
            .iter()
            .any(|name| name == "DYLD_INSERT_LIBRARIES"));
        assert!(env_remove.iter().any(|name| name == "DYLD_PRINT_TO_FILE"));
        assert!(env_remove.iter().any(|name| name == "SERVICE_PASSWORD"));
        for path in cleanup {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }

        let open = Caps::resolve(&ConfineOpts::default(), s.cwd());
        assert!(prelaunch_env_remove(&s, &open).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scratch_directory_is_private_at_creation() {
        use std::os::unix::fs::PermissionsExt as _;

        let scratch = make_scratch().expect("create scratch");
        let mode = std::fs::metadata(&scratch).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::remove_dir(scratch).unwrap();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn unsupported_platform_refuses_unless_downgrade_is_explicit() {
        let s = state();
        let opts = ConfineOpts {
            preset: Preset::ReadOnly,
            ..Default::default()
        };
        assert_eq!(detect_backend(), Backend::Unavailable);
        match plan(&s, &[], &toks(&["echo", "ok"]), "echo ok", &opts) {
            ConfinePlan::Refuse { message, code } => {
                assert_eq!(code, 2);
                assert!(message.contains("cannot enforce"));
            }
            _ => panic!("unsupported backend must fail closed"),
        }

        let downgraded = ConfineOpts {
            best_effort: true,
            ..opts
        };
        assert!(matches!(
            plan(&s, &[], &toks(&["echo", "ok"]), "echo ok", &downgraded),
            ConfinePlan::BestEffort
        ));
    }
}
