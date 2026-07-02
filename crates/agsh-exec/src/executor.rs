use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use agsh_compat::{CommandResolution, Resolver};
use agsh_core::{
    lexer::lex, parse_line, Assignment, CommandGraph, CommandInvocation, CommandListItem,
    ListOperator, Pipeline, QuoteKind, RedirectionMode, RedirectionTarget, ShellError,
    ShellErrorKind, Value, WordSegment,
};
use agsh_output::{
    render_observation, render_observation_with, CompactionContext, OutputMode, OutputObservation,
};

use crate::builtins::{is_builtin, run_builtin};
use crate::state::{BufferedStdin, LoopControlKind, StreamingStdin, StreamingStdout};
use crate::{ShellFunction, ShellState};

#[derive(Debug, Clone)]
pub struct ExecutionOptions {
    pub output_mode: OutputMode,
    pub allow_process_replacement: bool,
}

impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            output_mode: OutputMode::Raw,
            allow_process_replacement: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommandOutcome {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub observation: Option<OutputObservation>,
}

impl CommandOutcome {
    pub fn captured(exit_code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        Self {
            exit_code,
            stdout,
            stderr,
            observation: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct Executor {
    /// When true (top-level interactive/`-c` execution), each command's output
    /// is flushed to the real stdout/stderr in order in raw mode, so builtin
    /// and external output interleave correctly. Nested executors leave this
    /// off and accumulate into their outcome.
    flush_stdout: bool,
}

#[derive(Debug, Clone)]
struct ExpandedInvocation {
    assignments: Vec<Assignment>,
    argv: Vec<String>,
    redirections: Vec<ExpandedRedirection>,
}

#[derive(Debug, Clone)]
struct ExpandedRedirection {
    fd: u8,
    mode: RedirectionMode,
    target: ExpandedRedirectionTarget,
}

#[derive(Debug, Clone)]
struct ResolvedExternalInvocation {
    invocation: ExpandedInvocation,
    path: PathBuf,
}

enum ResolvedStreamingStage {
    External(ResolvedExternalInvocation),
    Shell(CommandInvocation),
}

enum RunningStreamingStage {
    External(Child),
    Shell(std::thread::JoinHandle<Result<CommandOutcome, ShellError>>),
}

enum ExternalStageStdin {
    Inherit,
    Null,
    Pipe(io::PipeReader),
}

impl ExternalStageStdin {
    fn into_stdio(self) -> Stdio {
        match self {
            Self::Inherit => Stdio::inherit(),
            Self::Null => Stdio::null(),
            Self::Pipe(reader) => Stdio::from(reader),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamingPipeKind {
    Stdout,
    Stderr,
}

#[derive(Debug)]
enum StreamingOutputTarget {
    File(File),
    Null,
    Pipe {
        kind: StreamingPipeKind,
        writer: io::PipeWriter,
    },
}

impl StreamingOutputTarget {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::File(file) => Ok(Self::File(file.try_clone()?)),
            Self::Null => Ok(Self::Null),
            Self::Pipe { kind, writer } => Ok(Self::Pipe {
                kind: *kind,
                writer: writer.try_clone()?,
            }),
        }
    }

    fn pipe_kind(&self) -> Option<StreamingPipeKind> {
        match self {
            Self::Null | Self::File(_) => None,
            Self::Pipe { kind, .. } => Some(*kind),
        }
    }

    fn into_stdio(self) -> Stdio {
        match self {
            Self::Null => Stdio::null(),
            Self::File(file) => Stdio::from(file),
            Self::Pipe { writer, .. } => Stdio::from(writer),
        }
    }
}

#[derive(Debug)]
struct StreamingOutputReaders {
    stdout: Option<io::PipeReader>,
    stderr: Option<io::PipeReader>,
}

#[derive(Debug, Clone)]
struct PositionalSnapshot {
    args: Vec<String>,
    at_was_set: bool,
}

#[derive(Debug, Clone)]
struct IfBlock {
    clauses: Vec<IfClause>,
    else_body: Option<String>,
}

#[derive(Debug, Clone)]
struct IfClause {
    condition: String,
    body: String,
}

#[derive(Debug, Clone)]
struct WhileBlock {
    kind: ConditionLoopKind,
    condition: String,
    body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionLoopKind {
    While,
    Until,
}

impl ConditionLoopKind {
    fn keyword(self) -> &'static str {
        match self {
            Self::While => "while",
            Self::Until => "until",
        }
    }

    fn should_run_body(self, condition_status: i32) -> bool {
        match self {
            Self::While => condition_status == 0,
            Self::Until => condition_status != 0,
        }
    }
}

#[derive(Debug, Clone)]
struct ForBlock {
    variable: String,
    items: ForItems,
    body: String,
    /// For a C-style `for ((init; cond; step))`: the three arithmetic clauses.
    arithmetic: Option<(String, String, String)>,
}

#[derive(Debug, Clone)]
struct SelectBlock {
    variable: String,
    items: ForItems,
    body: String,
}

#[derive(Debug, Clone)]
enum ForItems {
    Words(Vec<Vec<WordSegment>>),
    Positionals,
}

#[derive(Debug, Clone)]
struct CaseBlock {
    value: Vec<WordSegment>,
    arms: Vec<CaseArm>,
}

#[derive(Debug, Clone)]
struct CaseArm {
    patterns: Vec<Vec<WordSegment>>,
    body: String,
    terminator: CaseTerminator,
}

#[derive(Debug, Clone)]
struct IfWord {
    text: String,
    source: String,
    quote: QuoteKind,
}

#[derive(Debug, Clone)]
enum ExpandedRedirectionTarget {
    Path(String),
    Fd(u8),
    Close,
    /// Literal bytes to feed to stdin (heredocs and herestrings).
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LookupMode {
    Normal,
    BypassAliases,
    DefaultPath,
    ExternalOnly,
    BuiltinOnly,
}

#[derive(Debug, Clone, Copy)]
struct InvocationContext {
    lookup_mode: LookupMode,
    alias_depth: usize,
    allow_process_replacement: bool,
}

const MAX_ALIAS_EXPANSIONS: usize = 16;
const DEFAULT_COMMAND_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

impl Executor {
    pub fn new() -> Self {
        Self {
            flush_stdout: false,
        }
    }

    /// Enable immediate stdout/stderr flushing for top-level execution.
    pub fn with_stdout_flush(mut self, enabled: bool) -> Self {
        self.flush_stdout = enabled;
        self
    }

    pub fn run_graph(
        &mut self,
        graph: &CommandGraph,
        state: &mut ShellState,
        options: &ExecutionOptions,
    ) -> Result<CommandOutcome, ShellError> {
        // Bound total execution nesting here — the single chokepoint every
        // nested path re-enters (functions, `$( )`, `<( )`, subshells, brace
        // groups, `eval`, `source`, loop/compound bodies). Guards against
        // runaway recursion (`f() { f; }`, deeply nested `$( … )`) that would
        // otherwise overflow the stack and abort the whole shell.
        state.enter_exec()?;
        let result = self.run_graph_inner(graph, state, options);
        state.leave_exec();
        let outcome = result?;
        state.set_last_status(outcome.exit_code);
        Ok(outcome)
    }

    fn run_graph_inner(
        &mut self,
        graph: &CommandGraph,
        state: &mut ShellState,
        options: &ExecutionOptions,
    ) -> Result<CommandOutcome, ShellError> {
        if graph.is_empty() {
            return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
        }

        if graph.list.items.len() > 1 {
            return run_command_list(graph, state, options, self.flush_stdout);
        }

        let pipeline = graph
            .list
            .items
            .first()
            .map(|item| &item.pipeline)
            .unwrap_or(&graph.pipeline);

        let mut outcome = run_pipeline_item(graph, pipeline, state, options)?;
        // Stream this command's stdout to a downstream pipe if one is active (a
        // pipeline stage). Without this, a single-command loop body (`while true;
        // do echo x; done | head`) would accumulate its output and flush only at
        // stage end — never streaming — so an early-exiting consumer couldn't stop
        // an infinite producer. Multi-command lists already emit per command in
        // run_command_list. No-op when not streaming. (P0-8)
        emit_streaming_stdout(state, &mut outcome)?;
        if options.output_mode.should_capture() && outcome.observation.is_none() {
            state.record_trace(
                &graph.id,
                &graph.source,
                outcome.exit_code,
                &outcome.stdout,
                &outcome.stderr,
            );
            let argv = graph_primary_argv(graph);
            outcome.observation = if options.output_mode == OutputMode::Rich {
                rich_observation(state, &graph.id, &argv, &outcome.stdout, &outcome.stderr)
            } else {
                Some(render_observation_with(
                    &compaction_context(state, &argv),
                    options.output_mode,
                    &graph.id,
                    &argv,
                    outcome.exit_code,
                    &outcome.stdout,
                    &outcome.stderr,
                ))
            };
        }
        Ok(outcome)
    }
}

/// Fire any pending signal traps (POSIX 2.11) and merge their output into the
/// just-completed command's outcome. Run at command boundaries.
fn fire_signal_traps(
    state: &mut ShellState,
    options: &ExecutionOptions,
    outcome: &mut CommandOutcome,
) -> Result<(), ShellError> {
    let mut fired = false;
    for signal in state.take_pending_signal_traps() {
        if let Some(action) = state.trap_action(&signal) {
            fired = true;
            let mut trap_outcome = run_command_source(&action, state, options)?;
            outcome.stdout.append(&mut trap_outcome.stdout);
            outcome.stderr.append(&mut trap_outcome.stderr);
        }
    }
    // A trapped signal does not take its default action: clear the interrupt so
    // execution continues after the handler (POSIX 2.11).
    if fired {
        state.clear_interrupt();
    }
    Ok(())
}

fn run_command_list(
    graph: &CommandGraph,
    state: &mut ShellState,
    options: &ExecutionOptions,
    flush_stdout: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let mut last_status = 0;

    for (index, item) in graph.list.items.iter().enumerate() {
        let should_run = match item.operator {
            ListOperator::Always => true,
            ListOperator::And => last_status == 0,
            ListOperator::Or => last_status != 0,
        };
        if !should_run {
            continue;
        }

        // Track the source line of this command for `$LINENO`.
        if let Some(start) = item
            .pipeline
            .commands
            .first()
            .and_then(|c| c.span)
            .map(|s| s.start)
        {
            let line = 1 + graph.source[..start.min(graph.source.len())]
                .bytes()
                .filter(|&b| b == b'\n')
                .count();
            state.set_current_line(line as u32);
        }

        if item.background {
            let mut outcome = run_background_item(graph, item, state)?;
            last_status = outcome.exit_code;
            state.set_last_status(last_status);
            final_outcome.exit_code = outcome.exit_code;
            if flush_stdout && !options.output_mode.should_capture() {
                pipe_ok(io::stdout().write_all(&outcome.stdout))?;
                pipe_ok(io::stdout().flush())?;
                pipe_ok(io::stderr().write_all(&outcome.stderr))?;
                outcome.stdout.clear();
                outcome.stderr.clear();
            }
            final_outcome.stdout.append(&mut outcome.stdout);
            final_outcome.stderr.append(&mut outcome.stderr);
            continue;
        }

        let mut outcome = run_pipeline_item(graph, &item.pipeline, state, options)?;
        last_status = outcome.exit_code;
        state.set_last_status(last_status);
        // $PIPESTATUS for a single command is a one-element array (multi-stage
        // pipelines record it themselves in run_pipeline).
        if item.pipeline.commands.len() == 1 {
            record_pipestatus(state, &[outcome.exit_code]);
        }
        fire_signal_traps(state, options, &mut outcome)?;
        emit_streaming_stdout(state, &mut outcome)?;
        final_outcome.exit_code = outcome.exit_code;
        // In raw (non-capturing) mode, flush each command's output immediately so
        // builtin output interleaves correctly with external commands instead of
        // being buffered until the end of the list.
        if flush_stdout && !options.output_mode.should_capture() && state.streaming_stdout_is_none()
        {
            pipe_ok(io::stdout().write_all(&outcome.stdout))?;
            pipe_ok(io::stdout().flush())?;
            pipe_ok(io::stderr().write_all(&outcome.stderr))?;
            outcome.stdout.clear();
            outcome.stderr.clear();
        }
        final_outcome.stdout.append(&mut outcome.stdout);
        final_outcome.stderr.append(&mut outcome.stderr);

        if state.should_exit()
            || state.loop_control_requested()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
        {
            break;
        }

        let next_operator = graph
            .list
            .items
            .get(index + 1)
            .map(|next| next.operator)
            .unwrap_or(ListOperator::Always);
        let failure_is_boolean_operand =
            matches!(next_operator, ListOperator::And | ListOperator::Or);

        // ERR trap (POSIX/bash): fires when a command fails, except as a boolean
        // operand of `&&`/`||`. Fired before any errexit exit.
        if outcome.exit_code != 0 && !failure_is_boolean_operand {
            if let Some(action) = state.trap_action("ERR") {
                state.set_trap("ERR", None); // prevent recursion in the handler
                if let Ok(mut handler) = run_command_source(&action, state, options) {
                    if flush_stdout
                        && !options.output_mode.should_capture()
                        && state.streaming_stdout_is_none()
                    {
                        pipe_ok(io::stdout().write_all(&handler.stdout))?;
                        pipe_ok(io::stdout().flush())?;
                        pipe_ok(io::stderr().write_all(&handler.stderr))?;
                    } else {
                        final_outcome.stdout.append(&mut handler.stdout);
                        final_outcome.stderr.append(&mut handler.stderr);
                    }
                }
                state.set_trap("ERR", Some(action));
                state.set_last_status(last_status);
            }
        }

        if state.errexit() && outcome.exit_code != 0 && !failure_is_boolean_operand {
            break;
        }
    }

    if options.output_mode.should_capture() {
        state.record_trace(
            &graph.id,
            &graph.source,
            final_outcome.exit_code,
            &final_outcome.stdout,
            &final_outcome.stderr,
        );
        let argv = graph_primary_argv(graph);
        final_outcome.observation = if options.output_mode == OutputMode::Rich {
            rich_observation(
                state,
                &graph.id,
                &argv,
                &final_outcome.stdout,
                &final_outcome.stderr,
            )
        } else {
            Some(render_observation_with(
                &compaction_context(state, &argv),
                options.output_mode,
                &graph.id,
                &argv,
                final_outcome.exit_code,
                &final_outcome.stdout,
                &final_outcome.stderr,
            ))
        };
    }

    Ok(final_outcome)
}

/// Reconstruct the source text of one command-list item from the original line
/// using its command spans, falling back to rebuilding from the parsed words.
fn item_source(graph: &CommandGraph, item: &CommandListItem) -> String {
    let commands = &item.pipeline.commands;
    let start = commands.first().and_then(|c| c.span).map(|s| s.start);
    let end = commands.last().and_then(|c| c.span).map(|s| s.end);
    if let (Some(start), Some(end)) = (start, end) {
        if start <= end && end <= graph.source.len() {
            let slice = graph.source[start..end].trim();
            if !slice.is_empty() {
                return slice.to_string();
            }
        }
    }
    commands
        .iter()
        .map(|command| {
            let words = if_words(command);
            words_to_source(&words)
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Run a command-list item asynchronously: launch it as a child `agsh -c`
/// process that leads its own process group, register it as a job, and return
/// immediately with a `[id] pid` notice (matching shell behavior). Running a
/// child shell uniformly backgrounds builtins, compounds, and pipelines and
/// keeps the parent's state isolated (like a subshell).
fn run_background_item(
    graph: &CommandGraph,
    item: &CommandListItem,
    state: &ShellState,
) -> Result<CommandOutcome, ShellError> {
    let source = item_source(graph, item);
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command.arg("-c").arg(&source);
    command.current_dir(state.cwd());
    command.env_clear();
    command.envs(state.exported_env());
    command.process_group(0);
    // A background job does not read the terminal; its output still appears.
    command.stdin(Stdio::null());
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());

    let child = command.spawn()?;
    let pid = child.id();
    state.set_last_bg_pid(pid);
    let (id, _pgid) = state.register_job(child, source);
    Ok(CommandOutcome::captured(
        0,
        Vec::new(),
        format!("[{id}] {pid}\n").into_bytes(),
    ))
}

/// Treat a closed downstream pipe (SIGPIPE) as success when emitting output, so a
/// shell whose stdout is consumed by `head`/`less`/`grep -q` exits silently like
/// bash instead of printing a spurious "Broken pipe".
fn pipe_ok(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn emit_streaming_stdout(
    state: &mut ShellState,
    outcome: &mut CommandOutcome,
) -> Result<(), ShellError> {
    if outcome.stdout.is_empty() {
        return Ok(());
    }

    if let Some(result) = state.write_shell_stdout(&outcome.stdout) {
        match result {
            Ok(()) => {}
            // Downstream pipe closed (`… | head`): flag it so an enclosing
            // loop/list stops producing (SIGPIPE-like), rather than erroring.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                state.set_stream_pipe_closed();
            }
            Err(error) => return Err(error.into()),
        }
        outcome.stdout.clear();
    }

    Ok(())
}

/// Run one pipeline item, converting runtime I/O failures (failed `cd`,
/// failed redirection open, etc.) into a non-zero outcome so the command list
/// continues instead of aborting — matching POSIX shell behavior.
fn run_pipeline_item(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    match run_pipeline_item_inner(graph, pipeline, state, options) {
        Ok(outcome) => Ok(outcome),
        // A write to a downstream pipe that closed early (`… | head`, `… | grep -q`)
        // is a normal SIGPIPE, not an error: bash exits the producer silently. Emit
        // nothing, and flag the closed pipe so an enclosing loop/list stops
        // producing (P0-8) instead of iterating against a dead consumer.
        Err(error) if error.kind == ShellErrorKind::BrokenPipe => {
            state.set_stream_pipe_closed();
            Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()))
        }
        Err(error) if error.kind == ShellErrorKind::Io => Ok(CommandOutcome::captured(
            1,
            Vec::new(),
            format!("agsh: {}\n", error.message).into_bytes(),
        )),
        // A missing command in a pipeline stage yields exit 127 and the list
        // continues, matching POSIX shells.
        Err(error) if error.kind == ShellErrorKind::NotFound => Ok(CommandOutcome::captured(
            127,
            Vec::new(),
            format!("agsh: {}\n", error.message).into_bytes(),
        )),
        // A command refused by a `confine` allowlist: exit 126, list continues.
        Err(error) if error.kind == ShellErrorKind::Policy => Ok(CommandOutcome::captured(
            126,
            Vec::new(),
            format!("agsh: {}\n", error.message).into_bytes(),
        )),
        Err(error) => Err(error),
    }
}

fn run_pipeline_item_inner(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    if pipeline.commands.is_empty() {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }

    if pipeline.commands.len() == 1 {
        if let Some(if_block) = parse_if_invocation(&pipeline.commands[0])? {
            let mut outcome = run_if_invocation(
                &if_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(while_block) = parse_while_invocation(&pipeline.commands[0])? {
            let mut outcome = run_while_invocation(
                &while_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(for_block) = parse_for_invocation(&pipeline.commands[0])? {
            let mut outcome = run_for_invocation(
                &for_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(select_block) = parse_select_invocation(&pipeline.commands[0])? {
            let mut outcome = run_select_invocation(
                &select_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(case_block) = parse_case_invocation(&pipeline.commands[0])? {
            let mut outcome = run_case_invocation(
                &case_block,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(inner) = parse_subshell_invocation(&pipeline.commands[0])? {
            let mut outcome = run_subshell_invocation(
                &inner,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if let Some(inner) = parse_brace_group_invocation(&pipeline.commands[0])? {
            let mut outcome = run_brace_group_invocation(
                &inner,
                &pipeline.commands[0],
                state,
                options.output_mode,
                options.output_mode.should_capture(),
                options.allow_process_replacement,
            )?;
            apply_pipeline_negation(&mut outcome, pipeline.negated);
            return Ok(outcome);
        }

        if !pipeline.negated {
            if let Some((name, function)) = parse_function_definition(&pipeline.commands[0])? {
                state.set_function(name, function);
                return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
            }
        }

        let invocation = expand_invocation(&pipeline.commands[0], state)?;
        if invocation.argv.is_empty() {
            apply_shell_assignments(&invocation.assignments, state);
            // A redirection with no command still opens/truncates the target
            // (e.g. `> file`).
            let mut outcome = CommandOutcome::captured(
                state.last_command_substitution_status(),
                Vec::new(),
                Vec::new(),
            );
            apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
            return Ok(outcome);
        }

        let mut outcome = run_invocation(
            &invocation,
            state,
            options.output_mode,
            None,
            options.output_mode.should_capture(),
            LookupMode::Normal,
            options.allow_process_replacement,
        )?;
        apply_pipeline_negation(&mut outcome, pipeline.negated);
        return Ok(outcome);
    }

    run_pipeline(graph, pipeline, state, options)
}

fn apply_shell_assignments(assignments: &[Assignment], state: &mut ShellState) {
    for assignment in assignments {
        apply_assignment(state, assignment);
    }
}

/// Apply one assignment, handling array literals (`a=(x y z)`), element
/// assignment (`a[i]=v`), append (`+=`), and plain scalars.
fn apply_assignment(state: &mut ShellState, assignment: &Assignment) {
    let mut name = assignment.name.as_str();
    let append = name.ends_with('+');
    if append {
        name = &name[..name.len() - 1];
    }

    // Readonly enforcement (POSIX): refuse to reassign; report and fail.
    let base_name = name.split('[').next().unwrap_or(name);
    if state.is_readonly(base_name) {
        eprintln!("agsh: {base_name}: readonly variable");
        state.set_command_substitution_status(1);
        return;
    }

    // Subscript element: `a[index]=value`.
    if let Some(open) = name.find('[') {
        if let Some(close) = name.rfind(']') {
            let base = &name[..open];
            let sub = &name[open + 1..close];
            if state.is_assoc(base) {
                // Associative key: literal string (negative arith handled below
                // only for indexed arrays).
                state.set_assoc_element(base, sub.to_string(), assignment.value.clone(), append);
                return;
            }
            let raw = eval_arithmetic(sub, state).unwrap_or(0);
            let len = state.array(base).map(<[String]>::len).unwrap_or(0) as i64;
            let index = if raw < 0 { (len + raw).max(0) } else { raw } as usize;
            state.set_array_element(base, index, assignment.value.clone(), append);
            return;
        }
    }

    // Array literal: an unquoted `( ... )` value.
    let raw = assignment
        .value_segments
        .first()
        .filter(|s| s.quote == agsh_core::QuoteKind::None)
        .map(|s| s.text.as_str())
        .unwrap_or("");
    if raw.starts_with('(') && raw.ends_with(')') && assignment.value.starts_with('(') {
        let inner = &raw[1..raw.len() - 1];
        let elements = expand_array_elements(inner, state);
        if state.is_assoc(name) {
            state.set_assoc(name, assoc_pairs_from_elements(&elements), append);
        } else {
            state.set_array(name, elements, append);
        }
        return;
    }

    // Scalar (with optional append).
    let value = if append {
        format!(
            "{}{}",
            state.lookup(name).unwrap_or_default(),
            assignment.value
        )
    } else {
        assignment.value.clone()
    };
    if state.allexport() {
        state.export_var(name, value);
    } else {
        state.set_var(name, value);
    }
}

/// Expand the inner text of an array literal into elements, applying full
/// per-word expansion (parameter/command substitution, word-splitting, globbing).
/// Expand an array subscript expression (`a[i]`, `a[@]`, `#a[@]`, `!a[@]`, …) to
/// a string, or None when `expr` is not an array subscript of a known array.
/// `${a[@]}`/`${a[*]}` join elements with a space (quoted-element-with-spaces
/// preservation is a known limitation of the current return-string model).
fn expand_array_subscript(expr: &str, state: &mut ShellState) -> Option<String> {
    let (mode, rest) = match expr.chars().next() {
        Some('#') => ('#', &expr[1..]),
        Some('!') => ('!', &expr[1..]),
        _ => (' ', expr),
    };
    let open = rest.find('[')?;
    let close = rest.rfind(']')?;
    if close + 1 != rest.len() {
        return None;
    }
    let base = &rest[..open];
    let sub = &rest[open + 1..close];
    if !is_identifier(base) {
        return None;
    }

    // Associative arrays: keys are strings, not arithmetic.
    if state.is_assoc(base) {
        let result = match mode {
            '#' => {
                if sub == "@" || sub == "*" {
                    state.assoc_keys(base)?.len().to_string()
                } else {
                    state
                        .assoc_get(base, sub)
                        .map(|v| v.chars().count())
                        .unwrap_or(0)
                        .to_string()
                }
            }
            '!' => {
                if sub == "@" || sub == "*" {
                    state.assoc_keys(base)?.join(" ")
                } else {
                    return None;
                }
            }
            _ => {
                if sub == "@" || sub == "*" {
                    state.assoc_values(base)?.join(" ")
                } else {
                    state.assoc_get(base, sub).unwrap_or_default().to_string()
                }
            }
        };
        return Some(result);
    }

    let array = state.array(base)?.to_vec();
    let resolve = |raw: i64| -> Option<usize> {
        // Negative indices count from the end (bash 4.3): a[-1] is the last.
        let idx = if raw < 0 {
            array.len() as i64 + raw
        } else {
            raw
        };
        (idx >= 0).then_some(idx as usize)
    };
    let result = match mode {
        '#' => {
            if sub == "@" || sub == "*" {
                array.len().to_string()
            } else {
                let index = resolve(eval_arithmetic(sub, state).ok()?);
                index
                    .and_then(|i| array.get(i))
                    .map(|s| s.chars().count())
                    .unwrap_or(0)
                    .to_string()
            }
        }
        '!' => {
            if sub == "@" || sub == "*" {
                (0..array.len())
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                return None;
            }
        }
        _ => {
            if sub == "@" || sub == "*" {
                array.join(" ")
            } else {
                resolve(eval_arithmetic(sub, state).ok()?)
                    .and_then(|i| array.get(i))
                    .cloned()
                    .unwrap_or_default()
            }
        }
    };
    Some(result)
}

/// Build associative key/value pairs from expanded literal elements: `[k]=v`
/// form, or alternating `k v k2 v2`.
fn assoc_pairs_from_elements(elements: &[String]) -> Vec<(String, String)> {
    if elements.iter().any(|e| e.starts_with('[')) {
        elements
            .iter()
            .filter_map(|e| {
                e.strip_prefix('[')
                    .and_then(|r| r.split_once("]="))
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    } else {
        let mut pairs = Vec::new();
        let mut it = elements.iter();
        while let Some(k) = it.next() {
            pairs.push((k.clone(), it.next().cloned().unwrap_or_default()));
        }
        pairs
    }
}

fn expand_array_elements(inner: &str, state: &mut ShellState) -> Vec<String> {
    let Ok(tokens) = agsh_core::lexer::lex(inner) else {
        return Vec::new();
    };
    let mut elements = Vec::new();
    for token in tokens {
        let segments = if token.segments.is_empty() {
            vec![WordSegment::new(token.text.clone(), token.quote)]
        } else {
            token.segments.clone()
        };
        if let Ok(fields) = expand_word_segments_to_argv_fields(&segments, state) {
            elements.extend(fields);
        }
    }
    elements
}

fn run_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    lookup_mode: LookupMode,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    run_invocation_inner(
        invocation,
        state,
        output_mode,
        stdin_data,
        capture_outputs,
        InvocationContext {
            lookup_mode,
            alias_depth: 0,
            allow_process_replacement,
        },
    )
}

fn run_invocation_inner(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    context: InvocationContext,
) -> Result<CommandOutcome, ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    };

    match name.as_str() {
        "command" if context.lookup_mode == LookupMode::Normal => {
            let command_options = parse_command_options(invocation)?;
            if command_options.describe || command_options.unsupported_option.is_some() {
                let mut outcome = run_builtin(&to_command_invocation(invocation), state)?;
                apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
                return Ok(outcome);
            }
            let wrapped = strip_command_wrapper(invocation, command_options.command_index)?;
            run_invocation_inner(
                &wrapped,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                InvocationContext {
                    lookup_mode: if command_options.default_path {
                        LookupMode::DefaultPath
                    } else {
                        LookupMode::BypassAliases
                    },
                    ..context
                },
            )
        }
        "external" if context.lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "external")?;
            run_invocation_inner(
                &wrapped,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                InvocationContext {
                    lookup_mode: LookupMode::ExternalOnly,
                    ..context
                },
            )
        }
        "builtin" if context.lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "builtin")?;
            run_invocation_inner(
                &wrapped,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                InvocationContext {
                    lookup_mode: LookupMode::BuiltinOnly,
                    ..context
                },
            )
        }
        "agpty" | "pty"
            if context.lookup_mode == LookupMode::Normal && invocation.argv.len() > 1 =>
        {
            let wrapped = strip_wrapper(invocation, name.as_str())?;
            run_under_pty(&wrapped, state)
        }
        _ if context.lookup_mode == LookupMode::Normal => {
            if let Some(function) = state.function(name).cloned() {
                return run_function_invocation(
                    &function,
                    invocation,
                    state,
                    output_mode,
                    stdin_data,
                    capture_outputs,
                    context.allow_process_replacement,
                );
            }

            if let Some(expanded) = expand_alias_invocation(invocation, state)? {
                if context.alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: alias expansion limit exceeded"
                    )));
                }
                return run_invocation_inner(
                    &expanded,
                    state,
                    output_mode,
                    stdin_data,
                    capture_outputs,
                    InvocationContext {
                        alias_depth: context.alias_depth + 1,
                        ..context
                    },
                );
            }
            if let Some(expanded) = expand_abbreviation_invocation(invocation, state)? {
                if context.alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: abbreviation expansion limit exceeded"
                    )));
                }
                return run_invocation_inner(
                    &expanded,
                    state,
                    output_mode,
                    stdin_data,
                    capture_outputs,
                    InvocationContext {
                        alias_depth: context.alias_depth + 1,
                        ..context
                    },
                );
            }
            run_resolved_invocation(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                context.lookup_mode,
                context.allow_process_replacement,
            )
        }
        _ => run_resolved_invocation(
            invocation,
            state,
            output_mode,
            stdin_data,
            capture_outputs,
            context.lookup_mode,
            context.allow_process_replacement,
        ),
    }
}

fn parse_function_definition(
    invocation: &CommandInvocation,
) -> Result<Option<(String, ShellFunction)>, ShellError> {
    let argv = &invocation.argv;
    if argv.is_empty() {
        return Ok(None);
    }

    let (name, body_start) = if let Some(name) = argv[0].strip_suffix("()") {
        if argv.get(1).is_some_and(|word| word == "{") {
            (name.to_string(), 2)
        } else {
            return Ok(None);
        }
    } else if is_identifier(&argv[0])
        && argv.get(1).is_some_and(|word| word == "()")
        && argv.get(2).is_some_and(|word| word == "{")
    {
        // `name () { ... }` with a space before the parentheses.
        (argv[0].clone(), 3)
    } else if argv.first().is_some_and(|word| word == "function")
        && argv.get(2).is_some_and(|word| word == "{")
    {
        (argv[1].clone(), 3)
    } else {
        return Ok(None);
    };

    if !is_identifier(&name) {
        return Err(ShellError::parse(format!("invalid function name: {name}")));
    }
    if argv.last().is_none_or(|word| word != "}") || body_start >= argv.len() - 1 {
        return Err(ShellError::parse(format!(
            "{name}: malformed function definition"
        )));
    }
    if !invocation.assignments.is_empty() || !invocation.redirections.is_empty() {
        return Err(ShellError::unsupported(format!(
            "{name}: assignments or redirections on function definitions are not supported"
        )));
    }

    let body = invocation.argv_segments[body_start..argv.len() - 1]
        .iter()
        .zip(&argv[body_start..argv.len() - 1])
        .map(|(segments, fallback)| word_segments_to_source(segments, fallback))
        .collect::<Vec<_>>()
        .join(" ");

    Ok(Some((name, ShellFunction::new(body))))
}

fn parse_if_invocation(invocation: &CommandInvocation) -> Result<Option<IfBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "if" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "if: assignments before if are not supported",
        ));
    }

    let words = if_words(invocation);
    let mut clauses = Vec::new();
    let mut else_body = None;
    let mut index = 1;

    loop {
        let then_index = find_top_level_reserved(&words, index, &["then"])
            .ok_or_else(|| ShellError::parse("if: missing then"))?
            .0;
        let condition = words_to_source(&words[index..then_index]);
        if condition.trim().is_empty() {
            return Err(ShellError::parse("if: empty condition"));
        }
        index = then_index + 1;

        let (body_end, boundary) = find_top_level_reserved(&words, index, &["elif", "else", "fi"])
            .ok_or_else(|| ShellError::parse("if: missing fi"))?;
        let body = words_to_source(&words[index..body_end]);
        clauses.push(IfClause { condition, body });
        index = body_end + 1;

        match boundary.as_str() {
            "elif" => continue,
            "else" => {
                let (else_end, else_boundary) = find_top_level_reserved(&words, index, &["fi"])
                    .ok_or_else(|| ShellError::parse("if: missing fi"))?;
                if else_boundary != "fi" {
                    return Err(ShellError::parse("if: expected fi"));
                }
                else_body = Some(words_to_source(&words[index..else_end]));
                index = else_end + 1;
                break;
            }
            "fi" => break,
            _ => return Err(ShellError::parse("if: invalid branch delimiter")),
        }
    }

    if index != words.len() {
        return Err(ShellError::parse("if: unexpected tokens after fi"));
    }

    Ok(Some(IfBlock { clauses, else_body }))
}

fn parse_while_invocation(
    invocation: &CommandInvocation,
) -> Result<Option<WhileBlock>, ShellError> {
    let Some((keyword, quote)) = invocation.argv.first().zip(invocation.argv_quote.first()) else {
        return Ok(None);
    };
    let kind = match (keyword.as_str(), *quote) {
        ("while", QuoteKind::None) => ConditionLoopKind::While,
        ("until", QuoteKind::None) => ConditionLoopKind::Until,
        _ => return Ok(None),
    };
    let keyword = kind.keyword();
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(format!(
            "{keyword}: assignments before {keyword} are not supported"
        )));
    }

    let words = if_words(invocation);
    let (do_index, _) = find_top_level_reserved(&words, 1, &["do"])
        .ok_or_else(|| ShellError::parse(format!("{keyword}: missing do")))?;
    let condition = words_to_source(&words[1..do_index]);
    if condition.trim().is_empty() {
        return Err(ShellError::parse(format!("{keyword}: empty condition")));
    }

    let body_start = do_index + 1;
    let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
        .ok_or_else(|| ShellError::parse(format!("{keyword}: missing done")))?;
    let body = words_to_source(&words[body_start..done_index]);
    if done_index + 1 != words.len() {
        return Err(ShellError::parse(format!(
            "{keyword}: unexpected tokens after done"
        )));
    }

    Ok(Some(WhileBlock {
        kind,
        condition,
        body,
    }))
}

fn parse_for_invocation(invocation: &CommandInvocation) -> Result<Option<ForBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "for" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "for: assignments before for are not supported",
        ));
    }

    // C-style: `for (( init; cond; step )); do ... done`.
    if let Some(header) = invocation.argv.get(1) {
        if header.starts_with("((") && header.ends_with("))") {
            let inner = &header[2..header.len() - 2];
            let mut parts = inner.splitn(3, ';');
            let init = parts.next().unwrap_or("").trim().to_string();
            let cond = parts.next().unwrap_or("").trim().to_string();
            let step = parts.next().unwrap_or("").trim().to_string();
            let words = if_words(invocation);
            let (do_index, _) = find_top_level_reserved(&words, 2, &["do"])
                .ok_or_else(|| ShellError::parse("for: missing do"))?;
            let body_start = do_index + 1;
            let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
                .ok_or_else(|| ShellError::parse("for: missing done"))?;
            let body = words_to_source(&words[body_start..done_index]);
            return Ok(Some(ForBlock {
                variable: String::new(),
                items: ForItems::Positionals,
                body,
                arithmetic: Some((init, cond, step)),
            }));
        }
    }

    let Some(variable) = invocation.argv.get(1) else {
        return Err(ShellError::parse("for: missing variable name"));
    };
    if !invocation
        .argv_quote
        .get(1)
        .is_some_and(|quote| *quote == QuoteKind::None)
        || !is_identifier(variable)
    {
        return Err(ShellError::parse(format!(
            "for: invalid variable name: {variable}"
        )));
    }

    let words = if_words(invocation);
    let (do_index, _) = find_top_level_reserved(&words, 2, &["do"])
        .ok_or_else(|| ShellError::parse("for: missing do"))?;
    let mut header_start = 2;
    let mut header_end = do_index;
    if header_end > header_start
        && words[header_end - 1].quote == QuoteKind::None
        && words[header_end - 1].text == ";"
    {
        header_end -= 1;
    }

    let items = if header_start < header_end
        && words[header_start].quote == QuoteKind::None
        && words[header_start].text == "in"
    {
        header_start += 1;
        ForItems::Words(
            (header_start..header_end)
                .map(|index| {
                    invocation
                        .argv_segments
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| {
                            vec![WordSegment::new(
                                invocation.argv[index].clone(),
                                invocation.argv_quote[index],
                            )]
                        })
                })
                .collect(),
        )
    } else if header_start == header_end {
        ForItems::Positionals
    } else {
        return Err(ShellError::parse("for: expected in or do"));
    };

    let body_start = do_index + 1;
    let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
        .ok_or_else(|| ShellError::parse("for: missing done"))?;
    let body = words_to_source(&words[body_start..done_index]);
    if done_index + 1 != words.len() {
        return Err(ShellError::parse("for: unexpected tokens after done"));
    }

    Ok(Some(ForBlock {
        variable: variable.clone(),
        items,
        body,
        arithmetic: None,
    }))
}

fn parse_select_invocation(
    invocation: &CommandInvocation,
) -> Result<Option<SelectBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "select" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "select: assignments before select are not supported",
        ));
    }

    let Some(variable) = invocation.argv.get(1) else {
        return Err(ShellError::parse("select: missing variable name"));
    };
    if !invocation
        .argv_quote
        .get(1)
        .is_some_and(|quote| *quote == QuoteKind::None)
        || !is_identifier(variable)
    {
        return Err(ShellError::parse(format!(
            "select: invalid variable name: {variable}"
        )));
    }

    let words = if_words(invocation);
    let (do_index, _) = find_top_level_reserved(&words, 2, &["do"])
        .ok_or_else(|| ShellError::parse("select: missing do"))?;
    let mut header_start = 2;
    let mut header_end = do_index;
    if header_end > header_start
        && words[header_end - 1].quote == QuoteKind::None
        && words[header_end - 1].text == ";"
    {
        header_end -= 1;
    }

    let items = if header_start < header_end
        && words[header_start].quote == QuoteKind::None
        && words[header_start].text == "in"
    {
        header_start += 1;
        ForItems::Words(
            (header_start..header_end)
                .map(|index| {
                    invocation
                        .argv_segments
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| {
                            vec![WordSegment::new(
                                invocation.argv[index].clone(),
                                invocation.argv_quote[index],
                            )]
                        })
                })
                .collect(),
        )
    } else if header_start == header_end {
        ForItems::Positionals
    } else {
        return Err(ShellError::parse("select: expected in or do"));
    };

    let body_start = do_index + 1;
    let (done_index, _) = find_top_level_reserved(&words, body_start, &["done"])
        .ok_or_else(|| ShellError::parse("select: missing done"))?;
    let body = words_to_source(&words[body_start..done_index]);
    if done_index + 1 != words.len() {
        return Err(ShellError::parse("select: unexpected tokens after done"));
    }

    Ok(Some(SelectBlock {
        variable: variable.clone(),
        items,
        body,
    }))
}

fn parse_case_invocation(invocation: &CommandInvocation) -> Result<Option<CaseBlock>, ShellError> {
    if !invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "case" && *quote == QuoteKind::None)
    {
        return Ok(None);
    }
    if !invocation.assignments.is_empty() {
        return Err(ShellError::parse(
            "case: assignments before case are not supported",
        ));
    }

    let Some(value) = invocation.argv.get(1) else {
        return Err(ShellError::parse("case: missing word"));
    };
    let value_segments = invocation
        .argv_segments
        .get(1)
        .cloned()
        .unwrap_or_else(|| vec![WordSegment::new(value.clone(), invocation.argv_quote[1])]);
    if !invocation
        .argv
        .get(2)
        .zip(invocation.argv_quote.get(2))
        .is_some_and(|(word, quote)| word == "in" && *quote == QuoteKind::None)
    {
        return Err(ShellError::parse("case: missing in"));
    }

    let words = if_words(invocation);
    let mut arms = Vec::new();
    let mut index = 3;
    loop {
        while words
            .get(index)
            .is_some_and(|word| word.quote == QuoteKind::None && word.text == ";")
        {
            index += 1;
        }
        let Some(word) = words.get(index) else {
            return Err(ShellError::parse("case: missing esac"));
        };
        if word.quote == QuoteKind::None && word.text == "esac" {
            index += 1;
            break;
        }

        let patterns = parse_case_patterns(invocation, &words, &mut index)?;
        let body_start = index;
        let (body_end, terminator) = find_case_arm_terminator(&words, body_start)
            .ok_or_else(|| ShellError::parse("case: missing esac"))?;
        let body = words_to_source(&words[body_start..body_end]);
        arms.push(CaseArm {
            patterns,
            body,
            terminator,
        });

        match terminator {
            CaseTerminator::ArmSeparator
            | CaseTerminator::FallThrough
            | CaseTerminator::PatternContinue => index = body_end + terminator.token_count(),
            CaseTerminator::Esac => {
                index = body_end + terminator.token_count();
                break;
            }
        }
    }

    if index != words.len() {
        return Err(ShellError::parse("case: unexpected tokens after esac"));
    }

    Ok(Some(CaseBlock {
        value: value_segments,
        arms,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaseTerminator {
    ArmSeparator,
    FallThrough,
    PatternContinue,
    Esac,
}

impl CaseTerminator {
    fn token_count(self) -> usize {
        match self {
            Self::ArmSeparator | Self::FallThrough => 2,
            Self::PatternContinue => 3,
            Self::Esac => 1,
        }
    }
}

fn parse_case_patterns(
    invocation: &CommandInvocation,
    words: &[IfWord],
    index: &mut usize,
) -> Result<Vec<Vec<WordSegment>>, ShellError> {
    let mut patterns = Vec::new();
    let mut current = Vec::new();

    while let Some(word) = words.get(*index) {
        if word.quote == QuoteKind::None && word.text == "|" {
            push_case_pattern(&mut patterns, &mut current)?;
            *index += 1;
            continue;
        }

        let terminates = case_pattern_token_terminates(invocation, *index);
        let mut segments = case_pattern_segments(invocation, *index);
        if terminates {
            strip_case_pattern_terminator(&mut segments);
        }
        current.extend(segments);
        *index += 1;

        if terminates {
            push_case_pattern(&mut patterns, &mut current)?;
            return Ok(patterns);
        }
    }

    Err(ShellError::parse("case: missing ) after pattern"))
}

fn push_case_pattern(
    patterns: &mut Vec<Vec<WordSegment>>,
    current: &mut Vec<WordSegment>,
) -> Result<(), ShellError> {
    strip_case_pattern_leading_paren(current);
    if current.is_empty() {
        return Err(ShellError::parse("case: empty pattern"));
    }
    patterns.push(std::mem::take(current));
    Ok(())
}

fn case_pattern_token_terminates(invocation: &CommandInvocation, index: usize) -> bool {
    invocation
        .argv_segments
        .get(index)
        .and_then(|segments| segments.last())
        .is_some_and(|segment| segment.quote == QuoteKind::None && segment.text.ends_with(')'))
}

fn case_pattern_segments(invocation: &CommandInvocation, index: usize) -> Vec<WordSegment> {
    invocation
        .argv_segments
        .get(index)
        .cloned()
        .unwrap_or_else(|| {
            vec![WordSegment::new(
                invocation.argv[index].clone(),
                invocation.argv_quote[index],
            )]
        })
}

fn strip_case_pattern_terminator(segments: &mut Vec<WordSegment>) {
    if let Some(last) = segments.last_mut() {
        if last.quote == QuoteKind::None && last.text.ends_with(')') {
            last.text.pop();
        }
    }
    while segments
        .last()
        .is_some_and(|segment| segment.text.is_empty())
    {
        segments.pop();
    }
}

fn strip_case_pattern_leading_paren(segments: &mut Vec<WordSegment>) {
    if let Some(first) = segments.first_mut() {
        if first.quote == QuoteKind::None && first.text.starts_with('(') {
            first.text.remove(0);
        }
    }
    while segments
        .first()
        .is_some_and(|segment| segment.text.is_empty())
    {
        segments.remove(0);
    }
}

fn find_case_arm_terminator(words: &[IfWord], start: usize) -> Option<(usize, CaseTerminator)> {
    let mut nested_case_depth = 0usize;
    for index in start..words.len() {
        let word = &words[index];
        let command_position = is_if_word_command_position(words, start, index);
        if command_position && word.quote == QuoteKind::None {
            match word.text.as_str() {
                "case" => {
                    nested_case_depth += 1;
                    continue;
                }
                "esac" => {
                    if nested_case_depth == 0 {
                        return Some((index, CaseTerminator::Esac));
                    }
                    nested_case_depth = nested_case_depth.saturating_sub(1);
                    continue;
                }
                _ => {}
            }
        }
        if nested_case_depth == 0 && word.quote == QuoteKind::None && word.text == ";" {
            if words
                .get(index + 1)
                .is_some_and(|next| next.quote == QuoteKind::None && next.text == ";")
                && words
                    .get(index + 2)
                    .is_some_and(|next| next.quote == QuoteKind::None && next.text == "&")
            {
                return Some((index, CaseTerminator::PatternContinue));
            }
            if words
                .get(index + 1)
                .is_some_and(|next| next.quote == QuoteKind::None && next.text == ";")
            {
                return Some((index, CaseTerminator::ArmSeparator));
            }
            if words
                .get(index + 1)
                .is_some_and(|next| next.quote == QuoteKind::None && next.text == "&")
            {
                return Some((index, CaseTerminator::FallThrough));
            }
        }
    }
    None
}

fn if_words(invocation: &CommandInvocation) -> Vec<IfWord> {
    invocation
        .argv
        .iter()
        .enumerate()
        .map(|(index, fallback)| {
            let quote = invocation
                .argv_quote
                .get(index)
                .copied()
                .unwrap_or(QuoteKind::None);
            let source = invocation
                .argv_segments
                .get(index)
                .map(|segments| word_segments_to_compound_source(segments, fallback))
                .unwrap_or_else(|| fallback.clone());
            IfWord {
                text: fallback.clone(),
                source,
                quote,
            }
        })
        .collect()
}

fn find_top_level_reserved(
    words: &[IfWord],
    start: usize,
    targets: &[&str],
) -> Option<(usize, String)> {
    let mut nested_if_depth = 0usize;
    let mut nested_done_depth = 0usize;

    for (index, word) in words.iter().enumerate().skip(start) {
        if word.quote != QuoteKind::None {
            continue;
        }
        let command_position = is_if_word_command_position(words, start, index);
        if word.text == "if" && command_position {
            nested_if_depth += 1;
            continue;
        }
        if word.text == "fi" && command_position {
            if nested_if_depth == 0 && nested_done_depth == 0 && targets.contains(&"fi") {
                return Some((index, word.text.clone()));
            }
            nested_if_depth = nested_if_depth.saturating_sub(1);
            continue;
        }
        if matches!(word.text.as_str(), "while" | "until" | "for" | "select") && command_position {
            nested_done_depth += 1;
            continue;
        }
        if word.text == "done" && command_position {
            if nested_if_depth == 0 && nested_done_depth == 0 && targets.contains(&"done") {
                return Some((index, word.text.clone()));
            }
            nested_done_depth = nested_done_depth.saturating_sub(1);
            continue;
        }
        if nested_if_depth == 0
            && nested_done_depth == 0
            && command_position
            && targets.contains(&word.text.as_str())
        {
            return Some((index, word.text.clone()));
        }
    }

    None
}

fn is_if_word_command_position(words: &[IfWord], start: usize, index: usize) -> bool {
    if index == start {
        return true;
    }

    let Some(previous) = words[start..index]
        .iter()
        .rev()
        .find(|word| word.quote == QuoteKind::None)
    else {
        return true;
    };

    matches!(
        previous.text.as_str(),
        ";" | "&&" | "||" | "|" | "then" | "else" | "elif" | "do"
    )
}

fn words_to_source(words: &[IfWord]) -> String {
    words
        .iter()
        .map(|word| word.source.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_if_invocation(
    if_block: &IfBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };

    for clause in &if_block.clauses {
        let mut condition = run_command_source(&clause.condition, state, &nested_options)?;
        final_outcome.stdout.append(&mut condition.stdout);
        final_outcome.stderr.append(&mut condition.stderr);
        if state.should_exit() || state.loop_control_requested() {
            final_outcome.exit_code = condition.exit_code;
            apply_compound_redirections(&mut final_outcome, invocation, state)?;
            return Ok(final_outcome);
        }
        if condition.exit_code == 0 {
            let mut body = run_command_source(&clause.body, state, &nested_options)?;
            final_outcome.exit_code = body.exit_code;
            final_outcome.stdout.append(&mut body.stdout);
            final_outcome.stderr.append(&mut body.stderr);
            apply_compound_redirections(&mut final_outcome, invocation, state)?;
            return Ok(final_outcome);
        }
    }

    if let Some(else_body) = &if_block.else_body {
        let mut body = run_command_source(else_body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.stdout.append(&mut body.stdout);
        final_outcome.stderr.append(&mut body.stderr);
    }

    apply_compound_redirections(&mut final_outcome, invocation, state)?;
    Ok(final_outcome)
}

fn run_while_invocation(
    while_block: &WhileBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    state.enter_loop();
    let result = run_while_invocation_inner(
        while_block,
        invocation,
        state,
        output_mode,
        capture_outputs,
        allow_process_replacement,
    );
    state.leave_loop();
    result
}

fn run_while_invocation_inner(
    while_block: &WhileBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };

    loop {
        let mut condition = run_command_source(&while_block.condition, state, &nested_options)?;
        final_outcome.stdout.append(&mut condition.stdout);
        final_outcome.stderr.append(&mut condition.stderr);
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
        {
            final_outcome.exit_code = condition.exit_code;
            break;
        }
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if !while_block.kind.should_run_body(condition.exit_code) {
            break;
        }

        let mut body = run_command_source(&while_block.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.stdout.append(&mut body.stdout);
        final_outcome.stderr.append(&mut body.stderr);
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
            || (state.errexit() && body.exit_code != 0)
        {
            break;
        }
    }

    apply_compound_redirections(&mut final_outcome, invocation, state)?;
    Ok(final_outcome)
}

fn run_for_invocation(
    for_block: &ForBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    state.enter_loop();
    let result = run_for_invocation_inner(
        for_block,
        invocation,
        state,
        output_mode,
        capture_outputs,
        allow_process_replacement,
    );
    state.leave_loop();
    result
}

fn run_for_invocation_inner(
    for_block: &ForBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    // C-style arithmetic for-loop.
    if let Some((init, cond, step)) = &for_block.arithmetic {
        if !init.is_empty() {
            eval_arithmetic(init, state)?;
        }
        loop {
            if !cond.is_empty() && eval_arithmetic(cond, state)? == 0 {
                break;
            }
            let mut body = run_command_source(&for_block.body, state, &nested_options)?;
            final_outcome.exit_code = body.exit_code;
            final_outcome.stdout.append(&mut body.stdout);
            final_outcome.stderr.append(&mut body.stderr);
            if state.loop_control_requested() {
                match state.handle_loop_control_for_current_loop() {
                    Some(LoopControlKind::Break) => break,
                    Some(LoopControlKind::Continue) => {}
                    None => {}
                }
            }
            if state.should_exit()
                || state.return_requested()
                || state.interrupted()
                || state.stream_pipe_closed()
                || (state.errexit() && body.exit_code != 0)
            {
                break;
            }
            if !step.is_empty() {
                eval_arithmetic(step, state)?;
            }
        }
        apply_compound_redirections(&mut final_outcome, invocation, state)?;
        return Ok(final_outcome);
    }

    let items = expand_for_items(&for_block.items, state)?;

    for item in items {
        state.set_var(&for_block.variable, &item);
        let mut body = run_command_source(&for_block.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.stdout.append(&mut body.stdout);
        final_outcome.stderr.append(&mut body.stderr);
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
            || (state.errexit() && body.exit_code != 0)
        {
            break;
        }
    }

    apply_compound_redirections(&mut final_outcome, invocation, state)?;
    Ok(final_outcome)
}

fn run_select_invocation(
    select_block: &SelectBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    state.enter_loop();
    let result = run_select_invocation_inner(
        select_block,
        invocation,
        state,
        output_mode,
        capture_outputs,
        allow_process_replacement,
    );
    state.leave_loop();
    result
}

fn run_select_invocation_inner(
    select_block: &SelectBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let mut final_outcome = CommandOutcome::captured(1, Vec::new(), Vec::new());
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let items = expand_for_items(&select_block.items, state)?;
    let live_prompt =
        !capture_outputs && invocation.redirections.is_empty() && io::stdin().is_terminal();

    emit_select_stderr(&mut final_outcome, live_prompt, &format_select_menu(&items))?;
    let prompt = state.lookup("PS3").unwrap_or("#? ").to_string();

    loop {
        emit_select_stderr(&mut final_outcome, live_prompt, prompt.as_bytes())?;
        let Some(mut line) = read_one_line(None, state)? else {
            break;
        };
        trim_line_ending(&mut line);
        state.set_var("REPLY", &line);
        let selected = select_choice_index(&line, items.len())
            .and_then(|index| items.get(index))
            .cloned()
            .unwrap_or_default();
        state.set_var(&select_block.variable, &selected);

        let mut body = run_command_source(&select_block.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.stdout.append(&mut body.stdout);
        final_outcome.stderr.append(&mut body.stderr);
        if state.loop_control_requested() {
            match state.handle_loop_control_for_current_loop() {
                Some(LoopControlKind::Break) => break,
                Some(LoopControlKind::Continue) => continue,
                None => {}
            }
        }
        if state.should_exit()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
            || (state.errexit() && body.exit_code != 0)
        {
            break;
        }
    }

    apply_compound_redirections(&mut final_outcome, invocation, state)?;
    Ok(final_outcome)
}

fn format_select_menu(items: &[String]) -> Vec<u8> {
    let mut menu = Vec::new();
    for (index, item) in items.iter().enumerate() {
        menu.extend_from_slice(format!("{}) {item}\n", index + 1).as_bytes());
    }
    menu
}

fn emit_select_stderr(
    outcome: &mut CommandOutcome,
    live_prompt: bool,
    bytes: &[u8],
) -> Result<(), ShellError> {
    if live_prompt {
        let mut stderr = io::stderr();
        stderr.write_all(bytes)?;
        stderr.flush()?;
    } else {
        outcome.stderr.extend_from_slice(bytes);
    }
    Ok(())
}

fn select_choice_index(input: &str, item_count: usize) -> Option<usize> {
    let choice = input.trim().parse::<usize>().ok()?;
    if (1..=item_count).contains(&choice) {
        Some(choice - 1)
    } else {
        None
    }
}

fn run_case_invocation(
    case_block: &CaseBlock,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let value = expand_word(&case_block.value, state)?;
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    let mut execute_next_arm = false;
    for arm in &case_block.arms {
        let should_execute = if execute_next_arm {
            true
        } else {
            let mut matched = false;
            for pattern in &arm.patterns {
                let pattern = expand_word(pattern, state)?;
                if glob_match_bytes(pattern.as_bytes(), value.as_bytes()) {
                    matched = true;
                    break;
                }
            }
            matched
        };
        if !should_execute {
            continue;
        }

        let mut body = run_command_source(&arm.body, state, &nested_options)?;
        final_outcome.exit_code = body.exit_code;
        final_outcome.stdout.append(&mut body.stdout);
        final_outcome.stderr.append(&mut body.stderr);
        match arm.terminator {
            CaseTerminator::ArmSeparator | CaseTerminator::Esac => break,
            CaseTerminator::FallThrough => execute_next_arm = true,
            CaseTerminator::PatternContinue => execute_next_arm = false,
        }
    }

    apply_compound_redirections(&mut final_outcome, invocation, state)?;
    Ok(final_outcome)
}

fn expand_for_items(items: &ForItems, state: &mut ShellState) -> Result<Vec<String>, ShellError> {
    match items {
        ForItems::Words(words) => {
            let mut fields = Vec::new();
            for word in words {
                fields.extend(expand_word_segments_to_argv_fields(word, state)?);
            }
            Ok(fields)
        }
        ForItems::Positionals => Ok(state.positionals()),
    }
}

/// Detect a subshell `( list )` invocation and return its inner source.
fn parse_subshell_invocation(invocation: &CommandInvocation) -> Result<Option<String>, ShellError> {
    // `(( ... ))` is an arithmetic command, not a subshell.
    if invocation
        .argv
        .first()
        .is_some_and(|a| a.starts_with("((") && a.ends_with("))"))
    {
        return Ok(None);
    }
    let starts_paren = invocation
        .argv_segments
        .first()
        .and_then(|segments| segments.first())
        .map(|segment| segment.quote == QuoteKind::None && segment.text.starts_with('('))
        .unwrap_or_else(|| {
            invocation.argv_quote.first() == Some(&QuoteKind::None)
                && invocation
                    .argv
                    .first()
                    .is_some_and(|word| word.starts_with('('))
        });
    if !starts_paren {
        return Ok(None);
    }

    let words = if_words(invocation);
    let joined = words_to_source(&words);
    let trimmed = joined.trim();
    let inner = trimmed
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| ShellError::parse("subshell: missing matching )"))?;
    Ok(Some(inner.trim().to_string()))
}

/// Detect a brace group `{ list; }` invocation and return its inner source.
fn parse_brace_group_invocation(
    invocation: &CommandInvocation,
) -> Result<Option<String>, ShellError> {
    let opens = invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| word == "{" && *quote == QuoteKind::None);
    if !opens {
        return Ok(None);
    }
    let len = invocation.argv.len();
    let closes = len >= 2
        && invocation
            .argv
            .last()
            .zip(invocation.argv_quote.last())
            .is_some_and(|(word, quote)| word == "}" && *quote == QuoteKind::None);
    if !closes {
        return Err(ShellError::parse("brace group: missing closing }"));
    }

    let words = if_words(invocation);
    let inner = words_to_source(&words[1..words.len() - 1]);
    Ok(Some(inner.trim().to_string()))
}

/// Run a subshell: execute the inner list in an isolated copy of shell state so
/// variable/cwd/option changes do not propagate to the parent.
fn run_subshell_invocation(
    inner_source: &str,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let mut sub_state = state.clone();
    let result = run_command_source(inner_source, &mut sub_state, &nested_options);
    // `cd` mutates the process working directory; restore it so the subshell's
    // directory changes stay isolated from the parent.
    let _ = std::env::set_current_dir(state.cwd());
    let mut outcome = result?;
    outcome.observation = None;
    apply_compound_redirections(&mut outcome, invocation, state)?;
    Ok(outcome)
}

/// Run a brace group: execute the inner list in the current shell state.
fn run_brace_group_invocation(
    inner_source: &str,
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let mut outcome = run_command_source(inner_source, state, &nested_options)?;
    outcome.observation = None;
    apply_compound_redirections(&mut outcome, invocation, state)?;
    Ok(outcome)
}

fn run_command_source(
    source: &str,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    if source.trim().is_empty() {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }
    let graph = parse_line(source)?;
    let mut executor = Executor::new();
    let mut outcome = executor.run_graph(&graph, state, options)?;
    outcome.observation = None;
    Ok(outcome)
}

fn apply_compound_redirections(
    outcome: &mut CommandOutcome,
    invocation: &CommandInvocation,
    state: &mut ShellState,
) -> Result<(), ShellError> {
    let redirections = expand_redirections(&invocation.redirections, state)?;
    apply_builtin_redirections(outcome, &redirections, state)
}

fn run_function_invocation(
    function: &ShellFunction,
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let saved_positionals = save_positionals(state);
    let function_args = invocation.argv.iter().skip(1).cloned().collect::<Vec<_>>();
    state.set_positionals(&function_args);
    state.enter_function_scope();

    for assignment in &invocation.assignments {
        apply_assignment(state, assignment);
    }

    let redirected_stdin = redirected_stdin_from_expanded_redirections(&invocation.redirections)?;
    let result: Result<CommandOutcome, ShellError> =
        run_with_effective_shell_stdin(state, stdin_data, redirected_stdin, |state| {
            let graph = parse_line(&function.body)?;
            let mut executor = Executor::new();
            let mut outcome = executor.run_graph(
                &graph,
                state,
                &ExecutionOptions {
                    output_mode: if capture_outputs {
                        OutputMode::Clean
                    } else {
                        output_mode
                    },
                    allow_process_replacement,
                },
            )?;
            outcome.observation = None;
            apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
            Ok(outcome)
        });
    state.leave_function_scope();
    restore_positionals(state, &saved_positionals);
    let mut outcome = result?;
    // `return` inside the body stops the function; consume the request here so
    // it does not leak to the caller.
    if let Some(code) = state.take_return() {
        outcome.exit_code = code;
    }
    Ok(outcome)
}

fn save_positionals(state: &ShellState) -> PositionalSnapshot {
    PositionalSnapshot {
        args: state.positionals(),
        at_was_set: state.lookup("@").is_some(),
    }
}

fn restore_positionals(state: &mut ShellState, saved: &PositionalSnapshot) {
    if saved.at_was_set || !saved.args.is_empty() {
        state.set_positionals(&saved.args);
    } else {
        state.clear_positionals();
    }
}

fn word_segments_to_source(segments: &[WordSegment], fallback: &str) -> String {
    if segments.is_empty() {
        return fallback.to_string();
    }
    segments
        .iter()
        .map(|segment| match segment.quote {
            QuoteKind::None => shell_escape_unquoted(&segment.text),
            QuoteKind::Single => format!("'{}'", segment.text.replace('\'', "'\\''")),
            QuoteKind::Double => format!(
                "\"{}\"",
                segment.text.replace('\\', "\\\\").replace('"', "\\\"")
            ),
        })
        .collect::<String>()
}

fn word_segments_to_compound_source(segments: &[WordSegment], fallback: &str) -> String {
    if segments.is_empty() {
        return fallback.to_string();
    }
    segments
        .iter()
        .map(|segment| match segment.quote {
            QuoteKind::None => segment.text.clone(),
            QuoteKind::Single => format!("'{}'", segment.text.replace('\'', "'\\''")),
            QuoteKind::Double => format!(
                "\"{}\"",
                segment.text.replace('\\', "\\\\").replace('"', "\\\"")
            ),
        })
        .collect::<String>()
}

fn shell_escape_unquoted(text: &str) -> String {
    if text
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"' | '|' | '<' | '>' | '&'))
    {
        format!("'{}'", text.replace('\'', "'\\''"))
    } else {
        text.to_string()
    }
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// A command-not-found result: exit 127, error to stderr, execution continues
/// (matching POSIX shells) rather than aborting the whole command line.
fn command_not_found_outcome(name: &str, state: &ShellState) -> CommandOutcome {
    use std::io::IsTerminal;
    // Color the message only when stderr is a TTY (piped/`-c` stderr stays plain).
    let theme = if std::io::stderr().is_terminal() {
        state.theme()
    } else {
        agsh_style::Theme::plain()
    };

    let mut message = String::new();
    message.push_str(&theme.paint(
        agsh_style::Role::Error,
        &format!("{} {name}: command not found", theme.icons.error()),
    ));
    message.push('\n');

    // The "did you mean" + install advisory repeats verbatim if the same typo recurs
    // in a loop or an agent retry, flooding the context — so emit it only on the
    // first occurrence of this name per session. The error line + exit 127 always emit.
    if state.advise_once(&format!("cmd-not-found:{name}")) {
        let mut candidates: Vec<String> = crate::builtins::builtin_names()
            .iter()
            .map(|s| s.to_string())
            .collect();
        candidates.extend(state.aliases().keys().cloned());
        candidates.extend(state.functions().keys().cloned());
        if let Some(path) = state.lookup("PATH") {
            candidates.extend(crate::suggest::path_executables(path));
        }
        let suggestions = crate::suggest::did_you_mean(name, candidates.into_iter());
        if !suggestions.is_empty() {
            message.push_str(&theme.paint(agsh_style::Role::Muted, "Did you mean:"));
            message.push('\n');
            for suggestion in &suggestions {
                message.push_str(&format!(
                    "  {}\n",
                    theme.paint(agsh_style::Role::Command, suggestion)
                ));
            }
        }
        if let Some(hint) = crate::suggest::install_hint(name) {
            message.push_str(&theme.paint(agsh_style::Role::Muted, &format!("Install: {hint}")));
            message.push('\n');
        }
    }

    CommandOutcome::captured(127, Vec::new(), message.into_bytes())
}

/// `builtin <name>` where name is not a shell builtin: exit 1, like bash.
fn builtin_not_found_outcome(name: &str) -> CommandOutcome {
    CommandOutcome::captured(
        1,
        Vec::new(),
        format!("agsh: builtin: {name}: not a shell builtin\n").into_bytes(),
    )
}

fn run_resolved_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    lookup_mode: LookupMode,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let name = invocation.argv[0].as_str();

    // Confinement gate (single-command path): refuse a non-allowlisted external
    // with a clear message and exit 126, before it resolves. Builtins are exempt
    // — they funnel any external targets back through the gated resolver. This is
    // hit by direct commands and by eval/`$(…)`/subshell re-entry alike.
    if let Some(policy) = state.confine_policy() {
        if !is_builtin(name) && !policy.allows(name) {
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            return Ok(CommandOutcome::captured(
                126,
                Vec::new(),
                format!(
                    "agsh: {base}: not permitted in this confined session (allowed: {})\n",
                    policy.display_list()
                )
                .into_bytes(),
            ));
        }
    }

    let xtrace = if state.xtrace() {
        Some(render_xtrace(invocation))
    } else {
        None
    };

    let mut outcome = match lookup_mode {
        LookupMode::BuiltinOnly => {
            if !is_builtin(name) {
                return Ok(builtin_not_found_outcome(name));
            }
            run_shell_builtin_invocation(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                allow_process_replacement,
            )
        }
        LookupMode::ExternalOnly => {
            let Some(path) = resolve_external_path(invocation, state, name) else {
                return Ok(command_not_found_outcome(name, state));
            };
            run_external(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                Some(path.as_path()),
            )
        }
        LookupMode::Normal | LookupMode::BypassAliases | LookupMode::DefaultPath
            if is_builtin(name) =>
        {
            run_shell_builtin_invocation(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                allow_process_replacement,
            )
        }
        LookupMode::Normal | LookupMode::BypassAliases | LookupMode::DefaultPath => {
            let path = if lookup_mode == LookupMode::DefaultPath {
                resolve_external_path_with(invocation, state, name, DEFAULT_COMMAND_PATH)
            } else {
                resolve_external_path(invocation, state, name)
            };
            let Some(path) = path else {
                return Ok(command_not_found_outcome(name, state));
            };
            run_external(
                invocation,
                state,
                output_mode,
                stdin_data,
                capture_outputs,
                Some(path.as_path()),
            )
        }
    }?;

    if let Some(mut xtrace) = xtrace {
        xtrace.append(&mut outcome.stderr);
        outcome.stderr = xtrace;
    }

    Ok(outcome)
}

fn render_xtrace(invocation: &ExpandedInvocation) -> Vec<u8> {
    let mut parts = invocation
        .assignments
        .iter()
        .map(|assignment| {
            format!(
                "{}={}",
                assignment.name,
                shell_escape_unquoted(&assignment.value)
            )
        })
        .collect::<Vec<_>>();
    parts.extend(
        invocation
            .argv
            .iter()
            .map(|arg| shell_escape_unquoted(arg))
            .collect::<Vec<_>>(),
    );
    format!("+ {}\n", parts.join(" ")).into_bytes()
}

/// POSIX special builtins: a preceding variable assignment persists after the
/// builtin returns (unlike regular builtins, where it is transient).
fn is_special_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "."
            | "source"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "unset"
            | "break"
            | "continue"
    )
}

fn run_shell_builtin_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let redirected_stdin = redirected_stdin_from_expanded_redirections(&invocation.redirections)?;
    let stdin_data = redirected_stdin.as_deref().or(stdin_data);

    // Temporary prefix assignments (e.g. `IFS=: read a b`) apply to the shell
    // for the duration of a regular builtin, then are restored. For POSIX
    // special builtins the assignments persist, so the builtin itself handles
    // them and the wrapper leaves state alone.
    let transient = !is_special_builtin(&invocation.argv[0]);
    let saved_assignments: Vec<(String, Option<String>)> = if transient {
        let saved = invocation
            .assignments
            .iter()
            .map(|assignment| {
                (
                    assignment.name.clone(),
                    state.lookup(&assignment.name).map(str::to_string),
                )
            })
            .collect();
        for assignment in &invocation.assignments {
            apply_assignment(state, assignment);
        }
        saved
    } else {
        Vec::new()
    };

    let result = match invocation.argv[0].as_str() {
        "eval" => run_eval_invocation(
            invocation,
            state,
            output_mode,
            stdin_data,
            capture_outputs,
            allow_process_replacement,
        ),
        "source" | "." => run_source_invocation(
            invocation,
            state,
            output_mode,
            stdin_data,
            capture_outputs,
            allow_process_replacement,
        ),
        "exec" => run_exec_invocation(
            invocation,
            state,
            stdin_data,
            capture_outputs,
            allow_process_replacement,
        ),
        "read" => run_read_invocation(invocation, state, stdin_data),
        "agpatch" => run_patch_invocation(invocation, state, stdin_data),
        "agconfine" | "confine" => run_confine_invocation(
            invocation,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        ),
        name if name.starts_with("((") && name.ends_with("))") => {
            // `(( expr ))` arithmetic command: exit 0 if the value is non-zero.
            let expr = name[2..name.len() - 2].to_string();
            match eval_arithmetic(&expr, state) {
                Ok(value) => Ok(CommandOutcome::captured(
                    i32::from(value == 0),
                    Vec::new(),
                    Vec::new(),
                )),
                Err(error) => Ok(CommandOutcome::captured(
                    1,
                    Vec::new(),
                    format!("agsh: ((: {error}\n").into_bytes(),
                )),
            }
        }
        _ => run_builtin(&to_command_invocation(invocation), state),
    };

    for (name, prior) in saved_assignments {
        match prior {
            Some(value) => state.set_var(name, value),
            None => state.unset(&name),
        }
    }

    let mut outcome = result?;
    apply_builtin_redirections(&mut outcome, &invocation.redirections, state)?;
    Ok(outcome)
}

/// POSIX-sh shim body; `__AGSH__` is replaced with the quoted agsh path.
const SHIM_TEMPLATE: &str = r#"#!/bin/sh
# agsh confine shim — route the agent's shell through confined agsh.
c=
while [ $# -gt 0 ]; do
  case "$1" in
    -c) c=$2; shift 2; break;;
    --) shift; break;;
    --rcfile|--init-file|-o|-O) shift 2;;
    --*) shift;;
    -*c*) c=$2; shift 2; break;;
    -*) shift;;
    *) break;;
  esac
done
if [ -n "$c" ]; then exec __AGSH__ -c "$c"; fi
case "${1:-}" in
  -*|"") exec __AGSH__;;
  *) exec __AGSH__ "$@";;
esac
"#;

/// Install shell shims so a confined agent's own shell invocations are routed
/// back through agsh and gated. Claude-style agents run commands via their own
/// `bash -c '…'` subprocess (not through agsh), which would otherwise bypass the
/// allowlist. This drops `bash`/`sh`/`zsh`/`dash` shims — each re-execs agsh, which
/// self-confines from the inherited `AGSH_CONFINE` — into a temp dir, prepends it
/// to `PATH`, and points `SHELL` at it.
///
/// Coverage caveat: this catches shells resolved via `PATH`/`SHELL`. A program
/// that calls `/bin/bash` by absolute path, or spawns an interpreter (python,
/// node) directly, still bypasses it — closing that needs an OS sandbox (G7).
pub fn install_confine_shims(state: &mut ShellState) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe()?;
    let dir = std::env::temp_dir().join(format!("agsh-confine-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    // POSIX-sh shim: pull the command out of `-c CMD` (handling separate `-l -c`,
    // combined short bundles `-lc`/`-ic`, and arg-taking long options like
    // `--rcfile FILE`/`--norc`) and re-exec agsh, which self-confines from
    // AGSH_CONFINE. With no `-c` (interactive/persistent login shell) it drops to
    // an agsh REPL; stray flags are never forwarded to agsh (they would error).
    // Long `--*` options are matched before the `-*c*` short-bundle rule so that
    // e.g. `--norc`/`--rcfile` are NOT mistaken for a `-c`-bearing flag.
    let agsh = format!("'{}'", exe.display().to_string().replace('\'', "'\\''"));
    let shim = SHIM_TEMPLATE.replace("__AGSH__", &agsh);
    for name in ["bash", "sh", "zsh", "dash", "ksh"] {
        let path = dir.join(name);
        std::fs::write(&path, &shim)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    let prev_path = state.lookup("PATH").unwrap_or_default().to_string();
    state.export_var("PATH", format!("{}:{prev_path}", dir.display()));
    state.export_var("SHELL", dir.join("bash").display().to_string());
    Ok(dir)
}

/// Interception shim body (the compacting-proxy / flavor B): route the agent's
/// shell through `agsh --observe`, which runs the REAL shell and captures+renders
/// its output. Passes straight through to the real shell once already inside an
/// observed subtree, so nested shells run normally and there is no re-entrancy.
const INTERCEPT_SHIM_TEMPLATE: &str = r#"#!/bin/sh
# agsh interception shim — observe the agent's shell through agsh.
if [ -n "$AGSH_INTERCEPT_ACTIVE" ]; then exec __REAL__ "$@"; fi
exec __AGSH__ --output __MODE__ --observe __REAL__ "$@"
"#;

/// Flavor-A (native-interpret) interception shim: agsh *interprets* the `-c`
/// command itself rather than running the real shell. `AGSH_INTERCEPT_ACTIVE=1` is
/// set for the agsh child so any nested shells run as the real shell (no recursion);
/// non-`-c` invocations fall through to the real shell unchanged.
const INTERCEPT_SHIM_TEMPLATE_NATIVE: &str = r#"#!/bin/sh
# agsh interception shim (native) — interpret the agent's command in agsh.
if [ -n "$AGSH_INTERCEPT_ACTIVE" ]; then exec __REAL__ "$@"; fi
c=
while [ $# -gt 0 ]; do
  case "$1" in
    -c) c=$2; shift 2; break;;
    --) shift; break;;
    --rcfile|--init-file|-o|-O) shift 2;;
    --*) shift;;
    -*c*) c=$2; shift 2; break;;
    -*) shift;;
    *) break;;
  esac
done
if [ -n "$c" ]; then AGSH_INTERCEPT_ACTIVE=1 exec __AGSH__ --output __MODE__ -c "$c"; fi
exec __REAL__ "$@"
"#;

/// Install shell-*interception* shims (opt-in via `AGSH_INTERCEPT`): route the
/// session's `bash`/`sh`/`zsh`/… — resolved by name or via `$SHELL` — through
/// `agsh --observe`, so an agent's own `bash -c …` output is captured and rendered
/// in `mode`. The real shells are located on the current `PATH`; only installed
/// shells are shimmed, and each shim execs the real shell by absolute path (so it
/// never bounces back through the shim). Unlike [`install_confine_shims`], this
/// runs the real shell (exact semantics) instead of re-interpreting in agsh.
///
/// Coverage caveat: this catches shells resolved via `PATH`/`$SHELL`; a program
/// calling `/bin/bash` by absolute path bypasses it (that needs the exec-
/// interposition layer).
pub fn install_intercept_shims(
    state: &mut ShellState,
    mode: agsh_output::OutputMode,
    native: bool,
) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let exe = std::env::current_exe()?;
    let dir = std::env::temp_dir().join(format!("agsh-intercept-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let agsh = shell_quote(&exe.display().to_string());
    let template = if native {
        INTERCEPT_SHIM_TEMPLATE_NATIVE
    } else {
        INTERCEPT_SHIM_TEMPLATE
    };
    let path_str = state.lookup("PATH").unwrap_or_default().to_string();
    let mut shimmed = false;
    for name in ["bash", "sh", "zsh", "dash", "ksh"] {
        // Resolve against the ORIGINAL PATH (our shim dir isn't prepended yet), so
        // we never point a shim at itself.
        let Some(real) = resolve_on_path(name, &path_str) else {
            continue;
        };
        let shim = template
            .replace("__REAL__", &shell_quote(&real.display().to_string()))
            .replace("__AGSH__", &agsh)
            .replace("__MODE__", mode.as_str());
        let path = dir.join(name);
        std::fs::write(&path, &shim)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        shimmed = true;
    }
    if shimmed {
        state.export_var("PATH", format!("{}:{path_str}", dir.display()));
        state.export_var("SHELL", dir.join("bash").display().to_string());
        // Persist observed commands' raw output here so `raw:` file-path references
        // resolve across the ephemeral one-shot `agsh --observe` processes.
        if state.lookup("AGSH_TRACE_DIR").is_none() {
            let trace_dir = std::env::temp_dir().join("agsh-traces");
            state.export_var("AGSH_TRACE_DIR", trace_dir.display().to_string());
        }
        set_agent_fail_fast_env(state);
    }
    Ok(dir)
}

/// Make interactive tools FAIL FAST instead of blocking an agent forever on a
/// terminal password prompt (a hang `confine` can't see — it gates capabilities,
/// not a `/dev/tty` read). Only well-known non-interactive toggles, and only if the
/// user hasn't set them — no `unsafe`, no `setsid` (macOS ships no such binary),
/// portable. The dominant real case is git-over-HTTPS credential prompts.
fn set_agent_fail_fast_env(state: &mut ShellState) {
    const FAIL_FAST: &[(&str, &str)] = &[
        ("GIT_TERMINAL_PROMPT", "0"), // git: error instead of prompting for creds
        ("GCM_INTERACTIVE", "never"), // git-credential-manager: no interactive UI
        ("SSH_ASKPASS_REQUIRE", "never"), // ssh: don't pop an askpass helper
    ];
    for (key, value) in FAIL_FAST {
        if state.lookup(key).is_none() {
            state.export_var(*key, *value);
        }
    }
}

/// Find the first executable file named `name` in a colon-separated `PATH`.
fn resolve_on_path(name: &str, path: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = std::path::Path::new(dir).join(name);
        if let Ok(meta) = std::fs::metadata(&candidate) {
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                return Some(candidate);
            }
        }
    }
    None
}

/// Marker present in a PATH entry / `$SHELL` when interception is installed.
const INTERCEPT_DIR_MARKER: &str = "agsh-intercept-";

/// Parse an interception spec `<mode>[:native][:deep]` into `(mode, native, deep)`.
/// Returns `None` when disabled (`off`/empty) or the mode name is invalid.
pub fn parse_intercept_spec(spec: &str) -> Option<(agsh_output::OutputMode, bool, bool)> {
    let spec = spec.trim().to_ascii_lowercase();
    let mut parts = spec.split(':');
    let mode_part = parts.next().unwrap_or("");
    let flags: Vec<&str> = parts.collect();
    let native = flags.iter().any(|f| matches!(*f, "native" | "interpret"));
    let deep = flags.contains(&"deep");
    let mode = match mode_part {
        "" | "0" | "off" | "false" | "no" => return None,
        "1" | "on" | "true" | "yes" => agsh_output::OutputMode::Compact,
        other => <agsh_output::OutputMode as std::str::FromStr>::from_str(other).ok()?,
    };
    Some((mode, native, deep))
}

/// Whether shell interception is currently installed in `state` (a shim dir is on
/// `PATH`).
pub fn intercept_active(state: &ShellState) -> bool {
    state
        .lookup("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|d| d.contains(INTERCEPT_DIR_MARKER))
}

/// Enable the exec-interposition (deep) layer for the session's children by
/// preloading the `agsh-intercept` library. Returns `false` if the library can't be
/// located (caller should fall back to the PATH shims).
pub fn install_deep_intercept(state: &mut ShellState, mode: agsh_output::OutputMode) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let dir = exe
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let (var, ext) = if cfg!(target_os = "macos") {
        ("DYLD_INSERT_LIBRARIES", "dylib")
    } else {
        ("LD_PRELOAD", "so")
    };
    let name = format!("libagsh_intercept.{ext}");
    // Next to the binary, or an installed `../lib` layout.
    let candidates = [dir.join(&name), dir.join("..").join("lib").join(&name)];
    let Some(lib) = candidates.iter().find(|p| p.exists()) else {
        return false;
    };
    let prev = state.lookup(var).unwrap_or_default().to_string();
    let value = if prev.is_empty() {
        lib.display().to_string()
    } else {
        format!("{}:{prev}", lib.display())
    };
    state.export_var(var, value);
    state.export_var("AGSH_SELF", exe.display().to_string());
    state.export_var("AGSH_INTERCEPT_MODE", mode.as_str());
    true
}

/// Remove any shell-interception install from `state` (the shim dir on `PATH`, the
/// preload env, `$SHELL`). Newly launched children are no longer intercepted;
/// already-running children keep their environment.
pub fn uninstall_intercept(state: &mut ShellState) {
    let path = state.lookup("PATH").unwrap_or_default().to_string();
    let cleaned = path
        .split(':')
        .filter(|d| !d.contains(INTERCEPT_DIR_MARKER))
        .collect::<Vec<_>>()
        .join(":");
    // Drop our entry from the preload var (and unset it if it becomes empty).
    for var in ["DYLD_INSERT_LIBRARIES", "LD_PRELOAD"] {
        if let Some(current) = state.lookup(var) {
            let kept = current
                .split(':')
                .filter(|e| !e.contains("libagsh_intercept"))
                .collect::<Vec<_>>()
                .join(":");
            if kept.is_empty() {
                state.unset(var);
            } else {
                state.export_var(var, kept);
            }
        }
    }
    state.unset("AGSH_SELF");
    state.unset("AGSH_INTERCEPT_MODE");
    // If `$SHELL` pointed at a shim, restore it to the real shell.
    if state
        .lookup("SHELL")
        .is_some_and(|s| s.contains(INTERCEPT_DIR_MARKER))
    {
        if let Some(real) = resolve_on_path("bash", &cleaned) {
            state.export_var("SHELL", real.display().to_string());
        }
    }
    state.export_var("PATH", cleaned);
}

/// `confine LIST [-- COMMAND…]` — restrict which external commands may run.
///
/// * Sticky (no COMMAND): confine the current session (and its children, via the
///   inherited `AGSH_CONFINE` env). Narrow-only and irreversible for the session.
/// * Scoped (`-- COMMAND`): run COMMAND as a trusted payload with the allowlist
///   applied to its *descendants* (`AGSH_CONFINE` exported for the child); the
///   payload itself is exempt unless the session is already confined. The env is
///   restored afterward so the interactive session is unchanged.
///
/// `LIST` is comma/space-separated (`ls,df`); a leading `--allow` is accepted for
/// symmetry with the `agsh --allow` launch flag.
fn run_confine_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let args = &invocation.argv[1..];
    // Everything up to the first `--` is the spec (presets, flags, exec-allowlist);
    // the rest is the payload command.
    let dashdash = args.iter().position(|a| a == "--");
    let (spec, command): (&[String], Vec<String>) = match dashdash {
        Some(idx) => (&args[..idx], args[idx + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    if spec.is_empty() && command.is_empty() {
        return Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            b"confine: usage: confine [PRESET] [--FLAG ...] [LIST] [-- COMMAND ...]\n  presets: read-only, workspace, offline\n".to_vec(),
        ));
    }
    let (requested, opts) = match crate::confine::parse_spec(spec) {
        Ok(parsed) => parsed,
        Err(message) => {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                format!("{message}\n").into_bytes(),
            ))
        }
    };

    if command.is_empty() {
        // A preset is an OS-sandbox of a leaf payload; it needs a `-- COMMAND`.
        if opts.preset != crate::confine::Preset::ExecOnly || opts.net.is_some() {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                b"confine: presets (read-only/workspace/offline) require '-- COMMAND'\n".to_vec(),
            ));
        }
        // Sticky: confine THIS agsh session (the agsh-routed gate) and propagate
        // to children (env + shims). Governs commands agsh itself runs; not an OS
        // sandbox (a running process can't be retroactively jailed).
        let effective = match state.confine_policy() {
            Some(p) => p.intersect(&requested),
            None => agsh_policy::AllowPolicy::from_names(&requested),
        };
        state.set_confine(&requested);
        state.export_var("AGSH_CONFINE", effective.to_list());
        let _ = install_confine_shims(state);
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }

    // Scoped: OS-enforced confine of a leaf payload (kernel-level), or refuse.
    // payload0 is the exact program name; payload_shell re-quotes each argv token
    // so quoting (e.g. `python3 -c '…'`) survives the `/bin/sh -c` wrapper.
    let payload = command
        .iter()
        .map(|t| crate::confine::shell_quote(t))
        .collect::<Vec<_>>()
        .join(" ");
    match crate::confine::plan(state, &requested, &command, &payload, &opts) {
        crate::confine::ConfinePlan::Refuse { message, code } => Ok(CommandOutcome::captured(
            code,
            Vec::new(),
            message.into_bytes(),
        )),
        crate::confine::ConfinePlan::Sandboxed {
            command,
            cleanup,
            explain,
        } => {
            // The kernel enforces the policy; no shims / AGSH_CONFINE needed.
            if let Some(summary) = explain {
                eprint!("{summary}");
            }
            let outcome = if opts.dry_run {
                eprintln!("confine: would run: {command}");
                Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()))
            } else {
                run_shell_source(
                    &command,
                    state,
                    output_mode,
                    capture_outputs,
                    allow_process_replacement,
                )
            };
            for path in &cleanup {
                let _ = std::fs::remove_file(path);
                let _ = std::fs::remove_dir_all(path);
            }
            outcome
        }
        crate::confine::ConfinePlan::BestEffort => {
            // Weaker shim layer: route the payload's shells through confined agsh.
            let effective = match state.confine_policy() {
                Some(p) => p.intersect(&requested),
                None => agsh_policy::AllowPolicy::from_names(&requested),
            };
            let prev_confine = state.lookup("AGSH_CONFINE").map(str::to_string);
            let prev_path = state.lookup("PATH").map(str::to_string);
            let prev_shell = state.lookup("SHELL").map(str::to_string);
            state.export_var("AGSH_CONFINE", effective.to_list());
            let _ = install_confine_shims(state);
            let outcome = run_shell_source(
                &payload,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            );
            restore_env(state, "AGSH_CONFINE", prev_confine);
            restore_env(state, "PATH", prev_path);
            restore_env(state, "SHELL", prev_shell);
            outcome
        }
    }
}

/// Restore (or unset) an environment variable to a saved prior value.
fn restore_env(state: &mut ShellState, name: &str, prev: Option<String>) {
    match prev {
        Some(value) => state.export_var(name, value),
        None => state.unset(name),
    }
}

fn run_eval_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    if invocation.argv.len() == 1 {
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }

    run_with_buffered_stdin(state, stdin_data, |state| {
        run_shell_source(
            &invocation.argv[1..].join(" "),
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    })
}

fn run_source_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let Some(path) = invocation.argv.get(1) else {
        return Ok(CommandOutcome::captured(
            2,
            Vec::new(),
            format!("{}: missing file\n", invocation.argv[0]).into_bytes(),
        ));
    };

    let path = PathBuf::from(resolve_shell_path(path, state.cwd()));
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            return Ok(CommandOutcome::captured(
                1,
                Vec::new(),
                format!("{}: {}: {error}\n", invocation.argv[0], path.display()).into_bytes(),
            ));
        }
    };

    let source_args = invocation.argv[2..].to_vec();
    let saved_positionals = if source_args.is_empty() {
        None
    } else {
        let saved = save_positionals(state);
        state.set_positionals(&source_args);
        Some(saved)
    };

    state.enter_source();
    let result = run_with_buffered_stdin(state, stdin_data, |state| {
        run_shell_source(
            &source,
            state,
            output_mode,
            capture_outputs,
            allow_process_replacement,
        )
    });
    state.leave_source();
    if let Some(saved_positionals) = saved_positionals {
        restore_positionals(state, &saved_positionals);
    }
    let mut outcome = result?;
    // `return` inside a sourced script stops sourcing and sets the exit status.
    if let Some(code) = state.take_return() {
        outcome.exit_code = code;
    }
    Ok(outcome)
}

fn run_exec_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let command_index = if invocation.argv.get(1).is_some_and(|arg| arg == "--") {
        2
    } else {
        1
    };
    if invocation.argv.get(command_index).is_none() {
        apply_shell_assignments(&invocation.assignments, state);
        return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
    }

    if capture_outputs || stdin_data.is_some() || !allow_process_replacement {
        return Ok(CommandOutcome::captured(
            126,
            Vec::new(),
            b"exec: process replacement disabled in this execution context\n".to_vec(),
        ));
    }

    let exec_invocation = ExpandedInvocation {
        assignments: invocation.assignments.clone(),
        argv: invocation.argv[command_index..].to_vec(),
        redirections: invocation.redirections.clone(),
    };
    let name = exec_invocation.argv[0].as_str();
    if let Some(policy) = state.confine_policy() {
        if !policy.allows(name) {
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            return Ok(CommandOutcome::captured(
                126,
                Vec::new(),
                format!(
                    "agsh: {base}: not permitted in this confined session (allowed: {})\n",
                    policy.display_list()
                )
                .into_bytes(),
            ));
        }
    }
    let Some(path) = resolve_external_path(&exec_invocation, state, name) else {
        return Ok(CommandOutcome::captured(
            127,
            Vec::new(),
            format!("exec: {name}: command not found\n").into_bytes(),
        ));
    };

    exec_external(&exec_invocation, state, path.as_path())
}

#[cfg(unix)]
fn exec_external(
    invocation: &ExpandedInvocation,
    state: &ShellState,
    command_path: &Path,
) -> Result<CommandOutcome, ShellError> {
    let mut command = Command::new(command_path);
    command.args(&invocation.argv[1..]);
    command.current_dir(state.cwd());
    command.env_clear();
    command.envs(state.exported_env());
    for assignment in &invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut stdin_is_piped = false;
    let mut merge_stderr_to_stdout = false;
    let mut merge_stdout_to_stderr = false;
    apply_external_redirections(
        &mut command,
        &invocation.redirections,
        &mut stdin_is_piped,
        &mut merge_stderr_to_stdout,
        &mut merge_stdout_to_stderr,
        false,
        state.noclobber(),
    )?;

    let error = command.exec();
    Err(ShellError::execution(format!(
        "exec: {}: {error}",
        invocation.argv[0]
    )))
}

#[cfg(not(unix))]
fn exec_external(
    _invocation: &ExpandedInvocation,
    _state: &ShellState,
    _command_path: &Path,
) -> Result<CommandOutcome, ShellError> {
    Err(ShellError::unsupported(
        "exec: process replacement is only supported on Unix",
    ))
}

/// `patch <file>`: collect the unified diff from stdin (heredoc redirection via
/// `stdin_data`, or an in-shell pipe / inherited stdin drained line-by-line) and
/// apply it.
fn run_patch_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
) -> Result<CommandOutcome, ShellError> {
    let diff = if let Some(bytes) = stdin_data {
        Some(bytes.to_vec())
    } else {
        let mut buf = String::new();
        let mut any = false;
        while let Some(line) = read_one_line(None, state)? {
            any = true;
            buf.push_str(&line);
        }
        any.then_some(buf.into_bytes())
    };
    Ok(crate::agent::patch(
        &invocation.argv[1..],
        state,
        diff.as_deref(),
    ))
}

fn run_read_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
) -> Result<CommandOutcome, ShellError> {
    let mut raw = false;
    let mut prompt = None;
    let mut names = Vec::new();
    let mut args = invocation.argv.iter().skip(1).peekable();
    while let Some(arg) = args.next() {
        if arg == "-r" {
            raw = true;
        } else if arg == "-p" {
            let Some(prompt_arg) = args.next() else {
                return Ok(CommandOutcome::captured(
                    2,
                    Vec::new(),
                    b"read: -p requires a prompt\n".to_vec(),
                ));
            };
            prompt = Some(prompt_arg.clone());
        } else if let Some(prompt_arg) = arg.strip_prefix("-p") {
            prompt = Some(prompt_arg.to_string());
        } else if arg.starts_with('-') {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                format!("read: unsupported option: {arg}\n").into_bytes(),
            ));
        } else if !is_identifier(arg) {
            return Ok(CommandOutcome::captured(
                2,
                Vec::new(),
                format!("read: invalid identifier: {arg}\n").into_bytes(),
            ));
        } else {
            names.push(arg.clone());
        }
    }

    if names.is_empty() {
        names.push("REPLY".to_string());
    }

    if let Some(prompt) = prompt {
        if stdin_data.is_none() && io::stdin().is_terminal() {
            let mut stderr = io::stderr();
            stderr.write_all(prompt.as_bytes())?;
            stderr.flush()?;
        }
    }

    let Some(mut line) = read_logical_line(stdin_data, state, raw)? else {
        return Ok(CommandOutcome::captured(1, Vec::new(), Vec::new()));
    };
    trim_line_ending(&mut line);
    if !raw {
        line = unescape_read_line(&line);
    }
    assign_read_fields(state, &names, &line);
    Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()))
}

fn read_logical_line(
    stdin_data: Option<&[u8]>,
    state: &mut ShellState,
    raw: bool,
) -> Result<Option<String>, ShellError> {
    if raw {
        return read_one_line(stdin_data, state);
    }

    if let Some(input) = stdin_data {
        return Ok(read_logical_line_from_buffer(input));
    }

    let mut logical = String::new();
    loop {
        let Some(mut line) = read_one_line(None, state)? else {
            return if logical.is_empty() {
                Ok(None)
            } else {
                Ok(Some(logical))
            };
        };
        let continued = remove_read_continuation(&mut line);
        logical.push_str(&line);
        if !continued {
            return Ok(Some(logical));
        }
    }
}

fn read_logical_line_from_buffer(input: &[u8]) -> Option<String> {
    if input.is_empty() {
        return None;
    }

    let mut logical = String::new();
    let mut start = 0;
    while start < input.len() {
        let end = input[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(input.len(), |index| start + index + 1);
        let mut line = String::from_utf8_lossy(&input[start..end]).to_string();
        start = end;

        let continued = remove_read_continuation(&mut line);
        logical.push_str(&line);
        if !continued {
            break;
        }
    }

    Some(logical)
}

fn read_one_line(
    stdin_data: Option<&[u8]>,
    state: &mut ShellState,
) -> Result<Option<String>, ShellError> {
    if let Some(input) = stdin_data {
        if input.is_empty() {
            return Ok(None);
        }
        let end = input
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(input.len(), |index| index + 1);
        return Ok(Some(String::from_utf8_lossy(&input[..end]).to_string()));
    }

    if let Some(line) = state.read_shell_stdin_line() {
        return Ok(line?);
    }

    let mut line = String::new();
    let bytes = io::stdin().read_line(&mut line)?;
    if bytes == 0 {
        Ok(None)
    } else {
        Ok(Some(line))
    }
}

fn remove_read_continuation(line: &mut String) -> bool {
    let Some(ending_start) = line_ending_start(line) else {
        return false;
    };

    let backslashes = line[..ending_start]
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'\\')
        .count();
    if backslashes % 2 == 0 {
        return false;
    }

    line.replace_range(ending_start - 1.., "");
    true
}

fn line_ending_start(line: &str) -> Option<usize> {
    let mut ending_start = line.len().checked_sub(1)?;
    if !line.ends_with('\n') {
        return None;
    }
    if line[..ending_start].ends_with('\r') {
        ending_start -= 1;
    }
    Some(ending_start)
}

fn trim_line_ending(line: &mut String) {
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
}

fn unescape_read_line(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn assign_read_fields(state: &mut ShellState, names: &[String], line: &str) {
    if names.len() == 1 {
        state.set_var(&names[0], line);
        return;
    }

    let ifs = state.lookup("IFS").unwrap_or(" \t\n").to_string();
    let values = split_read_fields(line, names.len(), &ifs);
    for (name, value) in names.iter().zip(values) {
        state.set_var(name, value);
    }
}

fn split_read_fields(line: &str, count: usize, ifs: &str) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![line.to_string()];
    }
    if ifs.is_empty() {
        let mut values = Vec::with_capacity(count);
        values.push(line.to_string());
        values.resize(count, String::new());
        return values;
    }

    let mut values = Vec::with_capacity(count);
    let mut rest = line;
    for _ in 0..count - 1 {
        let (field, next_rest) = read_next_field(rest, ifs);
        values.push(field);
        rest = next_rest;
    }
    values.push(trim_read_remainder(rest, ifs).to_string());
    values
}

fn read_next_field<'a>(input: &'a str, ifs: &str) -> (String, &'a str) {
    let input = trim_start_ifs_whitespace(input, ifs);
    if input.is_empty() {
        return (String::new(), "");
    }

    for (index, ch) in input.char_indices() {
        if !is_ifs_delimiter(ch, ifs) {
            continue;
        }

        if is_ifs_whitespace(ch, ifs) {
            let after_ws = skip_ifs_whitespace(input, index, ifs);
            if let Some((_, delimiter)) = first_char(&input[after_ws..]) {
                if is_ifs_non_whitespace_delimiter(delimiter, ifs) {
                    let rest_start =
                        skip_ifs_whitespace(input, after_ws + delimiter.len_utf8(), ifs);
                    return (input[..index].to_string(), &input[rest_start..]);
                }
            }
            return (input[..index].to_string(), &input[after_ws..]);
        }

        let field_end = trim_end_ifs_whitespace_index(&input[..index], ifs);
        let rest_start = skip_ifs_whitespace(input, index + ch.len_utf8(), ifs);
        return (input[..field_end].to_string(), &input[rest_start..]);
    }

    (trim_end_ifs_whitespace(input, ifs).to_string(), "")
}

fn trim_read_remainder<'a>(input: &'a str, ifs: &str) -> &'a str {
    let input = trim_start_ifs_whitespace(input, ifs);
    let input = trim_end_ifs_whitespace(input, ifs);
    if let Some(index) = trailing_single_non_whitespace_ifs(input, ifs) {
        trim_end_ifs_whitespace(&input[..index], ifs)
    } else {
        input
    }
}

fn skip_ifs_whitespace(input: &str, start: usize, ifs: &str) -> usize {
    let mut next = start;
    for (offset, ch) in input[start..].char_indices() {
        if !is_ifs_whitespace(ch, ifs) {
            return start + offset;
        }
        next = start + offset + ch.len_utf8();
    }
    next
}

fn trim_start_ifs_whitespace<'a>(input: &'a str, ifs: &str) -> &'a str {
    &input[skip_ifs_whitespace(input, 0, ifs)..]
}

fn trim_end_ifs_whitespace<'a>(input: &'a str, ifs: &str) -> &'a str {
    &input[..trim_end_ifs_whitespace_index(input, ifs)]
}

fn trim_end_ifs_whitespace_index(input: &str, ifs: &str) -> usize {
    let mut end = input.len();
    for (index, ch) in input.char_indices().rev() {
        if !is_ifs_whitespace(ch, ifs) {
            break;
        }
        end = index;
    }
    end
}

fn trailing_single_non_whitespace_ifs(input: &str, ifs: &str) -> Option<usize> {
    let (last_index, last) = input.char_indices().next_back()?;
    if !is_ifs_non_whitespace_delimiter(last, ifs) {
        return None;
    }
    let previous = input[..last_index].chars().next_back();
    if previous.is_some_and(|ch| is_ifs_non_whitespace_delimiter(ch, ifs)) {
        None
    } else {
        Some(last_index)
    }
}

fn first_char(input: &str) -> Option<(usize, char)> {
    input.char_indices().next()
}

fn is_ifs_non_whitespace_delimiter(ch: char, ifs: &str) -> bool {
    is_ifs_delimiter(ch, ifs) && !is_ifs_whitespace(ch, ifs)
}

fn run_shell_source(
    source: &str,
    state: &mut ShellState,
    output_mode: OutputMode,
    capture_outputs: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let nested_options = ExecutionOptions {
        output_mode: if capture_outputs {
            OutputMode::Clean
        } else {
            output_mode
        },
        allow_process_replacement,
    };
    let mut final_outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    for line in source_logical_lines(source)? {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let graph = parse_line(&line)?;
        let mut executor = Executor::new();
        let mut outcome = executor.run_graph(&graph, state, &nested_options)?;
        final_outcome.exit_code = outcome.exit_code;
        final_outcome.stdout.append(&mut outcome.stdout);
        final_outcome.stderr.append(&mut outcome.stderr);

        if state.should_exit()
            || state.loop_control_requested()
            || state.return_requested()
            || state.interrupted()
            || state.stream_pipe_closed()
        {
            break;
        }

        if state.errexit() && outcome.exit_code != 0 {
            break;
        }
    }

    Ok(final_outcome)
}

fn source_logical_lines(source: &str) -> Result<Vec<String>, ShellError> {
    let physical_lines = source_physical_logical_lines(source);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut function_body_depth = 0usize;
    let mut if_block_depth = 0usize;
    let mut while_block_depth = 0usize;
    let mut for_block_depth = 0usize;
    let mut case_block_depth = 0usize;

    for line in physical_lines {
        let trimmed_end = line.trim_end_matches(['\r', '\n']);
        let trimmed = trimmed_end.trim();

        if function_body_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                function_body_depth =
                    function_body_depth.saturating_add_signed(brace_delta(trimmed_end)?);
            }
            if function_body_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if if_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                if_block_depth = if_block_depth.saturating_add_signed(if_delta(trimmed_end)?);
            }
            if if_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if while_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                while_block_depth =
                    while_block_depth.saturating_add_signed(while_delta(trimmed_end)?);
            }
            if while_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if for_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                for_block_depth = for_block_depth.saturating_add_signed(done_delta(trimmed_end)?);
            }
            if for_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if case_block_depth > 0 {
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                current.push_str("; ");
                current.push_str(trimmed_end);
                case_block_depth = case_block_depth.saturating_add_signed(case_delta(trimmed_end)?);
            }
            if case_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
            continue;
        }

        if let Some(depth) = function_definition_depth(trimmed_end)? {
            current.push_str(trimmed_end);
            function_body_depth = depth;
            if function_body_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = if_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            if_block_depth = depth;
            if if_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = while_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            while_block_depth = depth;
            if while_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = for_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            for_block_depth = depth;
            if for_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else if let Some(depth) = case_block_depth_for_line(trimmed_end)? {
            current.push_str(trimmed_end);
            case_block_depth = depth;
            if case_block_depth == 0 {
                lines.push(std::mem::take(&mut current));
            }
        } else {
            lines.push(line);
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    Ok(lines)
}

fn source_physical_logical_lines(source: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();

    for physical in source.split_inclusive('\n') {
        let mut line = physical.to_string();
        let continued = remove_read_continuation(&mut line);
        current.push_str(&line);
        if !continued {
            lines.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }

    lines
}

fn function_definition_depth(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    let Some(open_index) = function_definition_open_index(&tokens) else {
        return Ok(None);
    };

    let mut depth = 0usize;
    for token in &tokens[open_index..] {
        update_brace_depth(token, &mut depth);
    }
    Ok(Some(depth))
}

fn function_definition_open_index(tokens: &[agsh_core::lexer::Token]) -> Option<usize> {
    for index in 0..tokens.len() {
        if tokens[index].text != "{" || tokens[index].quote != QuoteKind::None {
            continue;
        }
        if index == 1 && tokens[0].text.ends_with("()") && tokens[0].quote == QuoteKind::None {
            return Some(index);
        }
        if index == 2
            && tokens[0].text == "function"
            && tokens[0].quote == QuoteKind::None
            && tokens[1].quote == QuoteKind::None
        {
            return Some(index);
        }
    }
    None
}

fn if_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens
        .first()
        .is_some_and(|token| token.text == "if" && token.quote == QuoteKind::None)
    {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_if_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn while_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens.first().is_some_and(|token| {
        matches!(token.text.as_str(), "while" | "until") && token.quote == QuoteKind::None
    }) {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_while_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn for_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens.first().is_some_and(|token| {
        matches!(token.text.as_str(), "for" | "select") && token.quote == QuoteKind::None
    }) {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_done_block_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn case_block_depth_for_line(line: &str) -> Result<Option<usize>, ShellError> {
    let tokens = lex(line)?;
    if !tokens
        .first()
        .is_some_and(|token| token.text == "case" && token.quote == QuoteKind::None)
    {
        return Ok(None);
    }

    let mut depth = 0usize;
    for index in 0..tokens.len() {
        update_case_depth(
            &tokens[index],
            &mut depth,
            is_source_reserved_command_position(&tokens, index),
        );
    }
    Ok(Some(depth))
}

fn if_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    let tokens = lex(line)?;
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if token.quote != QuoteKind::None {
            continue;
        }
        if !is_source_reserved_command_position(&tokens, index) {
            continue;
        }
        match token.text.as_str() {
            "if" => delta += 1,
            "fi" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn case_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    let tokens = lex(line)?;
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if token.quote != QuoteKind::None {
            continue;
        }
        if !is_source_reserved_command_position(&tokens, index) {
            continue;
        }
        match token.text.as_str() {
            "case" => delta += 1,
            "esac" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn while_delta(line: &str) -> Result<isize, ShellError> {
    done_delta(line)
}

fn done_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    let tokens = lex(line)?;
    for index in 0..tokens.len() {
        let token = &tokens[index];
        if token.quote != QuoteKind::None {
            continue;
        }
        if !is_source_reserved_command_position(&tokens, index) {
            continue;
        }
        match token.text.as_str() {
            "while" | "until" | "for" | "select" => delta += 1,
            "done" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn brace_delta(line: &str) -> Result<isize, ShellError> {
    let mut delta = 0isize;
    for token in lex(line)? {
        if token.quote != QuoteKind::None {
            continue;
        }
        match token.text.as_str() {
            "{" => delta += 1,
            "}" => delta -= 1,
            _ => {}
        }
    }
    Ok(delta)
}

fn update_if_depth(token: &agsh_core::lexer::Token, depth: &mut usize, command_position: bool) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "if" if command_position => *depth += 1,
        "fi" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn update_while_depth(token: &agsh_core::lexer::Token, depth: &mut usize, command_position: bool) {
    update_done_block_depth(token, depth, command_position);
}

fn update_done_block_depth(
    token: &agsh_core::lexer::Token,
    depth: &mut usize,
    command_position: bool,
) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "while" | "until" | "for" | "select" if command_position => *depth += 1,
        "done" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn update_case_depth(token: &agsh_core::lexer::Token, depth: &mut usize, command_position: bool) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "case" if command_position => *depth += 1,
        "esac" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn is_source_reserved_command_position(tokens: &[agsh_core::lexer::Token], index: usize) -> bool {
    let Some(previous) = tokens[..index]
        .iter()
        .rev()
        .find(|token| token.quote == QuoteKind::None)
    else {
        return true;
    };

    matches!(
        previous.text.as_str(),
        ";" | "&&" | "||" | "|" | "then" | "else" | "elif" | "do"
    )
}

fn update_brace_depth(token: &agsh_core::lexer::Token, depth: &mut usize) {
    if token.quote != QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "{" => *depth += 1,
        "}" => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn resolve_external_path(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    name: &str,
) -> Option<PathBuf> {
    let path_value = lookup_path_value(invocation, state);
    resolve_external_path_with(invocation, state, name, &path_value)
}

fn resolve_external_path_with(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    name: &str,
    path_value: &str,
) -> Option<PathBuf> {
    // Confinement backstop: a denied external command must not resolve to a path,
    // even via routes that skip preflight (e.g. `exec`). The friendly deny
    // message comes from preflight_resolved_invocation; this is the hard net.
    if let Some(policy) = state.confine_policy() {
        if !policy.allows(name) {
            return None;
        }
    }

    let uses_temporary_path = invocation
        .assignments
        .iter()
        .any(|assignment| assignment.name == "PATH");

    if !uses_temporary_path && !name.contains('/') {
        if let Some(path) = state.cached_path_lookup(path_value, name) {
            return Some(path);
        }
    }

    let resolver = Resolver::default();
    let path = match resolver.resolve_external_only(name, Some(path_value)) {
        Some(CommandResolution::External(path)) => path,
        _ => return None,
    };

    if !uses_temporary_path && !name.contains('/') {
        state.cache_path_lookup(path_value, name, path.clone());
    }
    Some(path)
}

fn lookup_path_value(invocation: &ExpandedInvocation, state: &ShellState) -> String {
    invocation
        .assignments
        .iter()
        .rev()
        .find(|assignment| assignment.name == "PATH")
        .map(|assignment| assignment.value.as_str())
        .or_else(|| state.lookup("PATH"))
        .unwrap_or_default()
        .to_string()
}

fn expand_alias_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
) -> Result<Option<ExpandedInvocation>, ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(None);
    };
    let Some(alias_value) = state.alias(name).map(str::to_string) else {
        return Ok(None);
    };

    let graph = parse_line(&alias_value)?;
    if graph.pipeline.commands.len() != 1 {
        return Err(ShellError::unsupported(format!(
            "{name}: aliases that expand to pipelines are not implemented"
        )));
    }

    let mut expanded = expand_invocation(&graph.pipeline.commands[0], state)?;
    if expanded.argv.is_empty() {
        return Ok(None);
    }

    expanded
        .assignments
        .splice(0..0, invocation.assignments.clone());
    expanded
        .argv
        .extend(invocation.argv.iter().skip(1).cloned());
    expanded
        .redirections
        .extend(invocation.redirections.clone());
    Ok(Some(expanded))
}

fn expand_abbreviation_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
) -> Result<Option<ExpandedInvocation>, ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(None);
    };
    let Some(abbreviation_value) = state.abbreviation(name).map(str::to_string) else {
        return Ok(None);
    };

    let graph = parse_line(&abbreviation_value)?;
    if graph.pipeline.commands.len() != 1 {
        return Err(ShellError::unsupported(format!(
            "{name}: abbreviations that expand to pipelines are not implemented"
        )));
    }

    let mut expanded = expand_invocation(&graph.pipeline.commands[0], state)?;
    if expanded.argv.is_empty() {
        return Ok(None);
    }

    expanded
        .assignments
        .splice(0..0, invocation.assignments.clone());
    expanded
        .argv
        .extend(invocation.argv.iter().skip(1).cloned());
    expanded
        .redirections
        .extend(invocation.redirections.clone());
    Ok(Some(expanded))
}

fn strip_wrapper(
    invocation: &ExpandedInvocation,
    wrapper: &str,
) -> Result<ExpandedInvocation, ShellError> {
    if invocation.argv.len() < 2 {
        return Err(ShellError::execution(format!("{wrapper}: missing command")));
    }
    let mut stripped = invocation.clone();
    stripped.argv.remove(0);
    if wrapper == "command" && stripped.argv.first().is_some_and(|arg| arg == "--") {
        stripped.argv.remove(0);
        if stripped.argv.is_empty() {
            return Err(ShellError::execution("command: missing command"));
        }
    }
    Ok(stripped)
}

#[derive(Debug, Clone, Copy)]
struct CommandOptions {
    default_path: bool,
    describe: bool,
    unsupported_option: Option<usize>,
    command_index: usize,
}

fn parse_command_options(invocation: &ExpandedInvocation) -> Result<CommandOptions, ShellError> {
    let mut options = CommandOptions {
        default_path: false,
        describe: false,
        unsupported_option: None,
        command_index: 1,
    };

    while let Some(arg) = invocation.argv.get(options.command_index) {
        match arg.as_str() {
            "--" => {
                options.command_index += 1;
                break;
            }
            "-p" => options.default_path = true,
            "-v" | "-V" => options.describe = true,
            other if other.starts_with('-') => {
                options.unsupported_option = Some(options.command_index);
                break;
            }
            _ => break,
        }
        options.command_index += 1;
    }

    if !options.describe
        && options.unsupported_option.is_none()
        && invocation.argv.get(options.command_index).is_none()
    {
        return Err(ShellError::execution("command: missing command"));
    }
    Ok(options)
}

fn strip_command_wrapper(
    invocation: &ExpandedInvocation,
    command_index: usize,
) -> Result<ExpandedInvocation, ShellError> {
    let mut stripped = invocation.clone();
    stripped.argv.drain(0..command_index);
    if stripped.argv.is_empty() {
        return Err(ShellError::execution("command: missing command"));
    }
    Ok(stripped)
}

fn preflight_buffered_pipeline_invocations(
    commands: &[ExpandedInvocation],
    state: &mut ShellState,
) -> Result<(), ShellError> {
    for command in commands {
        preflight_invocation(command, state, LookupMode::Normal, 0)?;
    }
    Ok(())
}

fn preflight_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    lookup_mode: LookupMode,
    alias_depth: usize,
) -> Result<(), ShellError> {
    let Some(name) = invocation.argv.first() else {
        return Ok(());
    };

    match name.as_str() {
        "command" if lookup_mode == LookupMode::Normal => {
            let command_options = parse_command_options(invocation)?;
            if command_options.describe || command_options.unsupported_option.is_some() {
                return Ok(());
            }
            let wrapped = strip_command_wrapper(invocation, command_options.command_index)?;
            preflight_invocation(
                &wrapped,
                state,
                if command_options.default_path {
                    LookupMode::DefaultPath
                } else {
                    LookupMode::BypassAliases
                },
                alias_depth,
            )
        }
        "external" if lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "external")?;
            preflight_invocation(&wrapped, state, LookupMode::ExternalOnly, alias_depth)
        }
        "builtin" if lookup_mode == LookupMode::Normal => {
            let wrapped = strip_wrapper(invocation, "builtin")?;
            preflight_invocation(&wrapped, state, LookupMode::BuiltinOnly, alias_depth)
        }
        _ if lookup_mode == LookupMode::Normal => {
            if state.function(name).is_some() {
                return Ok(());
            }

            if let Some(expanded) = expand_alias_invocation(invocation, state)? {
                if alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: alias expansion limit exceeded"
                    )));
                }
                return preflight_invocation(&expanded, state, lookup_mode, alias_depth + 1);
            }
            if let Some(expanded) = expand_abbreviation_invocation(invocation, state)? {
                if alias_depth >= MAX_ALIAS_EXPANSIONS {
                    return Err(ShellError::execution(format!(
                        "{name}: abbreviation expansion limit exceeded"
                    )));
                }
                return preflight_invocation(&expanded, state, lookup_mode, alias_depth + 1);
            }
            preflight_resolved_invocation(invocation, state, lookup_mode)
        }
        _ => preflight_resolved_invocation(invocation, state, lookup_mode),
    }
}

fn preflight_resolved_invocation(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
    lookup_mode: LookupMode,
) -> Result<(), ShellError> {
    let Some(name) = invocation.argv.first().map(String::as_str) else {
        return Ok(());
    };

    // Confinement gate: in a confined session, deny any external command not on
    // the allowlist. Builtins are exempt here — they are the shell's own surface
    // and funnel any external targets (exec/command/source) back through the
    // gated resolver below. eval/subshell/`$(…)` re-enter this preflight, so
    // they are covered too.
    if let Some(policy) = state.confine_policy() {
        if !is_builtin(name) && !policy.allows(name) {
            let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
            return Err(ShellError::denied(format!(
                "{base}: not permitted in this confined session (allowed: {})",
                policy.display_list()
            )));
        }
    }

    match lookup_mode {
        LookupMode::BuiltinOnly => {
            if is_builtin(name) {
                Ok(())
            } else {
                Err(ShellError::not_found(format!("{name}: builtin not found")))
            }
        }
        LookupMode::ExternalOnly => {
            if resolve_external_path(invocation, state, name).is_some() {
                Ok(())
            } else {
                Err(ShellError::not_found(format!(
                    "{name}: external command not found"
                )))
            }
        }
        LookupMode::Normal | LookupMode::BypassAliases | LookupMode::DefaultPath => {
            if is_builtin(name) {
                return Ok(());
            }

            let path = if lookup_mode == LookupMode::DefaultPath {
                resolve_external_path_with(invocation, state, name, DEFAULT_COMMAND_PATH)
            } else {
                resolve_external_path(invocation, state, name)
            };
            if path.is_some() {
                Ok(())
            } else {
                Err(ShellError::not_found(format!("{name}: command not found")))
            }
        }
    }
}

fn to_command_invocation(invocation: &ExpandedInvocation) -> CommandInvocation {
    CommandInvocation::new(
        invocation.assignments.clone(),
        invocation.argv.clone(),
        vec![QuoteKind::None; invocation.argv.len()],
        invocation
            .argv
            .iter()
            .map(|arg| vec![WordSegment::new(arg.clone(), QuoteKind::None)])
            .collect(),
        Vec::new(),
        None,
    )
}

/// Build a compaction context from the shell state and token config: shorten
/// the user's home and the working directory, redact the values of configured
/// secret env vars, apply the configured budget, and attach any `[[compactor]]`
/// that matches this command's argv.
fn compaction_context(state: &ShellState, argv: &[String]) -> CompactionContext {
    let config = state.output_config();
    let home = state.lookup("HOME").map(str::to_string);
    let workspace = Some(state.cwd().display().to_string());
    let literal_secrets = config
        .security
        .redact_env_names
        .iter()
        .filter_map(|name| state.lookup(name).map(str::to_string))
        .filter(|v| !v.is_empty())
        .collect::<Vec<_>>();
    CompactionContext {
        normalize: config.normalize_options(home, workspace),
        redact: config.redact_options(literal_secrets),
        budget: config.budget_options(),
        compactor: config.matching_compactor(argv).cloned(),
    }
}

/// Evaluate a `[[ ... ]]` conditional. `args` are the already-expanded operands
/// after `[[` (no glob/word-split was applied), including a trailing `]]`.
/// Supports `&&`/`||`/`!`/`( )`, unary file/string tests, integer comparisons,
/// `==`/`!=` glob patterns, `=~` regex, and `<`/`>` string ordering.
pub(crate) fn eval_double_bracket(args: &[String], state: &mut ShellState) -> CommandOutcome {
    let mut toks: Vec<&str> = args.iter().map(String::as_str).collect();
    if toks.last() == Some(&"]]") {
        toks.pop();
    }
    let mut parser = DoubleBracket {
        toks: &toks,
        pos: 0,
        cwd: state.cwd().to_path_buf(),
        rematch: None,
    };
    let outcome = match parser.or_expr() {
        Ok(value) if parser.pos == toks.len() => {
            CommandOutcome::captured(i32::from(!value), Vec::new(), Vec::new())
        }
        Ok(_) => cond_error(format!("[[: unexpected token near `{}`", toks[parser.pos])),
        Err(msg) => cond_error(msg),
    };
    // Populate $BASH_REMATCH if a `=~` was evaluated.
    if let Some(groups) = parser.rematch {
        state.set_array("BASH_REMATCH", groups, false);
    }
    outcome
}

fn cond_error(msg: String) -> CommandOutcome {
    CommandOutcome::captured(2, Vec::new(), format!("{msg}\n").into_bytes())
}

struct DoubleBracket<'a> {
    toks: &'a [&'a str],
    pos: usize,
    cwd: std::path::PathBuf,
    /// Captures from the last `=~` match, written to `$BASH_REMATCH` afterward.
    rematch: Option<Vec<String>>,
}

impl<'a> DoubleBracket<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.toks.get(self.pos).copied()
    }
    fn advance(&mut self) -> Option<&'a str> {
        let t = self.toks.get(self.pos).copied();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }
    fn or_expr(&mut self) -> Result<bool, String> {
        let mut value = self.and_expr()?;
        while self.peek() == Some("||") {
            self.pos += 1;
            let rhs = self.and_expr()?;
            value = value || rhs;
        }
        Ok(value)
    }
    fn and_expr(&mut self) -> Result<bool, String> {
        let mut value = self.unary()?;
        while self.peek() == Some("&&") {
            self.pos += 1;
            let rhs = self.unary()?;
            value = value && rhs;
        }
        Ok(value)
    }
    fn unary(&mut self) -> Result<bool, String> {
        match self.peek() {
            Some("!") => {
                self.pos += 1;
                Ok(!self.unary()?)
            }
            Some("(") => {
                self.pos += 1;
                let value = self.or_expr()?;
                if self.advance() != Some(")") {
                    return Err("[[: expected `)`".to_string());
                }
                Ok(value)
            }
            _ => self.primary(),
        }
    }
    fn primary(&mut self) -> Result<bool, String> {
        let tok = self.peek().ok_or("[[: unexpected end of expression")?;
        if is_unary_cond_op(tok) {
            self.pos += 1;
            let operand = self
                .advance()
                .ok_or_else(|| format!("[[: {tok}: operand expected"))?;
            return Ok(test_bool(
                &[tok.to_string(), operand.to_string()],
                &self.cwd,
            ));
        }
        let a = self.advance().expect("peeked Some");
        if let Some(op) = self.peek() {
            if is_binary_cond_op(op) {
                self.pos += 1;
                let b = self
                    .advance()
                    .ok_or_else(|| format!("[[: {op}: operand expected"))?;
                return self.binary(a, op, b);
            }
        }
        // A lone word is true when non-empty (like `[ word ]`).
        Ok(!a.is_empty())
    }
    fn binary(&mut self, a: &str, op: &str, b: &str) -> Result<bool, String> {
        match op {
            "==" | "=" => Ok(glob_match_bytes(b.as_bytes(), a.as_bytes())),
            "!=" => Ok(!glob_match_bytes(b.as_bytes(), a.as_bytes())),
            "=~" => {
                let re = regex::Regex::new(b).map_err(|_| format!("[[: `{b}`: invalid regex"))?;
                match re.captures(a) {
                    // $BASH_REMATCH: [0]=whole match, [1..]=capture groups.
                    Some(caps) => {
                        self.rematch = Some(
                            caps.iter()
                                .map(|m| m.map(|x| x.as_str().to_string()).unwrap_or_default())
                                .collect(),
                        );
                        Ok(true)
                    }
                    None => {
                        self.rematch = Some(Vec::new());
                        Ok(false)
                    }
                }
            }
            "<" => Ok(a < b),
            ">" => Ok(a > b),
            _ => Ok(test_bool(
                &[a.to_string(), op.to_string(), b.to_string()],
                &self.cwd,
            )),
        }
    }
}

fn is_unary_cond_op(tok: &str) -> bool {
    matches!(
        tok,
        "-f" | "-d"
            | "-e"
            | "-r"
            | "-w"
            | "-x"
            | "-s"
            | "-L"
            | "-h"
            | "-b"
            | "-c"
            | "-p"
            | "-S"
            | "-z"
            | "-n"
            | "-t"
            | "-g"
            | "-u"
            | "-k"
            | "-O"
            | "-G"
            | "-N"
    )
}

fn is_binary_cond_op(tok: &str) -> bool {
    matches!(
        tok,
        "==" | "="
            | "!="
            | "=~"
            | "<"
            | ">"
            | "-eq"
            | "-ne"
            | "-lt"
            | "-le"
            | "-gt"
            | "-ge"
            | "-nt"
            | "-ot"
            | "-ef"
    )
}

fn test_bool(args: &[String], cwd: &std::path::Path) -> bool {
    crate::builtins::builtin_test(args, "[[", cwd).exit_code == 0
}

/// The argv of the first command in a graph, used to classify and match the
/// command for observation rendering. Falls back to the raw source line.
fn graph_primary_argv(graph: &CommandGraph) -> Vec<String> {
    graph
        .list
        .items
        .first()
        .and_then(|item| item.pipeline.commands.first())
        .map(|cmd| cmd.argv.clone())
        .filter(|argv| !argv.is_empty())
        .unwrap_or_else(|| vec![graph.source.clone()])
}

/// Build a `rich` observation: render the captured stdout by detected type for
/// human display (only on a TTY); the raw bytes remain available via the trace
/// ref and still flow to pipes/redirects (handled separately). stderr appended.
fn rich_observation(
    state: &ShellState,
    cmd_id: &agsh_core::CommandId,
    argv: &[String],
    stdout: &[u8],
    stderr: &[u8],
) -> Option<OutputObservation> {
    use std::io::IsTerminal;
    // Rich rendering is a human-display transform only. When stdout is a pipe or
    // redirect, emit NO observation so the caller writes the raw bytes verbatim —
    // otherwise a lossy UTF-8 round-trip would corrupt images/binaries (e.g.
    // `view image.png > copy.png`).
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let theme = state.theme();
    let mut display = agsh_render::render(
        stdout,
        rich_hint(argv).as_deref(),
        &theme,
        rich_term_width(),
    );
    if !stderr.is_empty() {
        display.push_str(&String::from_utf8_lossy(stderr));
    }
    Some(OutputObservation {
        token_estimate: agsh_output::estimate_tokens(&display),
        display,
        raw: Some(agsh_output::RawStreamRef {
            stdout: format!("trace://{cmd_id}/stdout"),
            stderr: format!("trace://{cmd_id}/stderr"),
        }),
    })
}

/// A filename hint for type detection: the last path-like argument with an
/// extension (helps disambiguate markdown, whose content is ambiguous).
fn rich_hint(argv: &[String]) -> Option<String> {
    argv.iter()
        .skip(1)
        .rfind(|a| !a.starts_with('-') && a.contains('.'))
        .cloned()
}

/// Terminal width for rich rendering (columns), defaulting to 100.
fn rich_term_width() -> usize {
    rustix::termios::tcgetwinsize(std::io::stdout())
        .map(|w| w.ws_col as usize)
        .ok()
        .filter(|&c| c > 0)
        .unwrap_or(100)
}

/// PTY broker: run an external command connected to a pseudo-terminal so it
/// sees a TTY on its stdio (e.g. for tools that change behavior under `isatty`).
/// The controller side is drained into the captured output.
fn run_under_pty(
    invocation: &ExpandedInvocation,
    state: &mut ShellState,
) -> Result<CommandOutcome, ShellError> {
    use rustix::fs::{Mode, OFlags};
    use rustix::pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags};

    let name = invocation.argv[0].as_str();
    let Some(path) = resolve_external_path(invocation, state, name) else {
        return Ok(command_not_found_outcome(name, state));
    };

    fn pty_err(label: &str, e: rustix::io::Errno) -> ShellError {
        ShellError::execution(format!("pty: {label}: {e}"))
    }
    let controller =
        openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(|e| pty_err("openpt", e))?;
    grantpt(&controller).map_err(|e| pty_err("grantpt", e))?;
    unlockpt(&controller).map_err(|e| pty_err("unlockpt", e))?;
    let peripheral_name = ptsname(&controller, Vec::new()).map_err(|e| pty_err("ptsname", e))?;
    let peripheral = rustix::fs::open(
        &peripheral_name,
        OFlags::RDWR | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(|e| pty_err("open pts", e))?;

    let mut command = Command::new(&path);
    command.args(&invocation.argv[1..]);
    command.current_dir(state.cwd());
    command.env_clear();
    command.envs(state.exported_env());
    for assignment in &invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }
    command.stdin(Stdio::from(peripheral.try_clone()?));
    command.stdout(Stdio::from(peripheral.try_clone()?));
    command.stderr(Stdio::from(peripheral));

    let mut child = command.spawn()?;
    // Drain the controller. Linux EOFs (EIO) the master when the peripheral
    // closes, but macOS may block, so read non-blocking and poll the child.
    rustix::io::ioctl_fionbio(&controller, true).map_err(|e| pty_err("nonblock", e))?;
    let mut reader = std::fs::File::from(controller);
    let mut output = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut exited: Option<std::process::ExitStatus> = None;
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&chunk[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if exited.is_none() {
                    exited = child.try_wait()?;
                }
                if exited.is_some() {
                    // Child is gone; all its output is already buffered. Drain
                    // whatever remains without blocking, then stop.
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    while let Ok(n) = reader.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        output.extend_from_slice(&chunk[..n]);
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    let status = match exited {
        Some(status) => status,
        None => child.wait()?,
    };
    Ok(CommandOutcome::captured(
        exit_status_code(status),
        output,
        Vec::new(),
    ))
}

/// Map a child's exit status to a shell exit code: its code, or 128+signal when
/// terminated by a signal (so a SIGINT-killed command reports 130, like bash).
fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        128
    }
}

/// Per-stream capture caps: keep the first [`CAPTURE_HEAD`] and last
/// [`CAPTURE_TAIL`] bytes of a captured stream. Everything else is drained (so
/// the child's pipe never blocks) and replaced by a one-line marker. This bounds
/// memory in the capturing output modes (`compact`/`semantic`/…) so an agent
/// running `cat huge.bin` or a runaway producer can't OOM the shell. Raw mode
/// (which inherits fds) and redirections to files are unaffected — this only
/// touches the observation-capture plane.
const CAPTURE_HEAD: usize = 1 << 20; // 1 MiB
const CAPTURE_TAIL: usize = 1 << 20; // 1 MiB

/// Read `reader` to EOF, retaining at most the first `CAPTURE_HEAD` and last
/// `CAPTURE_TAIL` bytes. Under the cap the bytes are returned exactly; over it, a
/// `… [agsh: N bytes elided …] …` marker separates head from tail. Always drains
/// the reader fully so the child isn't blocked on a full pipe.
fn read_capped(mut reader: impl Read) -> io::Result<Vec<u8>> {
    let mut head: Vec<u8> = Vec::new();
    let mut tail: Vec<u8> = Vec::new();
    let mut total: usize = 0;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        total += n;
        let bytes = &chunk[..n];
        if head.len() < CAPTURE_HEAD {
            let take = (CAPTURE_HEAD - head.len()).min(n);
            head.extend_from_slice(&bytes[..take]);
            tail.extend_from_slice(&bytes[take..]);
        } else {
            tail.extend_from_slice(bytes);
        }
        // Keep the tail bounded, trimming amortized-O(1) (only past 2×).
        if tail.len() > CAPTURE_TAIL * 2 {
            let cut = tail.len() - CAPTURE_TAIL;
            tail.drain(0..cut);
        }
    }
    if total <= CAPTURE_HEAD + CAPTURE_TAIL {
        // Nothing was dropped — head + tail is the exact stream.
        head.extend_from_slice(&tail);
        return Ok(head);
    }
    if tail.len() > CAPTURE_TAIL {
        let cut = tail.len() - CAPTURE_TAIL;
        tail.drain(0..cut);
    }
    let dropped = total - CAPTURE_HEAD - CAPTURE_TAIL;
    head.extend_from_slice(
        format!(
            "\n… [agsh: {dropped} bytes of output elided; {total} bytes total, \
             showing first {CAPTURE_HEAD} and last {CAPTURE_TAIL}] …\n"
        )
        .as_bytes(),
    );
    head.extend_from_slice(&tail);
    Ok(head)
}

fn run_external(
    invocation: &ExpandedInvocation,
    state: &ShellState,
    _output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    command_path: Option<&Path>,
) -> Result<CommandOutcome, ShellError> {
    let mut command = if let Some(command_path) = command_path {
        Command::new(command_path)
    } else {
        Command::new(&invocation.argv[0])
    };
    command.args(&invocation.argv[1..]);
    command.current_dir(state.cwd());
    command.env_clear();
    command.envs(state.exported_env());
    for assignment in &invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }

    // Heredocs/herestrings carry literal stdin bytes; an explicit stdin
    // redirection overrides an inherited pipe.
    let heredoc_bytes: Option<Vec<u8>> = invocation.redirections.iter().find_map(|redirection| {
        match (&redirection.mode, &redirection.target) {
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(bytes),
            ) => Some(bytes.clone()),
            _ => None,
        }
    });
    let stdin_data = heredoc_bytes.as_deref().or(stdin_data);

    let mut stdin_is_piped = stdin_data.is_some();
    if stdin_is_piped {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::inherit());
    }

    if capture_outputs {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    }

    let mut merge_stderr_to_stdout = false;
    let mut merge_stdout_to_stderr = false;
    apply_external_redirections(
        &mut command,
        &invocation.redirections,
        &mut stdin_is_piped,
        &mut merge_stderr_to_stdout,
        &mut merge_stdout_to_stderr,
        capture_outputs,
        state.noclobber(),
    )?;

    if capture_outputs {
        // P0-8: when this is a streaming pipeline stage (stdout is a downstream
        // pipe) and there are no redirections/merges to honor, hand the child's
        // stdout straight to that pipe. Output then flows incrementally with real
        // backpressure, and a consumer that exits early (`… | head`) closes the
        // pipe so the producer gets SIGPIPE — instead of being captured and run to
        // completion (or forever) first, which hung `{ yes; } | head`.
        if invocation.redirections.is_empty() && !merge_stderr_to_stdout && !merge_stdout_to_stderr
        {
            if let Some(writer) = state.streaming_stdout_writer() {
                command.stdout(Stdio::from(writer));
                let mut child = command.spawn()?;
                let stdin_writer = match (stdin_is_piped, stdin_data, child.stdin.take()) {
                    (true, Some(input), Some(mut stdin)) => {
                        let buf = input.to_vec();
                        Some(std::thread::spawn(move || -> io::Result<()> {
                            match stdin.write_all(&buf) {
                                Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
                                _ => Ok(()),
                            }
                        }))
                    }
                    _ => None,
                };
                // stdout streams to the pipe; only stderr is captured (bounded).
                let stderr = match child.stderr.take() {
                    Some(reader) => read_capped(reader)?,
                    None => Vec::new(),
                };
                if let Some(handle) = stdin_writer {
                    handle
                        .join()
                        .map_err(|_| ShellError::execution("stdin writer thread panicked"))??;
                }
                let status = child.wait()?;
                return Ok(CommandOutcome::captured(
                    exit_status_code(status),
                    Vec::new(),
                    stderr,
                ));
            }
        }

        let mut child = command.spawn()?;
        // Feed stdin from a separate thread so `wait_with_output` can drain the
        // child's stdout/stderr concurrently. Writing all of stdin first would
        // deadlock once a child that echoes its input fills the stdout pipe
        // (e.g. `x=$(cat <<< "$(seq 200000)")`). BrokenPipe is benign — the child
        // may exit before consuming all input (e.g. `head -c1`).
        let stdin_writer = match (stdin_is_piped, stdin_data, child.stdin.take()) {
            (true, Some(input), Some(mut stdin)) => {
                let buf = input.to_vec();
                Some(std::thread::spawn(move || -> io::Result<()> {
                    match stdin.write_all(&buf) {
                        Err(e) if e.kind() != io::ErrorKind::BrokenPipe => Err(e),
                        _ => Ok(()),
                    }
                    // `stdin` drops here, sending EOF.
                }))
            }
            _ => None,
        };
        // Drain stdout+stderr concurrently into *bounded* buffers (stderr on a
        // thread, stdout here) so a huge/streaming child can't OOM us the way
        // `wait_with_output`'s unbounded read could. Two streams still need two
        // readers to avoid a full-pipe deadlock.
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stderr_handle =
            stderr_pipe.map(|reader| std::thread::spawn(move || read_capped(reader)));
        let mut stdout = match stdout_pipe {
            Some(reader) => read_capped(reader)?,
            None => Vec::new(),
        };
        let mut stderr = match stderr_handle {
            Some(handle) => handle
                .join()
                .map_err(|_| ShellError::execution("stderr reader thread panicked"))??,
            None => Vec::new(),
        };
        if let Some(handle) = stdin_writer {
            handle
                .join()
                .map_err(|_| ShellError::execution("stdin writer thread panicked"))??;
        }
        let status = child.wait()?;
        if merge_stderr_to_stdout {
            stdout.extend_from_slice(&stderr);
            stderr.clear();
        }
        if merge_stdout_to_stderr {
            stderr.extend_from_slice(&stdout);
            stdout.clear();
        }
        Ok(CommandOutcome::captured(
            exit_status_code(status),
            stdout,
            stderr,
        ))
    } else {
        let mut child = command.spawn()?;
        if stdin_is_piped {
            if let (Some(input), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
                // stdout/stderr are inherited here (no deadlock), but a child can
                // still exit early — treat BrokenPipe as benign.
                match stdin.write_all(input) {
                    Err(e) if e.kind() != io::ErrorKind::BrokenPipe => return Err(e.into()),
                    _ => {}
                }
            }
        }
        let status = child.wait()?;
        Ok(CommandOutcome::captured(
            exit_status_code(status),
            Vec::new(),
            Vec::new(),
        ))
    }
}

fn run_pipeline(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    if let Some(outcome) =
        try_run_streaming_mixed_shell_stage_pipeline(graph, pipeline, state, options)?
    {
        return Ok(outcome);
    }

    if let Some(outcome) =
        try_run_streaming_external_shell_stage_pipeline(graph, pipeline, state, options)?
    {
        return Ok(outcome);
    }

    if let Some(outcome) =
        try_run_streaming_external_prefix_to_final_shell_command(graph, pipeline, state, options)?
    {
        return Ok(outcome);
    }

    if pipeline
        .commands
        .iter()
        .any(command_invocation_starts_compound)
    {
        return run_buffered_command_pipeline(graph, pipeline, state, options);
    }

    let commands = pipeline
        .commands
        .iter()
        .map(|invocation| expand_invocation(invocation, state))
        .collect::<Result<Vec<_>, _>>()?;

    if commands.iter().any(|command| command.argv.is_empty()) {
        return Err(ShellError::execution("empty command in pipeline"));
    }

    if let Some((resolved_prefix, final_read)) =
        resolve_streaming_external_prefix_to_final_read(&commands, state)
    {
        let mut outcome = run_streaming_external_prefix_to_final_read(
            &resolved_prefix,
            &final_read,
            state,
            options.output_mode,
            options.allow_process_replacement,
        )?;
        apply_pipeline_negation(&mut outcome, pipeline.negated);
        if options.output_mode.should_capture() {
            let argv = vec![graph.source.clone()];
            outcome.observation = Some(render_observation(
                options.output_mode,
                &graph.id,
                &argv,
                outcome.exit_code,
                &outcome.stdout,
                &outcome.stderr,
            ));
        }
        return Ok(outcome);
    }

    if let Some(resolved) = resolve_streaming_external_pipeline(&commands, state) {
        let mut outcome = run_streaming_external_pipeline(&resolved, state)?;
        apply_pipeline_negation(&mut outcome, pipeline.negated);
        if options.output_mode.should_capture() {
            let argv = vec![graph.source.clone()];
            outcome.observation = Some(render_observation(
                options.output_mode,
                &graph.id,
                &argv,
                outcome.exit_code,
                &outcome.stdout,
                &outcome.stderr,
            ));
        }
        return Ok(outcome);
    }

    preflight_buffered_pipeline_invocations(&commands, state)?;

    let mut stdin_data: Option<Vec<u8>> = None;
    let mut stderr = Vec::new();
    let mut exit_codes = Vec::with_capacity(commands.len());
    let last_index = commands.len().saturating_sub(1);
    let mut outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    for (index, invocation) in commands.iter().enumerate() {
        outcome = if index == last_index {
            run_invocation(
                invocation,
                state,
                options.output_mode,
                stdin_data.as_deref(),
                true,
                LookupMode::Normal,
                options.allow_process_replacement,
            )?
        } else {
            let mut stage_state = state.clone();
            run_invocation(
                invocation,
                &mut stage_state,
                options.output_mode,
                stdin_data.as_deref(),
                true,
                LookupMode::Normal,
                options.allow_process_replacement,
            )?
        };
        exit_codes.push(outcome.exit_code);
        stderr.extend_from_slice(&outcome.stderr);
        if index != last_index {
            stdin_data = Some(std::mem::take(&mut outcome.stdout));
        }
    }

    record_pipestatus(state, &exit_codes);
    outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    outcome.stderr = stderr;
    if options.output_mode.should_capture() {
        let argv = vec![graph.source.clone()];
        outcome.observation = Some(render_observation(
            options.output_mode,
            &graph.id,
            &argv,
            outcome.exit_code,
            &outcome.stdout,
            &outcome.stderr,
        ));
    }
    Ok(outcome)
}

fn run_buffered_command_pipeline(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    let mut stdin_data: Option<Vec<u8>> = None;
    let mut stderr = Vec::new();
    let mut exit_codes = Vec::with_capacity(pipeline.commands.len());
    let last_index = pipeline.commands.len().saturating_sub(1);
    let mut outcome = CommandOutcome::captured(0, Vec::new(), Vec::new());

    for (index, invocation) in pipeline.commands.iter().enumerate() {
        outcome = if index == last_index {
            run_pipeline_command_invocation(
                invocation,
                state,
                options.output_mode,
                stdin_data.as_deref(),
                true,
                !pipeline.negated,
                options.allow_process_replacement,
            )?
        } else {
            let mut stage_state = state.clone();
            run_pipeline_command_invocation(
                invocation,
                &mut stage_state,
                options.output_mode,
                stdin_data.as_deref(),
                true,
                !pipeline.negated,
                options.allow_process_replacement,
            )?
        };
        exit_codes.push(outcome.exit_code);
        stderr.extend_from_slice(&outcome.stderr);
        if index != last_index {
            stdin_data = Some(std::mem::take(&mut outcome.stdout));
        }
    }

    record_pipestatus(state, &exit_codes);
    outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    outcome.stderr = stderr;
    if options.output_mode.should_capture() {
        let argv = vec![graph.source.clone()];
        outcome.observation = Some(render_observation(
            options.output_mode,
            &graph.id,
            &argv,
            outcome.exit_code,
            &outcome.stdout,
            &outcome.stderr,
        ));
    }
    Ok(outcome)
}

fn command_invocation_starts_compound(invocation: &CommandInvocation) -> bool {
    let reserved = invocation
        .argv
        .first()
        .zip(invocation.argv_quote.first())
        .is_some_and(|(word, quote)| {
            *quote == QuoteKind::None
                && matches!(
                    word.as_str(),
                    "if" | "while" | "until" | "for" | "select" | "case" | "{"
                )
        });
    reserved
        || invocation
            .argv_segments
            .first()
            .and_then(|segments| segments.first())
            .is_some_and(|segment| {
                segment.quote == QuoteKind::None && segment.text.starts_with('(')
            })
}

fn run_pipeline_command_invocation(
    invocation: &CommandInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    stdin_data: Option<&[u8]>,
    capture_outputs: bool,
    allow_function_definition: bool,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    if let Some(if_block) = parse_if_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_if_invocation(
                &if_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(while_block) = parse_while_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_while_invocation(
                &while_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(for_block) = parse_for_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_for_invocation(
                &for_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(select_block) = parse_select_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_select_invocation(
                &select_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(case_block) = parse_case_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_case_invocation(
                &case_block,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(inner) = parse_subshell_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_subshell_invocation(
                &inner,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if let Some(inner) = parse_brace_group_invocation(invocation)? {
        return run_compound_with_effective_stdin(state, invocation, stdin_data, |state| {
            run_brace_group_invocation(
                &inner,
                invocation,
                state,
                output_mode,
                capture_outputs,
                allow_process_replacement,
            )
        });
    }

    if allow_function_definition {
        if let Some((name, function)) = parse_function_definition(invocation)? {
            state.set_function(name, function);
            return Ok(CommandOutcome::captured(0, Vec::new(), Vec::new()));
        }
    }

    let invocation = expand_invocation(invocation, state)?;
    if invocation.argv.is_empty() {
        apply_shell_assignments(&invocation.assignments, state);
        return Ok(CommandOutcome::captured(
            state.last_command_substitution_status(),
            Vec::new(),
            Vec::new(),
        ));
    }

    run_invocation(
        &invocation,
        state,
        output_mode,
        stdin_data,
        capture_outputs,
        LookupMode::Normal,
        allow_process_replacement,
    )
}

fn run_with_buffered_stdin<F>(
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let Some(stdin_data) = stdin_data else {
        return run(state);
    };

    let previous = state.replace_buffered_stdin(Some(BufferedStdin::new(stdin_data.to_vec())));
    let result = run(state);
    state.replace_buffered_stdin(previous);
    result
}

fn run_compound_with_effective_stdin<F>(
    state: &mut ShellState,
    invocation: &CommandInvocation,
    stdin_data: Option<&[u8]>,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let redirected_stdin = redirected_stdin_from_command_redirections(invocation, state)?;
    run_with_effective_shell_stdin(state, stdin_data, redirected_stdin, run)
}

fn run_with_effective_shell_stdin<F>(
    state: &mut ShellState,
    stdin_data: Option<&[u8]>,
    redirected_stdin: Option<Vec<u8>>,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    run_with_buffered_stdin(state, redirected_stdin.as_deref().or(stdin_data), run)
}

fn redirected_stdin_from_command_redirections(
    invocation: &CommandInvocation,
    state: &mut ShellState,
) -> Result<Option<Vec<u8>>, ShellError> {
    let redirections = expand_redirections(&invocation.redirections, state)?;
    redirected_stdin_from_expanded_redirections(&redirections)
}

fn redirected_stdin_from_expanded_redirections(
    redirections: &[ExpandedRedirection],
) -> Result<Option<Vec<u8>>, ShellError> {
    let mut stdin = None;
    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(path))
                if redirection.fd == 0 =>
            {
                stdin = Some(std::fs::read(path)?);
            }
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(bytes),
            ) => {
                stdin = Some(bytes.clone());
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {
                stdin = Some(Vec::new());
            }
            _ => {}
        }
    }
    Ok(stdin)
}

fn run_with_streaming_stdin<F>(
    state: &mut ShellState,
    reader: io::PipeReader,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let previous = state.replace_streaming_stdin(Some(StreamingStdin::new(reader)));
    let result = run(state);
    state.replace_streaming_stdin(previous);
    result
}

fn run_with_streaming_stdout<F>(
    state: &mut ShellState,
    writer: io::PipeWriter,
    run: F,
) -> Result<CommandOutcome, ShellError>
where
    F: FnOnce(&mut ShellState) -> Result<CommandOutcome, ShellError>,
{
    let previous = state.replace_streaming_stdout(Some(StreamingStdout::new(writer)));
    let result = run(state);
    state.replace_streaming_stdout(previous);
    result
}

fn apply_pipeline_negation(outcome: &mut CommandOutcome, negated: bool) {
    if negated {
        outcome.exit_code = if outcome.exit_code == 0 { 1 } else { 0 };
    }
}

fn try_run_streaming_external_prefix_to_final_shell_command(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<Option<CommandOutcome>, ShellError> {
    let Some((resolved_prefix, final_invocation)) =
        resolve_streaming_external_prefix_to_final_shell_command(pipeline, state)?
    else {
        return Ok(None);
    };

    let mut outcome = run_streaming_external_prefix_to_final_shell_command(
        &resolved_prefix,
        &final_invocation,
        state,
        options,
        !pipeline.negated,
    )?;
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    if options.output_mode.should_capture() {
        let argv = vec![graph.source.clone()];
        outcome.observation = Some(render_observation(
            options.output_mode,
            &graph.id,
            &argv,
            outcome.exit_code,
            &outcome.stdout,
            &outcome.stderr,
        ));
    }
    Ok(Some(outcome))
}

fn resolve_streaming_external_prefix_to_final_shell_command(
    pipeline: &Pipeline,
    state: &mut ShellState,
) -> Result<Option<(Vec<ResolvedExternalInvocation>, CommandInvocation)>, ShellError> {
    let Some((final_invocation, prefix)) = pipeline.commands.split_last() else {
        return Ok(None);
    };
    if prefix.is_empty() || !command_invocation_accepts_streaming_stdin(final_invocation, state) {
        return Ok(None);
    }

    let mut resolved = Vec::with_capacity(prefix.len());
    for command in prefix {
        let invocation = expand_invocation(command, state)?;
        if invocation.argv.is_empty() {
            return Err(ShellError::execution("empty command in pipeline"));
        }
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return Ok(None);
        }

        let name = invocation
            .argv
            .first()
            .expect("checked non-empty invocation");
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return Ok(None);
        }

        let Some(path) = resolve_external_path(&invocation, state, name) else {
            return Ok(None);
        };
        resolved.push(ResolvedExternalInvocation { invocation, path });
    }

    Ok(Some((resolved, final_invocation.clone())))
}

fn command_invocation_accepts_streaming_stdin(
    invocation: &CommandInvocation,
    state: &ShellState,
) -> bool {
    if command_invocation_starts_compound(invocation) {
        return true;
    }

    let Some((name, quote)) = invocation.argv.first().zip(invocation.argv_quote.first()) else {
        return false;
    };
    if *quote != QuoteKind::None {
        return false;
    }

    if state.function(name).is_some() {
        return true;
    }

    if name == "builtin" {
        return invocation
            .argv
            .get(1)
            .zip(invocation.argv_quote.get(1))
            .is_some_and(|(wrapped, quote)| *quote == QuoteKind::None && is_builtin(wrapped));
    }

    state.alias(name).is_none() && state.abbreviation(name).is_none() && is_builtin(name)
}

fn supports_streaming_shell_stage_redirections(redirections: &[agsh_core::Redirection]) -> bool {
    redirections.iter().all(
        |redirection| match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, RedirectionTarget::Word { .. }) => redirection.fd == 0,
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                RedirectionTarget::Word { .. },
            )
            | (RedirectionMode::WriteBoth, RedirectionTarget::Word { .. }) => {
                redirection.fd == 1 || redirection.fd == 2
            }
            (RedirectionMode::DupFd, RedirectionTarget::Close) => redirection.fd <= 2,
            (RedirectionMode::DupFd, RedirectionTarget::Fd(1)) => redirection.fd == 2,
            (RedirectionMode::DupFd, RedirectionTarget::Fd(2)) => redirection.fd == 1,
            _ => false,
        },
    )
}

fn try_run_streaming_mixed_shell_stage_pipeline(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<Option<CommandOutcome>, ShellError> {
    let Some(stages) = resolve_streaming_mixed_shell_stage_pipeline(pipeline, state)? else {
        return Ok(None);
    };

    let mut outcome = run_streaming_mixed_shell_stage_pipeline(&stages, state, options)?;
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    if options.output_mode.should_capture() {
        let argv = vec![graph.source.clone()];
        outcome.observation = Some(render_observation(
            options.output_mode,
            &graph.id,
            &argv,
            outcome.exit_code,
            &outcome.stdout,
            &outcome.stderr,
        ));
    }
    Ok(Some(outcome))
}

fn resolve_streaming_mixed_shell_stage_pipeline(
    pipeline: &Pipeline,
    state: &mut ShellState,
) -> Result<Option<Vec<ResolvedStreamingStage>>, ShellError> {
    if pipeline.commands.len() < 2 {
        return Ok(None);
    }

    let shell_indices = pipeline
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, invocation)| {
            command_invocation_accepts_streaming_stdin(invocation, state).then_some(index)
        })
        .collect::<Vec<_>>();
    if shell_indices.is_empty()
        || shell_indices
            .last()
            .is_some_and(|index| *index + 1 == pipeline.commands.len())
    {
        return Ok(None);
    }
    let has_first_stage_shell = shell_indices.first() == Some(&0);
    let has_non_contiguous_shell_stages = shell_indices
        .windows(2)
        .any(|indices| indices[1] != indices[0] + 1);
    if !has_first_stage_shell && !has_non_contiguous_shell_stages {
        return Ok(None);
    }

    let mut stages = Vec::with_capacity(pipeline.commands.len());
    for command in &pipeline.commands {
        if command_invocation_accepts_streaming_stdin(command, state) {
            if !supports_streaming_shell_stage_redirections(&command.redirections) {
                return Ok(None);
            }
            stages.push(ResolvedStreamingStage::Shell(command.clone()));
            continue;
        }

        let invocation = expand_invocation(command, state)?;
        if invocation.argv.is_empty() {
            return Err(ShellError::execution("empty command in pipeline"));
        }
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return Ok(None);
        }

        let name = invocation
            .argv
            .first()
            .expect("checked non-empty invocation");
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return Ok(None);
        }

        let Some(path) = resolve_external_path(&invocation, state, name) else {
            return Ok(None);
        };
        stages.push(ResolvedStreamingStage::External(
            ResolvedExternalInvocation { invocation, path },
        ));
    }

    Ok(Some(stages))
}

fn try_run_streaming_external_shell_stage_pipeline(
    graph: &CommandGraph,
    pipeline: &Pipeline,
    state: &mut ShellState,
    options: &ExecutionOptions,
) -> Result<Option<CommandOutcome>, ShellError> {
    let Some(resolved) = resolve_streaming_external_shell_stage_pipeline(pipeline, state)? else {
        return Ok(None);
    };

    let mut outcome = run_streaming_external_shell_stage_pipeline(
        &resolved.prefix,
        &resolved.shell_stages,
        &resolved.suffix,
        state,
        options,
    )?;
    apply_pipeline_negation(&mut outcome, pipeline.negated);
    if options.output_mode.should_capture() {
        let argv = vec![graph.source.clone()];
        outcome.observation = Some(render_observation(
            options.output_mode,
            &graph.id,
            &argv,
            outcome.exit_code,
            &outcome.stdout,
            &outcome.stderr,
        ));
    }
    Ok(Some(outcome))
}

fn resolve_streaming_external_shell_stage_pipeline(
    pipeline: &Pipeline,
    state: &mut ShellState,
) -> Result<Option<ResolvedShellStagePipeline>, ShellError> {
    if pipeline.commands.len() < 3 {
        return Ok(None);
    }

    let shell_indices = pipeline
        .commands
        .iter()
        .enumerate()
        .filter_map(|(index, invocation)| {
            command_invocation_accepts_streaming_stdin(invocation, state).then_some(index)
        })
        .collect::<Vec<_>>();
    let (Some(first_shell_index), Some(last_shell_index)) = (
        shell_indices.first().copied(),
        shell_indices.last().copied(),
    ) else {
        return Ok(None);
    };
    if first_shell_index == 0 || last_shell_index + 1 == pipeline.commands.len() {
        return Ok(None);
    }
    if shell_indices
        .windows(2)
        .any(|indices| indices[1] != indices[0] + 1)
    {
        return Ok(None);
    }

    let shell_stages = &pipeline.commands[first_shell_index..=last_shell_index];
    if shell_stages
        .iter()
        .any(|stage| !supports_streaming_shell_stage_redirections(&stage.redirections))
    {
        return Ok(None);
    }

    let prefix = resolve_external_pipeline_slice(&pipeline.commands[..first_shell_index], state)?;
    let suffix =
        resolve_external_pipeline_slice(&pipeline.commands[last_shell_index + 1..], state)?;
    let (Some(prefix), Some(suffix)) = (prefix, suffix) else {
        return Ok(None);
    };

    Ok(Some(ResolvedShellStagePipeline {
        prefix,
        shell_stages: shell_stages.to_vec(),
        suffix,
    }))
}

fn resolve_external_pipeline_slice(
    commands: &[CommandInvocation],
    state: &mut ShellState,
) -> Result<Option<Vec<ResolvedExternalInvocation>>, ShellError> {
    let mut resolved = Vec::with_capacity(commands.len());
    for command in commands {
        let invocation = expand_invocation(command, state)?;
        if invocation.argv.is_empty() {
            return Err(ShellError::execution("empty command in pipeline"));
        }
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return Ok(None);
        }

        let name = invocation
            .argv
            .first()
            .expect("checked non-empty invocation");
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return Ok(None);
        }

        let Some(path) = resolve_external_path(&invocation, state, name) else {
            return Ok(None);
        };
        resolved.push(ResolvedExternalInvocation { invocation, path });
    }
    Ok(Some(resolved))
}

fn resolve_streaming_external_prefix_to_final_read(
    commands: &[ExpandedInvocation],
    state: &mut ShellState,
) -> Option<(Vec<ResolvedExternalInvocation>, ExpandedInvocation)> {
    let (final_read, prefix) = commands.split_last()?;
    if prefix.is_empty()
        || final_read.argv.first().map(String::as_str) != Some("read")
        || !final_read.redirections.is_empty()
        || state.function("read").is_some()
        || state.alias("read").is_some()
        || state.abbreviation("read").is_some()
    {
        return None;
    }

    let mut resolved = Vec::with_capacity(prefix.len());
    for invocation in prefix {
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return None;
        }

        let name = invocation.argv.first()?;
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return None;
        }

        let path = resolve_external_path(invocation, state, name)?;
        resolved.push(ResolvedExternalInvocation {
            invocation: invocation.clone(),
            path,
        });
    }

    Some((resolved, final_read.clone()))
}

fn resolve_streaming_external_pipeline(
    commands: &[ExpandedInvocation],
    state: &mut ShellState,
) -> Option<Vec<ResolvedExternalInvocation>> {
    let mut resolved = Vec::with_capacity(commands.len());

    for invocation in commands {
        if !supports_streaming_external_redirections(&invocation.redirections) {
            return None;
        }

        let name = invocation.argv.first()?;
        if state.function(name).is_some()
            || state.alias(name).is_some()
            || state.abbreviation(name).is_some()
            || is_builtin(name)
        {
            return None;
        }

        let path = resolve_external_path(invocation, state, name)?;
        resolved.push(ResolvedExternalInvocation {
            invocation: invocation.clone(),
            path,
        });
    }

    Some(resolved)
}

fn supports_streaming_external_redirections(redirections: &[ExpandedRedirection]) -> bool {
    redirections.iter().all(
        |redirection| match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(_)) => redirection.fd == 0,
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                ExpandedRedirectionTarget::Path(_),
            ) => redirection.fd == 1 || redirection.fd == 2,
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(_)) => true,
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(target_fd)) => {
                (redirection.fd == 1 || redirection.fd == 2) && (*target_fd == 1 || *target_fd == 2)
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) => redirection.fd <= 2,
            _ => false,
        },
    )
}

fn spawn_resolved_external_stage(
    resolved: &ResolvedExternalInvocation,
    state: &ShellState,
    stdin: ExternalStageStdin,
) -> Result<(Child, StreamingOutputReaders), ShellError> {
    let mut command = Command::new(&resolved.path);
    command.args(&resolved.invocation.argv[1..]);
    command.current_dir(state.cwd());
    command.env_clear();
    command.envs(state.exported_env());
    for assignment in &resolved.invocation.assignments {
        command.env(&assignment.name, &assignment.value);
    }
    command.stdin(stdin.into_stdio());

    let output_readers = apply_streaming_external_redirections(
        &mut command,
        &resolved.invocation.redirections,
        state.noclobber(),
    )?;
    let child = command.spawn()?;
    Ok((child, output_readers))
}

fn run_streaming_external_pipeline(
    commands: &[ResolvedExternalInvocation],
    state: &ShellState,
) -> Result<CommandOutcome, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdout_handle = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) = match spawn_resolved_external_stage(resolved, state, stdin) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };

        if let Some(stderr) = output_readers.stderr {
            stderr_handles.push(read_pipe_to_end(stderr));
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdout_handle = Some(read_pipe_to_end(stdout));
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }

        children.push(child);
    }

    drop(previous_stdout);

    let mut last_exit_code = 0;
    let mut exit_codes = Vec::with_capacity(children.len());
    for (index, child) in children.iter_mut().enumerate() {
        let status = child.wait()?;
        let exit_code = exit_status_code(status);
        exit_codes.push(exit_code);
        if index == last_index {
            last_exit_code = exit_code;
        }
    }
    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());

    let stdout = match final_stdout_handle {
        Some(handle) => join_pipe_reader(handle)?,
        None => Vec::new(),
    };
    let mut stderr = Vec::new();
    for handle in stderr_handles {
        stderr.extend(join_pipe_reader(handle)?);
    }

    Ok(CommandOutcome::captured(
        if state.pipefail() {
            exit_code
        } else {
            last_exit_code
        },
        stdout,
        stderr,
    ))
}

fn run_streaming_mixed_shell_stage_pipeline(
    stages: &[ResolvedStreamingStage],
    state: &ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    let last_index = stages.len().saturating_sub(1);
    let mut running_stages = Vec::with_capacity(stages.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(stages.len());
    let mut final_stdout_handle = None;

    for (index, stage) in stages.iter().enumerate() {
        match stage {
            ResolvedStreamingStage::External(resolved) => {
                let stdin = if let Some(stdout) = previous_stdout.take() {
                    ExternalStageStdin::Pipe(stdout)
                } else if previous_pipe_closed {
                    previous_pipe_closed = false;
                    ExternalStageStdin::Null
                } else {
                    ExternalStageStdin::Inherit
                };

                let (child, output_readers) =
                    match spawn_resolved_external_stage(resolved, state, stdin) {
                        Ok(child) => child,
                        Err(error) => {
                            terminate_running_streaming_stages(&mut running_stages);
                            return Err(error);
                        }
                    };

                if let Some(stderr) = output_readers.stderr {
                    stderr_handles.push(read_pipe_to_end(stderr));
                }

                if let Some(stdout) = output_readers.stdout {
                    if index == last_index {
                        final_stdout_handle = Some(read_pipe_to_end(stdout));
                    } else {
                        previous_stdout = Some(stdout);
                        previous_pipe_closed = false;
                    }
                } else if index != last_index {
                    previous_pipe_closed = true;
                }

                running_stages.push(RunningStreamingStage::External(child));
            }
            ResolvedStreamingStage::Shell(shell_stage) => {
                let stage_stdin = previous_stdout.take();
                if previous_pipe_closed {
                    previous_pipe_closed = false;
                }
                let (stdout_reader, stdout_writer) = io::pipe()?;
                if index == last_index {
                    final_stdout_handle = Some(read_pipe_to_end(stdout_reader));
                } else {
                    previous_stdout = Some(stdout_reader);
                    previous_pipe_closed = false;
                }

                running_stages.push(RunningStreamingStage::Shell(spawn_shell_pipeline_stage(
                    shell_stage.clone(),
                    state.clone(),
                    stage_stdin,
                    stdout_writer,
                    options.output_mode,
                    options.allow_process_replacement,
                )));
            }
        }
    }

    drop(previous_stdout);

    let mut exit_codes = Vec::with_capacity(running_stages.len());
    let mut stderr = Vec::new();
    let mut first_shell_error = None;
    for stage in running_stages {
        match stage {
            RunningStreamingStage::External(mut child) => {
                let status = child.wait()?;
                exit_codes.push(exit_status_code(status));
            }
            RunningStreamingStage::Shell(thread) => match thread
                .join()
                .map_err(|_| ShellError::execution("pipeline shell stage thread panicked"))
            {
                Ok(Ok(mut outcome)) => {
                    exit_codes.push(outcome.exit_code);
                    stderr.append(&mut outcome.stderr);
                }
                Ok(Err(error)) => {
                    exit_codes.push(1);
                    if first_shell_error.is_none() {
                        first_shell_error = Some(error);
                    }
                }
                Err(error) => {
                    exit_codes.push(1);
                    if first_shell_error.is_none() {
                        first_shell_error = Some(error);
                    }
                }
            },
        }
    }

    if let Some(error) = first_shell_error {
        return Err(error);
    }

    let stdout = match final_stdout_handle {
        Some(handle) => join_pipe_reader(handle)?,
        None => Vec::new(),
    };
    for handle in stderr_handles {
        stderr.extend(join_pipe_reader(handle)?);
    }

    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    Ok(CommandOutcome::captured(exit_code, stdout, stderr))
}

fn terminate_running_streaming_stages(stages: &mut [RunningStreamingStage]) {
    for stage in stages {
        if let RunningStreamingStage::External(child) = stage {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct RunningExternalPrefix {
    children: Vec<Child>,
    stderr_handles: Vec<std::thread::JoinHandle<io::Result<Vec<u8>>>>,
    final_stdout: Option<io::PipeReader>,
}

struct RunningExternalSuffix {
    children: Vec<Child>,
    stderr_handles: Vec<std::thread::JoinHandle<io::Result<Vec<u8>>>>,
    final_stdout_handle: Option<std::thread::JoinHandle<io::Result<Vec<u8>>>>,
}

struct ResolvedShellStagePipeline {
    prefix: Vec<ResolvedExternalInvocation>,
    shell_stages: Vec<CommandInvocation>,
    suffix: Vec<ResolvedExternalInvocation>,
}

fn run_streaming_external_shell_stage_pipeline(
    prefix: &[ResolvedExternalInvocation],
    shell_stages: &[CommandInvocation],
    suffix: &[ResolvedExternalInvocation],
    state: &ShellState,
    options: &ExecutionOptions,
) -> Result<CommandOutcome, ShellError> {
    let mut prefix = spawn_external_prefix_for_shell_stage(prefix, state)?;
    let mut shell_stdin = prefix.final_stdout.take();
    let mut stage_specs = Vec::with_capacity(shell_stages.len());
    let mut suffix_stdin = None;

    for (index, shell_stage) in shell_stages.iter().enumerate() {
        let stage_stdin = shell_stdin.take();
        let (shell_stdout_reader, shell_stdout_writer) = io::pipe()?;
        if index + 1 == shell_stages.len() {
            suffix_stdin = Some(shell_stdout_reader);
        } else {
            shell_stdin = Some(shell_stdout_reader);
        }
        stage_specs.push((shell_stage.clone(), stage_stdin, shell_stdout_writer));
    }

    let Some(suffix_stdin) = suffix_stdin else {
        return Err(ShellError::execution("missing shell pipeline output"));
    };
    let mut suffix = spawn_external_suffix_from_shell_stage(suffix, state, suffix_stdin)?;
    let shell_threads = stage_specs
        .into_iter()
        .map(|(shell_stage, stage_stdin, shell_stdout_writer)| {
            spawn_shell_pipeline_stage(
                shell_stage,
                state.clone(),
                stage_stdin,
                shell_stdout_writer,
                options.output_mode,
                options.allow_process_replacement,
            )
        })
        .collect::<Vec<_>>();

    let mut shell_outcomes = Vec::with_capacity(shell_threads.len());
    for thread in shell_threads {
        shell_outcomes.push(
            thread
                .join()
                .map_err(|_| ShellError::execution("pipeline shell stage thread panicked"))??,
        );
    }

    let mut exit_codes =
        Vec::with_capacity(prefix.children.len() + shell_outcomes.len() + suffix.children.len());
    for child in &mut prefix.children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }
    for shell_outcome in &shell_outcomes {
        exit_codes.push(shell_outcome.exit_code);
    }
    for child in &mut suffix.children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }

    let stdout = match suffix.final_stdout_handle {
        Some(handle) => join_pipe_reader(handle)?,
        None => Vec::new(),
    };
    let mut stderr = Vec::new();
    for mut shell_outcome in shell_outcomes {
        stderr.append(&mut shell_outcome.stderr);
    }
    for handle in prefix.stderr_handles {
        stderr.extend(join_pipe_reader(handle)?);
    }
    for handle in suffix.stderr_handles {
        stderr.extend(join_pipe_reader(handle)?);
    }

    let exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    Ok(CommandOutcome::captured(exit_code, stdout, stderr))
}

fn spawn_shell_pipeline_stage(
    shell_stage: CommandInvocation,
    mut stage_state: ShellState,
    shell_stdin: Option<io::PipeReader>,
    mut shell_stdout_writer: io::PipeWriter,
    output_mode: OutputMode,
    allow_process_replacement: bool,
) -> std::thread::JoinHandle<Result<CommandOutcome, ShellError>> {
    std::thread::spawn(move || {
        let run_stage = |state: &mut ShellState| {
            if shell_stage.redirections.is_empty() {
                run_with_streaming_stdout(state, shell_stdout_writer, |state| {
                    let mut outcome = run_pipeline_command_invocation(
                        &shell_stage,
                        state,
                        output_mode,
                        None,
                        true,
                        true,
                        allow_process_replacement,
                    )?;
                    emit_streaming_stdout(state, &mut outcome)?;
                    Ok(outcome)
                })
            } else {
                let mut outcome = run_pipeline_command_invocation(
                    &shell_stage,
                    state,
                    output_mode,
                    None,
                    true,
                    true,
                    allow_process_replacement,
                )?;
                shell_stdout_writer.write_all(&outcome.stdout)?;
                outcome.stdout.clear();
                Ok(outcome)
            }
        };

        if let Some(stdin) = shell_stdin {
            run_with_streaming_stdin(&mut stage_state, stdin, run_stage)
        } else {
            run_with_buffered_stdin(&mut stage_state, Some(&[]), run_stage)
        }
    })
}

fn spawn_external_prefix_for_shell_stage(
    commands: &[ResolvedExternalInvocation],
    state: &ShellState,
) -> Result<RunningExternalPrefix, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdout = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) = match spawn_resolved_external_stage(resolved, state, stdin) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };

        if let Some(stderr) = output_readers.stderr {
            stderr_handles.push(read_pipe_to_end(stderr));
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdout = Some(stdout);
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }

        children.push(child);
    }

    drop(previous_stdout);
    Ok(RunningExternalPrefix {
        children,
        stderr_handles,
        final_stdout,
    })
}

fn spawn_external_suffix_from_shell_stage(
    commands: &[ResolvedExternalInvocation],
    state: &ShellState,
    initial_stdin: io::PipeReader,
) -> Result<RunningExternalSuffix, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = Some(initial_stdin);
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdout_handle = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) = match spawn_resolved_external_stage(resolved, state, stdin) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };

        if let Some(stderr) = output_readers.stderr {
            stderr_handles.push(read_pipe_to_end(stderr));
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdout_handle = Some(read_pipe_to_end(stdout));
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }

        children.push(child);
    }

    drop(previous_stdout);
    Ok(RunningExternalSuffix {
        children,
        stderr_handles,
        final_stdout_handle,
    })
}

fn run_streaming_external_prefix_to_final_read(
    commands: &[ResolvedExternalInvocation],
    final_read: &ExpandedInvocation,
    state: &mut ShellState,
    output_mode: OutputMode,
    allow_process_replacement: bool,
) -> Result<CommandOutcome, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdin = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) = match spawn_resolved_external_stage(resolved, state, stdin) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };

        if let Some(stderr) = output_readers.stderr {
            stderr_handles.push(read_pipe_to_end(stderr));
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdin = Some(stdout);
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }

        children.push(child);
    }

    drop(previous_stdout);

    let raw = read_invocation_raw_mode(final_read);
    let stdin_bytes = match final_stdin {
        Some(reader) => read_read_input_from_pipe(reader, raw)?,
        None => Vec::new(),
    };

    let mut exit_codes = Vec::with_capacity(children.len() + 1);
    for child in &mut children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }

    let mut read_outcome = run_invocation(
        final_read,
        state,
        output_mode,
        Some(&stdin_bytes),
        true,
        LookupMode::Normal,
        allow_process_replacement,
    )?;
    exit_codes.push(read_outcome.exit_code);
    read_outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());

    for handle in stderr_handles {
        read_outcome.stderr.extend(join_pipe_reader(handle)?);
    }

    Ok(read_outcome)
}

fn run_streaming_external_prefix_to_final_shell_command(
    commands: &[ResolvedExternalInvocation],
    final_invocation: &CommandInvocation,
    state: &mut ShellState,
    options: &ExecutionOptions,
    allow_function_definition: bool,
) -> Result<CommandOutcome, ShellError> {
    let last_index = commands.len().saturating_sub(1);
    let mut children: Vec<Child> = Vec::with_capacity(commands.len());
    let mut previous_stdout: Option<io::PipeReader> = None;
    let mut previous_pipe_closed = false;
    let mut stderr_handles = Vec::with_capacity(commands.len());
    let mut final_stdin = None;

    for (index, resolved) in commands.iter().enumerate() {
        let stdin = if let Some(stdout) = previous_stdout.take() {
            ExternalStageStdin::Pipe(stdout)
        } else if previous_pipe_closed {
            previous_pipe_closed = false;
            ExternalStageStdin::Null
        } else {
            ExternalStageStdin::Inherit
        };

        let (child, output_readers) = match spawn_resolved_external_stage(resolved, state, stdin) {
            Ok(child) => child,
            Err(error) => {
                terminate_children(&mut children);
                return Err(error);
            }
        };

        if let Some(stderr) = output_readers.stderr {
            stderr_handles.push(read_pipe_to_end(stderr));
        }

        if let Some(stdout) = output_readers.stdout {
            if index == last_index {
                final_stdin = Some(stdout);
            } else {
                previous_stdout = Some(stdout);
                previous_pipe_closed = false;
            }
        } else if index != last_index {
            previous_pipe_closed = true;
        }

        children.push(child);
    }

    drop(previous_stdout);

    let final_result = if let Some(reader) = final_stdin {
        run_with_streaming_stdin(state, reader, |state| {
            run_pipeline_command_invocation(
                final_invocation,
                state,
                options.output_mode,
                None,
                true,
                allow_function_definition,
                options.allow_process_replacement,
            )
        })
    } else {
        run_with_buffered_stdin(state, Some(&[]), |state| {
            run_pipeline_command_invocation(
                final_invocation,
                state,
                options.output_mode,
                None,
                true,
                allow_function_definition,
                options.allow_process_replacement,
            )
        })
    };

    let mut exit_codes = Vec::with_capacity(children.len() + 1);
    for child in &mut children {
        let status = child.wait()?;
        exit_codes.push(exit_status_code(status));
    }

    let mut prefix_stderr = Vec::new();
    for handle in stderr_handles {
        prefix_stderr.extend(join_pipe_reader(handle)?);
    }

    let mut final_outcome = final_result?;
    exit_codes.push(final_outcome.exit_code);
    final_outcome.exit_code = pipeline_exit_code(&exit_codes, state.pipefail());
    final_outcome.stderr.extend(prefix_stderr);

    Ok(final_outcome)
}

fn read_invocation_raw_mode(invocation: &ExpandedInvocation) -> bool {
    invocation.argv.iter().skip(1).any(|arg| arg == "-r")
}

fn read_read_input_from_pipe<R>(mut reader: R, raw: bool) -> Result<Vec<u8>, ShellError>
where
    R: Read,
{
    let mut input = Vec::new();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 {
            break;
        }

        input.push(byte[0]);
        line.push(byte[0]);
        if byte[0] == b'\n' {
            if raw || !read_line_is_continued(&line) {
                break;
            }
            line.clear();
        }
    }

    Ok(input)
}

fn read_line_is_continued(line: &[u8]) -> bool {
    let mut text = String::from_utf8_lossy(line).to_string();
    remove_read_continuation(&mut text)
}

fn pipeline_exit_code(exit_codes: &[i32], pipefail: bool) -> i32 {
    if pipefail {
        exit_codes
            .iter()
            .rev()
            .copied()
            .find(|code| *code != 0)
            .unwrap_or(0)
    } else {
        exit_codes.last().copied().unwrap_or(0)
    }
}

fn apply_streaming_external_redirections(
    command: &mut Command,
    redirections: &[ExpandedRedirection],
    noclobber: bool,
) -> Result<StreamingOutputReaders, ShellError> {
    let (stdout_reader, stdout_writer) = io::pipe()?;
    let (stderr_reader, stderr_writer) = io::pipe()?;
    let mut stdout_target = StreamingOutputTarget::Pipe {
        kind: StreamingPipeKind::Stdout,
        writer: stdout_writer,
    };
    let mut stderr_target = StreamingOutputTarget::Pipe {
        kind: StreamingPipeKind::Stderr,
        writer: stderr_writer,
    };

    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(path))
                if redirection.fd == 0 =>
            {
                command.stdin(Stdio::from(File::open(path)?));
            }
            (RedirectionMode::Write, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, false)?;
                if redirection.fd == 1 {
                    stdout_target = StreamingOutputTarget::File(file);
                } else if redirection.fd == 2 {
                    stderr_target = StreamingOutputTarget::File(file);
                }
            }
            (RedirectionMode::WriteClobber, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, true)?;
                if redirection.fd == 1 {
                    stdout_target = StreamingOutputTarget::File(file);
                } else if redirection.fd == 2 {
                    stderr_target = StreamingOutputTarget::File(file);
                }
            }
            (RedirectionMode::Append, ExpandedRedirectionTarget::Path(path)) => {
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                if redirection.fd == 1 {
                    stdout_target = StreamingOutputTarget::File(file);
                } else if redirection.fd == 2 {
                    stderr_target = StreamingOutputTarget::File(file);
                }
            }
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, false)?;
                stdout_target = StreamingOutputTarget::File(file.try_clone()?);
                stderr_target = StreamingOutputTarget::File(file);
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {
                command.stdin(Stdio::null());
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 1 => {
                stdout_target = StreamingOutputTarget::Null;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 2 => {
                stderr_target = StreamingOutputTarget::Null;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1)) if redirection.fd == 2 => {
                stderr_target = stdout_target.try_clone()?;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2)) if redirection.fd == 1 => {
                stdout_target = stderr_target.try_clone()?;
            }
            _ => {
                return Err(ShellError::unsupported(format!(
                    "unsupported streaming pipeline redirection for fd {}",
                    redirection.fd
                )));
            }
        }
    }

    let stdout_pipe_used = stdout_target.pipe_kind() == Some(StreamingPipeKind::Stdout)
        || stderr_target.pipe_kind() == Some(StreamingPipeKind::Stdout);
    let stderr_pipe_used = stdout_target.pipe_kind() == Some(StreamingPipeKind::Stderr)
        || stderr_target.pipe_kind() == Some(StreamingPipeKind::Stderr);

    command.stdout(stdout_target.into_stdio());
    command.stderr(stderr_target.into_stdio());

    Ok(StreamingOutputReaders {
        stdout: if stdout_pipe_used {
            Some(stdout_reader)
        } else {
            None
        },
        stderr: if stderr_pipe_used {
            Some(stderr_reader)
        } else {
            None
        },
    })
}

fn read_pipe_to_end<R>(mut reader: R) -> std::thread::JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn join_pipe_reader(
    handle: std::thread::JoinHandle<io::Result<Vec<u8>>>,
) -> Result<Vec<u8>, ShellError> {
    handle
        .join()
        .map_err(|_| ShellError::execution("pipeline reader thread panicked"))?
        .map_err(ShellError::from)
}

fn terminate_children(children: &mut [Child]) {
    for child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn expand_invocation(
    invocation: &CommandInvocation,
    state: &mut ShellState,
) -> Result<ExpandedInvocation, ShellError> {
    // Reset so an assignment-only command can report the status of a command
    // substitution in its value (e.g. `x=$(cmd)`); stays 0 when none runs.
    state.set_command_substitution_status(0);
    // `[[ ... ]]` and `(( ... ))`: operands get parameter/tilde expansion but NO
    // globbing and NO word-splitting (each stays one word/expression).
    let no_split = invocation.argv.first().map(String::as_str) == Some("[[")
        || invocation
            .argv
            .first()
            .is_some_and(|a| a.starts_with("((") && a.ends_with("))"));
    let mut argv = Vec::new();
    for (index, arg) in invocation.argv.iter().enumerate() {
        // Input process substitution `<(cmd)` -> a temp-file path.
        if arg.starts_with("<(") && arg.ends_with(')') {
            argv.push(process_substitution_path(&arg[2..arg.len() - 1], state)?);
            continue;
        }
        let segments = invocation
            .argv_segments
            .get(index)
            .cloned()
            .unwrap_or_else(|| vec![WordSegment::new(arg.clone(), invocation.argv_quote[index])]);
        if no_split {
            argv.push(expand_word(&segments, state)?);
        } else {
            argv.extend(expand_word_segments_to_argv_fields(&segments, state)?);
        }
    }

    Ok(ExpandedInvocation {
        assignments: invocation
            .assignments
            .iter()
            .map(|assignment| {
                Ok(Assignment {
                    name: assignment.name.clone(),
                    value: expand_word(&assignment.value_segments, state)?,
                    value_segments: assignment.value_segments.clone(),
                })
            })
            .collect::<Result<Vec<_>, ShellError>>()?,
        argv,
        redirections: expand_redirections(&invocation.redirections, state)?,
    })
}

fn expand_word_segments_to_argv_fields(
    segments: &[WordSegment],
    state: &mut ShellState,
) -> Result<Vec<String>, ShellError> {
    let expanded_fields = expand_argument_fields(segments, state)?;
    let brace_expanded = if has_unquoted_brace(segments) {
        expanded_fields
            .into_iter()
            .flat_map(|field| {
                let glob_eligible = field.has_active_glob();
                expand_braces(&field.text).into_iter().map(move |text| {
                    let glob_mask = if glob_eligible {
                        text.chars().map(is_glob_metachar).collect()
                    } else {
                        vec![false; text.chars().count()]
                    };
                    ExpandedField { text, glob_mask }
                })
            })
            .collect::<Vec<_>>()
    } else {
        expanded_fields
    };

    let mut fields = Vec::new();
    for field in brace_expanded {
        if !state.noglob() && field.has_active_glob() {
            let opts = GlobOpts {
                globstar: state.shopt("globstar"),
                dotglob: state.shopt("dotglob"),
                nocaseglob: state.shopt("nocaseglob"),
            };
            let matches = expand_glob(&field.text, &field.glob_mask, state.cwd(), opts);
            if matches.is_empty() {
                // nullglob: a non-matching pattern expands to nothing (else the
                // literal pattern is kept, as in bash default).
                if !state.shopt("nullglob") {
                    fields.push(field.text);
                }
            } else {
                fields.extend(matches);
            }
        } else {
            fields.push(field.text);
        }
    }
    Ok(fields)
}

fn expand_redirections(
    redirections: &[agsh_core::Redirection],
    state: &mut ShellState,
) -> Result<Vec<ExpandedRedirection>, ShellError> {
    redirections
        .iter()
        .map(|redirection| {
            let target = match redirection.mode {
                RedirectionMode::HereDoc => match &redirection.target {
                    RedirectionTarget::Word { segments, .. } => {
                        // Double segment => expand the body; Single => literal.
                        let body = expand_word(segments, state)?;
                        ExpandedRedirectionTarget::Bytes(body.into_bytes())
                    }
                    _ => ExpandedRedirectionTarget::Bytes(Vec::new()),
                },
                RedirectionMode::HereString => match &redirection.target {
                    RedirectionTarget::Word { segments, .. } => {
                        let mut body = expand_word(segments, state)?;
                        body.push('\n');
                        ExpandedRedirectionTarget::Bytes(body.into_bytes())
                    }
                    _ => ExpandedRedirectionTarget::Bytes(b"\n".to_vec()),
                },
                _ => match &redirection.target {
                    // `< <(cmd)` / `> >(cmd)`: a process substitution as the
                    // redirection target resolves to its temp-file path.
                    RedirectionTarget::Word { text, .. }
                        if text.starts_with("<(") && text.ends_with(')') =>
                    {
                        let path = process_substitution_path(&text[2..text.len() - 1], state)?;
                        ExpandedRedirectionTarget::Path(resolve_shell_path(&path, state.cwd()))
                    }
                    RedirectionTarget::Word { segments, .. } => {
                        let path = expand_word(segments, state)?;
                        ExpandedRedirectionTarget::Path(resolve_shell_path(&path, state.cwd()))
                    }
                    RedirectionTarget::Fd(fd) => ExpandedRedirectionTarget::Fd(*fd),
                    RedirectionTarget::Close => ExpandedRedirectionTarget::Close,
                },
            };
            Ok(ExpandedRedirection {
                fd: redirection.fd,
                mode: redirection.mode,
                target,
            })
        })
        .collect()
}

fn resolve_shell_path(path: &str, cwd: &Path) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.display().to_string()
    } else {
        cwd.join(path).display().to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpansionFragment {
    text: String,
    split_eligible: bool,
    preserves_field: bool,
    glob_eligible: bool,
}

impl ExpansionFragment {
    fn literal(text: impl Into<String>, preserves_field: bool, glob_eligible: bool) -> Self {
        Self {
            text: text.into(),
            split_eligible: false,
            preserves_field,
            glob_eligible,
        }
    }

    fn expanded(text: impl Into<String>, split_eligible: bool) -> Self {
        Self {
            text: text.into(),
            split_eligible,
            preserves_field: false,
            glob_eligible: split_eligible,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpandedField {
    text: String,
    glob_mask: Vec<bool>,
}

impl ExpandedField {
    fn has_active_glob(&self) -> bool {
        self.text
            .chars()
            .zip(&self.glob_mask)
            .any(|(ch, active)| *active && is_glob_metachar(ch))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExpandedFieldBuilder {
    text: String,
    glob_mask: Vec<bool>,
    material: bool,
}

impl ExpandedFieldBuilder {
    fn append_field(&mut self, field: ExpandedField) {
        self.material = true;
        self.text.push_str(&field.text);
        self.glob_mask.extend(field.glob_mask);
    }

    fn append_quoted_text(&mut self, text: String) {
        self.material = true;
        self.glob_mask.extend(text.chars().map(|_| false));
        self.text.push_str(&text);
    }

    fn into_field(self) -> Option<ExpandedField> {
        self.material.then_some(ExpandedField {
            text: self.text,
            glob_mask: self.glob_mask,
        })
    }
}

fn expand_substitutions(input: &str, state: &mut ShellState) -> Result<String, ShellError> {
    let fragments = expand_substitution_fragments(input, state, false, PositionalStarJoin::Space)?;
    Ok(fragments_to_string(&fragments))
}

fn expand_argument_fields(
    segments: &[WordSegment],
    state: &mut ShellState,
) -> Result<Vec<ExpandedField>, ShellError> {
    // `${a[@]}`/`${a[*]}` (quoted or not) -> one field per element (preserved),
    // so `for x in "${a[@]}"` keeps elements with spaces intact.
    if let Some(fields) = expand_array_at_fields(segments, state) {
        return Ok(fields);
    }

    if let Some(fields) = expand_native_list_fields(segments, state) {
        return Ok(fields);
    }

    if let Some(fields) = expand_quoted_positional_list_fields(segments, state)? {
        return Ok(fields);
    }

    let fragments = expand_word_fragments(segments, state, true)?;
    Ok(split_expanded_fields(&fragments, state))
}

/// Expand `[prefix]${name[@]}[suffix]` / `${name[*]}` (name an indexed array)
/// into preserved fields: one per element for `[@]` (prefix glued to the first,
/// suffix to the last), or a single IFS-joined field for `[*]`. Quoted and
/// unquoted are treated the same (per-element); unquoted IFS-splitting of
/// elements that themselves contain IFS chars is not applied (rare).
fn expand_array_at_fields(
    segments: &[WordSegment],
    state: &ShellState,
) -> Option<Vec<ExpandedField>> {
    if segments.len() != 1 {
        return None;
    }
    let text = &segments[0].text;
    let start = text.find("${")?;
    let after = &text[start + 2..];
    let close = after.find('}')?;
    let inner = &after[..close];
    let open = inner.find('[')?;
    if !inner.ends_with(']') {
        return None;
    }
    let name = &inner[..open];
    let sub = &inner[open + 1..inner.len() - 1];
    if (sub != "@" && sub != "*") || !is_identifier(name) {
        return None;
    }
    let prefix = &text[..start];
    let suffix = &text[start + 2 + close + 1..];
    if prefix.contains('$') || suffix.contains('$') {
        return None;
    }
    let array = if state.is_assoc(name) {
        state.assoc_values(name)?
    } else {
        state.array(name)?.to_vec()
    };
    let field = |t: String| ExpandedField {
        glob_mask: vec![false; t.chars().count()],
        text: t,
    };

    if sub == "*" {
        let sep = state
            .lookup("IFS")
            .and_then(|s| s.chars().next())
            .map(String::from)
            .unwrap_or_default();
        return Some(vec![field(format!("{prefix}{}{suffix}", array.join(&sep)))]);
    }
    // `[@]`
    if array.is_empty() {
        return Some(if prefix.is_empty() && suffix.is_empty() {
            Vec::new()
        } else {
            vec![field(format!("{prefix}{suffix}"))]
        });
    }
    let last = array.len() - 1;
    Some(
        array
            .iter()
            .enumerate()
            .map(|(i, elem)| {
                let p = if i == 0 { prefix } else { "" };
                let s = if i == last { suffix } else { "" };
                field(format!("{p}{elem}{s}"))
            })
            .collect(),
    )
}

fn expand_native_list_fields(
    segments: &[WordSegment],
    state: &ShellState,
) -> Option<Vec<ExpandedField>> {
    let word = native_list_word(segments)?;
    let Value::List(values) = state.lookup_value(word.name)? else {
        return None;
    };

    Some(
        values
            .iter()
            .map(|value| {
                let text = format!("{}{}{}", word.prefix, value.as_string_lossy(), word.suffix);
                ExpandedField {
                    glob_mask: vec![false; text.chars().count()],
                    text,
                }
            })
            .collect(),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeListWord<'a> {
    prefix: &'a str,
    name: &'a str,
    suffix: &'a str,
}

fn native_list_word(segments: &[WordSegment]) -> Option<NativeListWord<'_>> {
    if segments.len() != 1 {
        return None;
    }
    let segment = segments.first()?;
    if segment.quote != QuoteKind::None {
        return None;
    }

    if let Some(braced_start) = segment.text.find("${") {
        let name_start = braced_start + 2;
        let rest = &segment.text[name_start..];
        let braced_end = rest.find('}').map(|offset| name_start + offset)?;
        let name = &segment.text[name_start..braced_end];
        let prefix = &segment.text[..braced_start];
        let suffix = &segment.text[braced_end + 1..];
        if !prefix.contains('$') && !suffix.contains('$') && is_identifier(name) {
            return Some(NativeListWord {
                prefix,
                name,
                suffix,
            });
        }
        return None;
    }

    let marker_start = segment.text.find('$')?;
    let prefix = &segment.text[..marker_start];
    if prefix.contains('$') {
        return None;
    }

    let name_start = marker_start + 1;
    let name_end = parameter_identifier_end(&segment.text, name_start)?;
    let name = &segment.text[name_start..name_end];
    let suffix = &segment.text[name_end..];
    if suffix.contains('$') {
        return None;
    }
    Some(NativeListWord {
        prefix,
        name,
        suffix,
    })
}

fn parameter_identifier_end(text: &str, start: usize) -> Option<usize> {
    let mut chars = text[start..].char_indices();
    let (_, first) = chars.next()?;
    if first != '_' && !first.is_ascii_alphabetic() {
        return None;
    }

    let mut end = start + first.len_utf8();
    for (offset, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = start + offset + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn expand_quoted_positional_list_fields(
    segments: &[WordSegment],
    state: &mut ShellState,
) -> Result<Option<Vec<ExpandedField>>, ShellError> {
    if !segments.iter().any(contains_double_quoted_at) {
        return Ok(None);
    }

    let mut fields = vec![ExpandedFieldBuilder::default()];
    for segment in segments {
        if contains_double_quoted_at(segment) {
            append_double_quoted_at_segment(&mut fields, &segment.text, state)?;
        } else {
            let fragments = expand_word_fragments(std::slice::from_ref(segment), state, true)?;
            append_expanded_fields(&mut fields, split_expanded_fields(&fragments, state));
        }
    }

    Ok(Some(
        fields
            .into_iter()
            .filter_map(ExpandedFieldBuilder::into_field)
            .collect(),
    ))
}

fn contains_double_quoted_at(segment: &WordSegment) -> bool {
    segment.quote == QuoteKind::Double && next_quoted_at_marker(&segment.text, 0).is_some()
}

fn append_double_quoted_at_segment(
    fields: &mut Vec<ExpandedFieldBuilder>,
    text: &str,
    state: &mut ShellState,
) -> Result<(), ShellError> {
    let mut start = 0;
    while let Some((marker_start, marker_end)) = next_quoted_at_marker(text, start) {
        append_double_quoted_chunk(fields, &text[start..marker_start], state)?;
        append_positionals(fields, &state.positionals());
        start = marker_end;
    }
    append_double_quoted_chunk(fields, &text[start..], state)
}

fn append_double_quoted_chunk(
    fields: &mut Vec<ExpandedFieldBuilder>,
    chunk: &str,
    state: &mut ShellState,
) -> Result<(), ShellError> {
    if chunk.is_empty() {
        return Ok(());
    }

    let fragments =
        expand_substitution_fragments(chunk, state, false, PositionalStarJoin::IfsFirst)?;
    let text = fragments_to_string(&fragments);
    ensure_field_builder(fields).append_quoted_text(text);
    Ok(())
}

fn append_positionals(fields: &mut Vec<ExpandedFieldBuilder>, positionals: &[String]) {
    let Some((first, rest)) = positionals.split_first() else {
        return;
    };

    ensure_field_builder(fields).append_quoted_text(first.clone());
    fields.extend(rest.iter().cloned().map(|text| {
        let mut field = ExpandedFieldBuilder::default();
        field.append_quoted_text(text);
        field
    }));
}

fn append_expanded_fields(fields: &mut Vec<ExpandedFieldBuilder>, expanded: Vec<ExpandedField>) {
    let mut expanded = expanded.into_iter();
    let Some(first) = expanded.next() else {
        return;
    };

    ensure_field_builder(fields).append_field(first);
    fields.extend(expanded.map(|field| {
        let mut builder = ExpandedFieldBuilder::default();
        builder.append_field(field);
        builder
    }));
}

fn ensure_field_builder(fields: &mut Vec<ExpandedFieldBuilder>) -> &mut ExpandedFieldBuilder {
    if fields.is_empty() {
        fields.push(ExpandedFieldBuilder::default());
    }
    fields.last_mut().expect("field builder must exist")
}

fn next_quoted_at_marker(text: &str, start: usize) -> Option<(usize, usize)> {
    let mut index = start;
    while index < text.len() {
        let rest = &text[index..];
        if rest.starts_with("$@") {
            return Some((index, index + 2));
        }
        if rest.starts_with("${@}") {
            return Some((index, index + 4));
        }

        index += rest
            .chars()
            .next()
            .expect("index is inside string bounds")
            .len_utf8();
    }
    None
}

fn expand_word(segments: &[WordSegment], state: &mut ShellState) -> Result<String, ShellError> {
    let fragments = expand_word_fragments(segments, state, false)?;
    Ok(fragments_to_string(&fragments))
}

fn expand_word_fragments(
    segments: &[WordSegment],
    state: &mut ShellState,
    split_expansions: bool,
) -> Result<Vec<ExpansionFragment>, ShellError> {
    let mut fragments = Vec::new();
    for segment in segments {
        match segment.quote {
            QuoteKind::Single => {
                push_fragment(
                    &mut fragments,
                    ExpansionFragment::literal(&segment.text, true, false),
                );
            }
            QuoteKind::Double => {
                let double_fragments = expand_substitution_fragments(
                    &segment.text,
                    state,
                    false,
                    PositionalStarJoin::IfsFirst,
                )?;
                if double_fragments.is_empty() {
                    push_fragment(&mut fragments, ExpansionFragment::literal("", true, false));
                    continue;
                }
                for fragment in double_fragments {
                    push_fragment(
                        &mut fragments,
                        ExpansionFragment::literal(fragment.text, true, false),
                    );
                }
            }
            QuoteKind::None => {
                fragments.extend(expand_substitution_fragments(
                    &segment.text,
                    state,
                    split_expansions,
                    PositionalStarJoin::Space,
                )?);
            }
        }
    }

    if segments
        .first()
        .is_some_and(|segment| segment.quote == QuoteKind::None)
    {
        expand_tilde_in_fragments(&mut fragments, state);
    }

    Ok(fragments)
}

fn expand_substitution_fragments(
    input: &str,
    state: &mut ShellState,
    split_expansions: bool,
    positional_star_join: PositionalStarJoin,
) -> Result<Vec<ExpansionFragment>, ShellError> {
    let mut fragments = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '`' {
            let Some((command_text, next_i)) = read_backtick_command(&chars, i) else {
                return Err(ShellError::parse("unterminated backtick substitution"));
            };
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    run_command_substitution(&command_text, state)?,
                    split_expansions,
                ),
            );
            i = next_i;
            continue;
        }

        if chars[i] != '$' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::literal(chars[i].to_string(), true, true),
            );
            i += 1;
            continue;
        }

        i += 1;
        if i >= chars.len() {
            push_fragment(&mut fragments, ExpansionFragment::literal("$", true, true));
            break;
        }

        if chars[i] == '{' {
            i += 1;
            let start = i;
            // Match nested braces, and skip a `}` inside a quoted default value, so
            // `${VAR:-${INNER}}` and `${x:-'a}b'}` read the full expression.
            let mut depth = 1usize;
            let mut quote: Option<char> = None;
            while i < chars.len() && depth > 0 {
                let ch = chars[i];
                if let Some(q) = quote {
                    if ch == q {
                        quote = None;
                    }
                    i += 1;
                    continue;
                }
                match ch {
                    '\'' | '"' => quote = Some(ch),
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            if i < chars.len() {
                let expression = chars[start..i].iter().collect::<String>();
                push_fragment(
                    &mut fragments,
                    ExpansionFragment::expanded(
                        expand_braced_parameter(&expression, state, positional_star_join)?,
                        split_expansions,
                    ),
                );
                i += 1;
            } else {
                push_fragment(&mut fragments, ExpansionFragment::literal("${", true, true));
                for ch in &chars[start..] {
                    push_fragment(
                        &mut fragments,
                        ExpansionFragment::literal(ch.to_string(), true, true),
                    );
                }
                break;
            }
            continue;
        }

        if chars[i] == '@' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(joined_positionals(state), split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '*' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    joined_star_positionals(state, positional_star_join),
                    split_expansions,
                ),
            );
            i += 1;
            continue;
        }

        if chars[i] == '$' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(std::process::id().to_string(), split_expansions),
            );
            i += 1;
            continue;
        }

        // `$-`: current shell option flags (POSIX 2.5.2).
        if chars[i] == '-' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(state.option_flags(), split_expansions),
            );
            i += 1;
            continue;
        }

        // `$!`: PID of the most recent background command (POSIX 2.5.2).
        if chars[i] == '!' {
            let pid = state
                .last_bg_pid()
                .map(|p| p.to_string())
                .unwrap_or_default();
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(pid, split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '?' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(state.last_status().to_string(), split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '#' {
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(state.positional_count().to_string(), split_expansions),
            );
            i += 1;
            continue;
        }

        if chars[i] == '(' && chars.get(i + 1).is_some_and(|ch| *ch == '(') {
            let expr_start = i + 2;
            let Some((expr_end, next_i)) = find_arithmetic_end(&chars, expr_start) else {
                push_fragment(
                    &mut fragments,
                    ExpansionFragment::literal("$((", true, true),
                );
                i = expr_start;
                continue;
            };
            let expression = chars[expr_start..expr_end].iter().collect::<String>();
            // Expand $-substitutions ($var, $1, $(...), nested $((...))) inside the
            // arithmetic text before evaluating, matching shell semantics.
            let expanded_expression =
                expand_word(&[WordSegment::new(expression, QuoteKind::Double)], state)?;
            // Float-aware: expressions with a decimal point, exponent, or math
            // function/constant evaluate as floating point (bash has no float).
            let value = if crate::math::looks_floating(&expanded_expression) {
                let vars = StateFloatVars(state);
                crate::math::eval(&expanded_expression, &vars)
                    .map(crate::math::format_result)
                    .map_err(|e| ShellError::execution(format!("arithmetic: {e}")))?
            } else {
                eval_arithmetic(&expanded_expression, state)?.to_string()
            };
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(value, split_expansions),
            );
            i = next_i;
            continue;
        }

        if chars[i] == '(' {
            let command_start = i + 1;
            let Some((command_end, next_i)) = find_command_substitution_end(&chars, i) else {
                push_fragment(&mut fragments, ExpansionFragment::literal("$(", true, true));
                i = command_start;
                continue;
            };
            let command_text = chars[command_start..command_end].iter().collect::<String>();
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    run_command_substitution(&command_text, state)?,
                    split_expansions,
                ),
            );
            i = next_i;
            continue;
        }

        let start = i;
        while i < chars.len() && (chars[i] == '_' || chars[i].is_ascii_alphanumeric()) {
            i += 1;
        }
        if start == i {
            push_fragment(&mut fragments, ExpansionFragment::literal("$", true, true));
        } else {
            let name = chars[start..i].iter().collect::<String>();
            push_fragment(
                &mut fragments,
                ExpansionFragment::expanded(
                    expand_required_parameter(&name, state)?,
                    split_expansions,
                ),
            );
        }
    }

    Ok(fragments)
}

fn push_fragment(fragments: &mut Vec<ExpansionFragment>, fragment: ExpansionFragment) {
    if fragment.text.is_empty() {
        if fragment.preserves_field {
            fragments.push(fragment);
        }
        return;
    }

    if let Some(previous) = fragments.last_mut() {
        if previous.split_eligible == fragment.split_eligible
            && previous.preserves_field == fragment.preserves_field
            && previous.glob_eligible == fragment.glob_eligible
        {
            previous.text.push_str(&fragment.text);
            return;
        }
    }
    fragments.push(fragment);
}

fn fragments_to_string(fragments: &[ExpansionFragment]) -> String {
    let mut out = String::new();
    for fragment in fragments {
        out.push_str(&fragment.text);
    }
    out
}

fn expand_tilde_in_fragments(fragments: &mut [ExpansionFragment], state: &ShellState) {
    let Some(first) = fragments.first_mut() else {
        return;
    };
    if first.split_eligible {
        return;
    }

    if first.text == "~" {
        if let Some(home) = state.lookup("HOME") {
            first.text = home.to_string();
        }
    } else if let Some(rest) = first.text.strip_prefix("~/") {
        if let Some(home) = state.lookup("HOME") {
            first.text = format!("{home}/{rest}");
        }
    } else if let Some(rest) = first.text.strip_prefix('~') {
        // `~user` / `~user/path` -> that user's home (POSIX 2.6.1).
        let (user, tail) = match rest.split_once('/') {
            Some((u, t)) => (u, Some(t)),
            None => (rest, None),
        };
        if let Some(home) = user_home(user) {
            first.text = match tail {
                Some(t) => format!("{home}/{t}"),
                None => home,
            };
        }
    }
}

/// Look up a user's home directory from `/etc/passwd` (best-effort; covers local
/// users — directory-service-only users on macOS are not resolved).
fn user_home(user: &str) -> Option<String> {
    let content = std::fs::read_to_string("/etc/passwd").ok()?;
    for line in content.lines() {
        let mut fields = line.split(':');
        if fields.next() == Some(user) {
            // name:passwd:uid:gid:gecos:home:shell -> home is the 5th remaining.
            return fields.nth(4).map(str::to_string);
        }
    }
    None
}

fn split_expanded_fields(
    fragments: &[ExpansionFragment],
    state: &ShellState,
) -> Vec<ExpandedField> {
    let ifs = state.lookup("IFS").unwrap_or(" \t\n");
    if ifs.is_empty() {
        let field = fragments_to_string(fragments);
        return if field.is_empty()
            && !fragments
                .iter()
                .any(|fragment| fragment.preserves_field || !fragment.text.is_empty())
        {
            Vec::new()
        } else {
            vec![ExpandedField {
                text: field,
                glob_mask: fragments
                    .iter()
                    .flat_map(|fragment| {
                        fragment
                            .text
                            .chars()
                            .map(|_| fragment.glob_eligible)
                            .collect::<Vec<_>>()
                    })
                    .collect(),
            }]
        };
    }

    let mut fields = Vec::new();
    let mut current = String::new();
    let mut current_glob_mask = Vec::new();
    let mut current_has_material = false;
    let mut previous_non_whitespace_delimiter = false;

    for fragment in fragments {
        if !fragment.split_eligible {
            current.push_str(&fragment.text);
            current_glob_mask.extend(fragment.text.chars().map(|_| fragment.glob_eligible));
            current_has_material |= fragment.preserves_field || !fragment.text.is_empty();
            if fragment.preserves_field || !fragment.text.is_empty() {
                previous_non_whitespace_delimiter = false;
            }
            continue;
        }

        for ch in fragment.text.chars() {
            if is_ifs_whitespace(ch, ifs) {
                if current_has_material {
                    fields.push(ExpandedField {
                        text: std::mem::take(&mut current),
                        glob_mask: std::mem::take(&mut current_glob_mask),
                    });
                    current_has_material = false;
                }
            } else if is_ifs_delimiter(ch, ifs) {
                if current_has_material || previous_non_whitespace_delimiter || fields.is_empty() {
                    fields.push(ExpandedField {
                        text: std::mem::take(&mut current),
                        glob_mask: std::mem::take(&mut current_glob_mask),
                    });
                } else {
                    current.clear();
                    current_glob_mask.clear();
                }
                current_has_material = false;
                previous_non_whitespace_delimiter = true;
            } else {
                current.push(ch);
                current_glob_mask.push(fragment.glob_eligible);
                current_has_material = true;
                previous_non_whitespace_delimiter = false;
            }
        }
    }

    if current_has_material {
        fields.push(ExpandedField {
            text: current,
            glob_mask: current_glob_mask,
        });
    }

    fields
}

fn is_ifs_whitespace(ch: char, ifs: &str) -> bool {
    matches!(ch, ' ' | '\t' | '\n') && ifs.contains(ch)
}

fn is_ifs_delimiter(ch: char, ifs: &str) -> bool {
    ifs.contains(ch)
}

fn is_glob_metachar(ch: char) -> bool {
    matches!(ch, '*' | '?' | '[')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionalStarJoin {
    Space,
    IfsFirst,
}

#[derive(Default)]
struct ZshFlags {
    split: Option<String>,
    join: Option<String>,
    unique: bool,
    sort: Option<bool>, // Some(false)=asc (o), Some(true)=desc (O)
    numeric: bool,
    case: Option<u8>, // b'U' | b'L' | b'C'
    quote: bool,
    unquote: bool,
    indirect: bool,
    at: bool,
}

/// Parse a `${(flags)...}` flag string. Returns None on a malformed/unknown flag
/// (the caller then falls back to normal expansion).
fn parse_zsh_flags(flag_str: &str) -> Option<ZshFlags> {
    let mut flags = ZshFlags::default();
    let chars: Vec<char> = flag_str.chars().collect();
    let mut i = 0;
    // A delimited argument: the char after the flag is the delimiter, read until
    // it recurs. e.g. `s.:.` -> ":".
    fn read_delim(chars: &[char], i: &mut usize) -> Option<String> {
        let delim = *chars.get(*i)?;
        *i += 1;
        let start = *i;
        while *i < chars.len() && chars[*i] != delim {
            *i += 1;
        }
        if *i >= chars.len() {
            return None;
        }
        let arg: String = chars[start..*i].iter().collect();
        *i += 1; // consume closing delimiter
        Some(arg)
    }
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            'f' => flags.split = Some("\n".to_string()),
            's' => flags.split = Some(read_delim(&chars, &mut i)?),
            'j' => flags.join = Some(read_delim(&chars, &mut i)?),
            'u' => flags.unique = true,
            'o' => flags.sort = Some(false),
            'O' => flags.sort = Some(true),
            'n' => flags.numeric = true,
            'U' => flags.case = Some(b'U'),
            'L' => flags.case = Some(b'L'),
            'C' => flags.case = Some(b'C'),
            'q' => flags.quote = true,
            'Q' => flags.unquote = true,
            'P' => flags.indirect = true,
            '@' => flags.at = true,
            ' ' | ',' => {}
            _ => return None,
        }
    }
    Some(flags)
}

/// Expand `${(flags)name}` zsh-style. Returns Ok(None) if the flags or body are
/// not recognized (so the caller falls back to normal handling).
fn expand_zsh_param_flags(
    expression: &str,
    state: &mut ShellState,
) -> Result<Option<String>, ShellError> {
    let Some(close) = expression.find(')') else {
        return Ok(None);
    };
    let flag_str = &expression[1..close];
    let body = &expression[close + 1..];
    let Some(flags) = parse_zsh_flags(flag_str) else {
        return Ok(None);
    };

    // Compute the base items from the body (array elements, or a scalar).
    let base_name: String = if flags.indirect {
        lookup_parameter(body, state).unwrap_or("").to_string()
    } else {
        body.to_string()
    };
    let stripped = base_name
        .strip_suffix("[@]")
        .or_else(|| base_name.strip_suffix("[*]"));
    let mut items: Vec<String> = if let Some(stripped) = stripped {
        if state.is_assoc(stripped) {
            state.assoc_values(stripped).unwrap_or_default()
        } else {
            state
                .array(stripped)
                .map(<[String]>::to_vec)
                .unwrap_or_default()
        }
    } else if let Some(arr) = state.array(&base_name) {
        arr.to_vec()
    } else if state.is_assoc(&base_name) {
        state.assoc_values(&base_name).unwrap_or_default()
    } else {
        vec![lookup_parameter(&base_name, state)
            .unwrap_or("")
            .to_string()]
    };

    // Split (f / s:sep:).
    if let Some(sep) = &flags.split {
        items = items
            .iter()
            .flat_map(|item| {
                if sep.is_empty() {
                    vec![item.clone()]
                } else {
                    item.split(sep.as_str()).map(str::to_string).collect()
                }
            })
            .collect();
    }
    // Unique (preserving first-seen order).
    if flags.unique {
        let mut seen = std::collections::HashSet::new();
        items.retain(|item| seen.insert(item.clone()));
    }
    // Sort.
    if let Some(desc) = flags.sort {
        if flags.numeric {
            items.sort_by(|a, b| {
                let (x, y) = (
                    a.parse::<f64>().unwrap_or(0.0),
                    b.parse::<f64>().unwrap_or(0.0),
                );
                x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            items.sort();
        }
        if desc {
            items.reverse();
        }
    }
    // Case.
    if let Some(kind) = flags.case {
        for item in &mut items {
            *item = match kind {
                b'U' => item.to_uppercase(),
                b'L' => item.to_lowercase(),
                _ => capitalize_words(item),
            };
        }
    }
    // Quote / unquote.
    if flags.quote {
        for item in &mut items {
            *item = shell_quote(item);
        }
    } else if flags.unquote {
        for item in &mut items {
            *item = item.trim_matches(|c| c == '\'' || c == '"').to_string();
        }
    }

    let sep = flags.join.clone().unwrap_or_else(|| " ".to_string());
    Ok(Some(items.join(&sep)))
}

/// Capitalize the first letter of each whitespace-separated word.
fn capitalize_words(s: &str) -> String {
    s.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Single-quote a string for safe shell re-use (zsh `(q)` flag).
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn expand_braced_parameter(
    expression: &str,
    state: &mut ShellState,
    positional_star_join: PositionalStarJoin,
) -> Result<String, ShellError> {
    // zsh-style parameter expansion flags: `${(flags)name}` (the `(` right after
    // `${` is invalid in bash, so this never collides). e.g. `${(U)v}`,
    // `${(s.:.)PATH}`, `${(j:,:)arr}`, `${(fu)var}`, `${(oa)arr}`.
    if expression.starts_with('(') {
        if let Some(result) = expand_zsh_param_flags(expression, state)? {
            return Ok(result);
        }
    }

    if expression == "#" {
        return Ok(state.positional_count().to_string());
    }

    if expression == "*" {
        return Ok(joined_star_positionals(state, positional_star_join));
    }

    if expression == "@" {
        return Ok(joined_positionals(state));
    }

    // Array subscripts: ${a[i]}, ${a[@]}, ${a[*]}, ${#a[@]}, ${!a[@]}.
    if expression.contains('[') {
        if let Some(value) = expand_array_subscript(expression, state) {
            return Ok(value);
        }
    }

    if let Some(name) = expression.strip_prefix('#') {
        let Some(value) = lookup_parameter(name, state) else {
            if state.nounset() {
                return Err(unset_parameter_error(name));
            }
            return Ok("0".to_string());
        };
        return Ok(value.chars().count().to_string());
    }

    if expression == "?" {
        return Ok(state.last_status().to_string());
    }

    if let Some(value) = dynamic_special_var(expression, state) {
        return Ok(value);
    }

    // Indirect expansion: `${!name}` -> value of the variable named by `$name`.
    if let Some(inner) = expression.strip_prefix('!') {
        // `${!prefix*}` / `${!prefix@}`: names of variables with this prefix.
        if let Some(prefix) = inner.strip_suffix('*').or_else(|| inner.strip_suffix('@')) {
            if prefix.is_empty() || is_identifier(prefix) {
                let names: Vec<String> = state
                    .vars()
                    .keys()
                    .filter(|k| k.starts_with(prefix))
                    .cloned()
                    .collect();
                return Ok(names.join(" "));
            }
        }
        if is_identifier(inner) {
            let target = lookup_parameter(inner, state)
                .unwrap_or_default()
                .to_string();
            if target.is_empty() {
                if state.nounset() {
                    return Err(unset_parameter_error(inner));
                }
                return Ok(String::new());
            }
            return Ok(lookup_parameter(&target, state)
                .unwrap_or_default()
                .to_string());
        }
    }

    let Some((name, operator, word)) = parse_parameter_expression(expression) else {
        return Ok(state.lookup(expression).unwrap_or_default().to_string());
    };

    let value = lookup_parameter(name, state).map(str::to_string);
    let is_set = value.is_some();
    let is_non_null = value.as_deref().is_some_and(|value| !value.is_empty());

    match operator {
        ParameterOperator::None => {
            if let Some(value) = value {
                Ok(value)
            } else if state.nounset() {
                Err(unset_parameter_error(name))
            } else {
                Ok(String::new())
            }
        }
        ParameterOperator::Default { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                Ok(value.unwrap_or_default())
            } else {
                expand_substitutions(word, state)
            }
        }
        ParameterOperator::AssignDefault { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                Ok(value.unwrap_or_default())
            } else {
                if !is_identifier(name) {
                    return Err(ShellError::execution(format!(
                        "{name}: cannot assign default to this parameter"
                    )));
                }
                let expanded = expand_substitutions(word, state)?;
                state.set_var(name, expanded.clone());
                Ok(expanded)
            }
        }
        ParameterOperator::Alternate { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                expand_substitutions(word, state)
            } else {
                Ok(String::new())
            }
        }
        ParameterOperator::Error { require_non_null } => {
            if parameter_condition(is_set, is_non_null, require_non_null) {
                Ok(value.unwrap_or_default())
            } else {
                let message = if word.is_empty() {
                    if require_non_null {
                        "parameter null or not set"
                    } else {
                        "parameter not set"
                    }
                } else {
                    word
                };
                Err(ShellError::execution(format!("{name}: {message}")))
            }
        }
        ParameterOperator::RemovePrefix { longest } => {
            let pattern = expand_substitutions(word, state)?;
            Ok(remove_pattern_prefix(
                &value.unwrap_or_default(),
                &pattern,
                longest,
            ))
        }
        ParameterOperator::RemoveSuffix { longest } => {
            let pattern = expand_substitutions(word, state)?;
            Ok(remove_pattern_suffix(
                &value.unwrap_or_default(),
                &pattern,
                longest,
            ))
        }
        ParameterOperator::Substitute { mode } => {
            let (pattern, replacement) = split_substitution_word(word);
            let pattern = expand_substitutions(pattern, state)?;
            let replacement = expand_substitutions(replacement, state)?;
            Ok(substitute_pattern(
                &value.unwrap_or_default(),
                &pattern,
                &replacement,
                mode,
            ))
        }
        ParameterOperator::Substring => {
            let chars: Vec<char> = value.unwrap_or_default().chars().collect();
            let len = chars.len() as i64;
            let (offset_expr, length_expr) = match word.split_once(':') {
                Some((o, l)) => (o.trim(), Some(l.trim())),
                None => (word.trim(), None),
            };
            let offset = eval_arithmetic(offset_expr, state)?;
            let start = if offset < 0 {
                (len + offset).max(0)
            } else {
                offset.min(len)
            };
            let end = match length_expr {
                None => len,
                Some(l) => {
                    let length = eval_arithmetic(l, state)?;
                    if length < 0 {
                        (len + length).max(start)
                    } else {
                        (start + length).min(len)
                    }
                }
            };
            let (start, end) = (start as usize, end.max(start) as usize);
            Ok(chars[start..end].iter().collect())
        }
        ParameterOperator::Case { upper, all } => {
            let v = value.unwrap_or_default();
            let convert = |c: char| {
                if upper {
                    c.to_ascii_uppercase()
                } else {
                    c.to_ascii_lowercase()
                }
            };
            let result = if all {
                v.chars().map(convert).collect()
            } else {
                let mut chars = v.chars();
                match chars.next() {
                    Some(first) => {
                        let mut s = convert(first).to_string();
                        s.extend(chars);
                        s
                    }
                    None => String::new(),
                }
            };
            Ok(result)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterOperator {
    None,
    Default {
        require_non_null: bool,
    },
    AssignDefault {
        require_non_null: bool,
    },
    Alternate {
        require_non_null: bool,
    },
    Error {
        require_non_null: bool,
    },
    RemovePrefix {
        longest: bool,
    },
    RemoveSuffix {
        longest: bool,
    },
    Substitute {
        mode: SubstitutionMode,
    },
    /// `${var:offset}` / `${var:offset:length}` substring (arithmetic operands).
    Substring,
    /// `${var^^}`/`${var^}`/`${var,,}`/`${var,}` case modification (bash 4).
    Case {
        upper: bool,
        all: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubstitutionMode {
    First,
    Global,
    Prefix,
    Suffix,
}

fn parse_parameter_expression(expression: &str) -> Option<(&str, ParameterOperator, &str)> {
    let name_len = parameter_name_len(expression)?;
    let name = &expression[..name_len];
    let rest = &expression[name_len..];
    match rest {
        "" => Some((name, ParameterOperator::None, "")),
        rest if rest.starts_with(":-") => Some((
            name,
            ParameterOperator::Default {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with(":=") => Some((
            name,
            ParameterOperator::AssignDefault {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('-') => Some((
            name,
            ParameterOperator::Default {
                require_non_null: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with('=') => Some((
            name,
            ParameterOperator::AssignDefault {
                require_non_null: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with(":+") => Some((
            name,
            ParameterOperator::Alternate {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('+') => Some((
            name,
            ParameterOperator::Alternate {
                require_non_null: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with(":?") => Some((
            name,
            ParameterOperator::Error {
                require_non_null: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('?') => Some((
            name,
            ParameterOperator::Error {
                require_non_null: false,
            },
            &rest[1..],
        )),
        // Substring `${var:offset[:length]}`. Checked after the `:-/:=/:+/:?`
        // modifiers so they win; a bare `:` (incl. `: -1`) is a substring.
        rest if rest.starts_with(':') => Some((name, ParameterOperator::Substring, &rest[1..])),
        rest if rest.starts_with("##") => Some((
            name,
            ParameterOperator::RemovePrefix { longest: true },
            &rest[2..],
        )),
        rest if rest.starts_with('#') => Some((
            name,
            ParameterOperator::RemovePrefix { longest: false },
            &rest[1..],
        )),
        rest if rest.starts_with("%%") => Some((
            name,
            ParameterOperator::RemoveSuffix { longest: true },
            &rest[2..],
        )),
        rest if rest.starts_with('%') => Some((
            name,
            ParameterOperator::RemoveSuffix { longest: false },
            &rest[1..],
        )),
        rest if rest.starts_with("//") => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::Global,
            },
            &rest[2..],
        )),
        rest if rest.starts_with("/#") => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::Prefix,
            },
            &rest[2..],
        )),
        rest if rest.starts_with("/%") => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::Suffix,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('/') => Some((
            name,
            ParameterOperator::Substitute {
                mode: SubstitutionMode::First,
            },
            &rest[1..],
        )),
        // Case modification (bash 4): ^^ ^ ,, , — optional trailing pattern.
        rest if rest.starts_with("^^") => Some((
            name,
            ParameterOperator::Case {
                upper: true,
                all: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with('^') => Some((
            name,
            ParameterOperator::Case {
                upper: true,
                all: false,
            },
            &rest[1..],
        )),
        rest if rest.starts_with(",,") => Some((
            name,
            ParameterOperator::Case {
                upper: false,
                all: true,
            },
            &rest[2..],
        )),
        rest if rest.starts_with(',') => Some((
            name,
            ParameterOperator::Case {
                upper: false,
                all: false,
            },
            &rest[1..],
        )),
        _ => None,
    }
}

fn remove_pattern_prefix(value: &str, pattern: &str, longest: bool) -> String {
    let mut matched_index = None;
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    for index in char_boundaries(value) {
        if glob_match_bytes(pattern_bytes, &value_bytes[..index]) {
            matched_index = Some(match (matched_index, longest) {
                (Some(previous), false) => previous,
                _ => index,
            });
            if !longest {
                break;
            }
        }
    }

    matched_index
        .map(|index| value[index..].to_string())
        .unwrap_or_else(|| value.to_string())
}

fn remove_pattern_suffix(value: &str, pattern: &str, longest: bool) -> String {
    let mut matched_index = None;
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    for index in char_boundaries(value) {
        if glob_match_bytes(pattern_bytes, &value_bytes[index..]) {
            matched_index = Some(match (matched_index, longest) {
                (Some(previous), true) => previous,
                _ => index,
            });
            if longest {
                break;
            }
        }
    }

    matched_index
        .map(|index| value[..index].to_string())
        .unwrap_or_else(|| value.to_string())
}

/// Record each pipeline stage's exit code into the `PIPESTATUS` array.
fn record_pipestatus(state: &mut ShellState, exit_codes: &[i32]) {
    state.set_array(
        "PIPESTATUS",
        exit_codes.iter().map(i32::to_string).collect(),
        false,
    );
}

fn split_substitution_word(word: &str) -> (&str, &str) {
    word.split_once('/').unwrap_or((word, ""))
}

fn substitute_pattern(
    value: &str,
    pattern: &str,
    replacement: &str,
    mode: SubstitutionMode,
) -> String {
    if pattern.is_empty() {
        return value.to_string();
    }

    match mode {
        SubstitutionMode::First => replace_first_pattern_match(value, pattern, replacement)
            .unwrap_or_else(|| value.to_string()),
        SubstitutionMode::Global => replace_all_pattern_matches(value, pattern, replacement),
        SubstitutionMode::Prefix => replace_prefix_pattern_match(value, pattern, replacement)
            .unwrap_or_else(|| value.to_string()),
        SubstitutionMode::Suffix => replace_suffix_pattern_match(value, pattern, replacement)
            .unwrap_or_else(|| value.to_string()),
    }
}

fn replace_first_pattern_match(value: &str, pattern: &str, replacement: &str) -> Option<String> {
    let (start, end) = find_pattern_match(value, pattern, 0)?;
    Some(format!(
        "{}{}{}",
        &value[..start],
        replacement,
        &value[end..]
    ))
}

fn replace_all_pattern_matches(value: &str, pattern: &str, replacement: &str) -> String {
    let mut out = String::new();
    let mut search_start = 0;

    while search_start < value.len() {
        let Some((start, end)) = find_pattern_match(value, pattern, search_start) else {
            break;
        };
        out.push_str(&value[search_start..start]);
        out.push_str(replacement);
        if end == start {
            if end >= value.len() {
                search_start = end;
                break;
            }
            let next_index = value[end..]
                .char_indices()
                .nth(1)
                .map_or(value.len(), |(offset, _)| end + offset);
            out.push_str(&value[end..next_index]);
            search_start = next_index;
        } else {
            search_start = end;
        }
    }

    out.push_str(&value[search_start..]);
    out
}

fn replace_prefix_pattern_match(value: &str, pattern: &str, replacement: &str) -> Option<String> {
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    let end = char_boundaries(value)
        .filter(|index| glob_match_bytes(pattern_bytes, &value_bytes[..*index]))
        .max()?;
    Some(format!("{replacement}{}", &value[end..]))
}

fn replace_suffix_pattern_match(value: &str, pattern: &str, replacement: &str) -> Option<String> {
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    let start = char_boundaries(value)
        .filter(|index| glob_match_bytes(pattern_bytes, &value_bytes[*index..]))
        .min()?;
    Some(format!("{}{replacement}", &value[..start]))
}

fn find_pattern_match(value: &str, pattern: &str, search_start: usize) -> Option<(usize, usize)> {
    let pattern_bytes = pattern.as_bytes();
    let value_bytes = value.as_bytes();
    let boundaries = char_boundaries(value).collect::<Vec<_>>();

    for start in boundaries
        .iter()
        .copied()
        .filter(|index| *index >= search_start)
    {
        let mut best_end = None;
        for end in boundaries.iter().copied().filter(|index| *index >= start) {
            if glob_match_bytes(pattern_bytes, &value_bytes[start..end]) {
                best_end = Some(end);
            }
        }
        if let Some(end) = best_end {
            return Some((start, end));
        }
    }

    None
}

/// Parse a leading extended-glob group `?(..)`/`*(..)`/`+(..)`/`@(..)`/`!(..)`.
/// Returns (operator, alternatives, pattern after the closing `)`), or None.
/// (operator byte, alternatives, pattern remainder) for an extglob group.
type ExtglobGroup<'a> = (u8, Vec<&'a [u8]>, &'a [u8]);

fn parse_extglob_group(pattern: &[u8]) -> Option<ExtglobGroup<'_>> {
    let op = *pattern.first()?;
    if !matches!(op, b'?' | b'*' | b'+' | b'@' | b'!') || pattern.get(1) != Some(&b'(') {
        return None;
    }
    // Find the matching close paren (respecting nesting and `[...]`).
    let mut depth = 0usize;
    let mut i = 1;
    let mut in_class = false;
    while i < pattern.len() {
        match pattern[i] {
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => depth += 1,
            b')' if !in_class => {
                depth -= 1;
                if depth == 0 {
                    let inner = &pattern[2..i];
                    let rest = &pattern[i + 1..];
                    return Some((op, split_extglob_alts(inner), rest));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Split extglob inner on top-level `|` (respecting nested `(`/`[`).
fn split_extglob_alts(inner: &[u8]) -> Vec<&[u8]> {
    let mut alts = Vec::new();
    let mut depth = 0usize;
    let mut in_class = false;
    let mut start = 0;
    for (i, &b) in inner.iter().enumerate() {
        match b {
            b'[' if !in_class => in_class = true,
            b']' if in_class => in_class = false,
            b'(' if !in_class => depth += 1,
            b')' if !in_class && depth > 0 => depth -= 1,
            b'|' if !in_class && depth == 0 => {
                alts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    alts.push(&inner[start..]);
    alts
}

/// Match an extended-glob group against `name`, then `rest` against the remainder.
fn match_extglob(op: u8, alts: &[&[u8]], rest: &[u8], name: &[u8]) -> bool {
    // Whether some alternative fully matches `name[..len]`.
    let alt_matches = |len: usize| alts.iter().any(|alt| glob_match_bytes(alt, &name[..len]));
    match op {
        b'@' => (0..=name.len()).any(|i| alt_matches(i) && glob_match_bytes(rest, &name[i..])),
        b'?' => {
            glob_match_bytes(rest, name)
                || (1..=name.len()).any(|i| alt_matches(i) && glob_match_bytes(rest, &name[i..]))
        }
        b'*' => {
            glob_match_bytes(rest, name)
                || (1..=name.len())
                    .any(|i| alt_matches(i) && match_extglob(b'*', alts, rest, &name[i..]))
        }
        b'+' => (1..=name.len()).any(|i| {
            alt_matches(i)
                && (glob_match_bytes(rest, &name[i..])
                    || match_extglob(b'*', alts, rest, &name[i..]))
        }),
        b'!' => (0..=name.len()).any(|i| !alt_matches(i) && glob_match_bytes(rest, &name[i..])),
        _ => false,
    }
}

fn glob_match_bytes(pattern: &[u8], name: &[u8]) -> bool {
    // Extended glob groups: ?(..) *(..) +(..) @(..) !(..) with `|` alternation.
    if let Some((op, alts, rest)) = parse_extglob_group(pattern) {
        return match_extglob(op, &alts, rest, name);
    }
    match (pattern.split_first(), name.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&b'*', rest)), _) => {
            glob_match_bytes(rest, name)
                || name
                    .split_first()
                    .is_some_and(|(_, name_rest)| glob_match_bytes(pattern, name_rest))
        }
        (Some((&b'?', rest)), Some((_, name_rest))) => glob_match_bytes(rest, name_rest),
        (Some((&b'[', _)), Some((name_ch, name_rest))) => {
            if let Some((matched, rest)) = match_byte_char_class(pattern, *name_ch) {
                matched && glob_match_bytes(rest, name_rest)
            } else {
                pattern.first() == Some(name_ch) && glob_match_bytes(&pattern[1..], name_rest)
            }
        }
        (Some((pattern_ch, pattern_rest)), Some((name_ch, name_rest))) if pattern_ch == name_ch => {
            glob_match_bytes(pattern_rest, name_rest)
        }
        _ => false,
    }
}

/// POSIX bracket character class membership (`[:alpha:]` etc.), C/POSIX locale.
fn char_class_matches(class: &[u8], ch: u8) -> bool {
    match class {
        b"alpha" => ch.is_ascii_alphabetic(),
        b"digit" => ch.is_ascii_digit(),
        b"alnum" => ch.is_ascii_alphanumeric(),
        b"upper" => ch.is_ascii_uppercase(),
        b"lower" => ch.is_ascii_lowercase(),
        b"space" => ch.is_ascii_whitespace() || ch == 0x0b,
        b"blank" => ch == b' ' || ch == b'\t',
        b"punct" => ch.is_ascii_punctuation(),
        b"print" => ch.is_ascii_graphic() || ch == b' ',
        b"graph" => ch.is_ascii_graphic(),
        b"cntrl" => ch.is_ascii_control(),
        b"xdigit" => ch.is_ascii_hexdigit(),
        _ => false,
    }
}

fn match_byte_char_class(pattern: &[u8], name_ch: u8) -> Option<(bool, &[u8])> {
    if pattern.first().copied() != Some(b'[') {
        return None;
    }
    let mut index = 1;
    let negate = matches!(pattern.get(index), Some(b'!') | Some(b'^'));
    if negate {
        index += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while index < pattern.len() {
        if pattern[index] == b']' && saw_member {
            return Some((
                if negate { !matched } else { matched },
                &pattern[index + 1..],
            ));
        }

        // POSIX character class `[:name:]` (e.g. `[[:alpha:]]`).
        if pattern[index] == b'[' && pattern.get(index + 1) == Some(&b':') {
            if let Some(rel) = pattern[index + 2..].windows(2).position(|w| w == b":]") {
                let class = &pattern[index + 2..index + 2 + rel];
                if char_class_matches(class, name_ch) {
                    matched = true;
                }
                index += 2 + rel + 2; // past the closing ":]"
                saw_member = true;
                continue;
            }
        }

        let start = pattern[index];
        if pattern.get(index + 1) == Some(&b'-')
            && pattern.get(index + 2).is_some()
            && pattern[index + 2] != b']'
        {
            let end = pattern[index + 2];
            let (low, high) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            if (low..=high).contains(&name_ch) {
                matched = true;
            }
            index += 3;
        } else {
            if start == name_ch {
                matched = true;
            }
            index += 1;
        }
        saw_member = true;
    }

    None
}

fn char_boundaries(value: &str) -> impl Iterator<Item = usize> + '_ {
    value
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(value.len()))
}

fn parameter_name_len(expression: &str) -> Option<usize> {
    let mut chars = expression.char_indices();
    let (_, first) = chars.next()?;
    if first == '@' {
        return Some(first.len_utf8());
    }
    if first.is_ascii_digit() {
        let mut end = first.len_utf8();
        for (index, ch) in chars {
            if ch.is_ascii_digit() {
                end = index + ch.len_utf8();
            } else {
                break;
            }
        }
        return Some(end);
    }
    if first != '_' && !first.is_ascii_alphabetic() {
        return None;
    }

    let mut end = first.len_utf8();
    for (index, ch) in chars {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    Some(end)
}

fn parameter_condition(is_set: bool, is_non_null: bool, require_non_null: bool) -> bool {
    if require_non_null {
        is_non_null
    } else {
        is_set
    }
}

fn lookup_parameter<'a>(name: &str, state: &'a ShellState) -> Option<&'a str> {
    if name == "0" {
        // $0 is the shell/command name; it is not a normal positional parameter.
        return Some("agsh");
    }
    if name == "@" {
        state.lookup("@")
    } else {
        // `$name` on an array reads element 0 (bash semantics) when no scalar.
        state.lookup(name).or_else(|| {
            state
                .array(name)
                .and_then(|a| a.first())
                .map(String::as_str)
        })
    }
}

fn expand_required_parameter(name: &str, state: &ShellState) -> Result<String, ShellError> {
    if let Some(value) = dynamic_special_var(name, state) {
        Ok(value)
    } else if let Some(value) = lookup_parameter(name, state) {
        Ok(value.to_string())
    } else if state.nounset() {
        Err(unset_parameter_error(name))
    } else {
        Ok(String::new())
    }
}

/// Dynamic special variables computed on each read (`$RANDOM`, `$SECONDS`).
/// Checked before normal lookup so they behave like bash even if a same-named
/// variable was assigned.
fn dynamic_special_var(name: &str, state: &ShellState) -> Option<String> {
    match name {
        "RANDOM" => Some(state.next_random().to_string()),
        "SECONDS" => Some(state.uptime_secs().to_string()),
        "LINENO" => Some(state.current_line().to_string()),
        "PPID" => Some(
            rustix::process::getppid()
                .map(|p| p.as_raw_nonzero().get().to_string())
                .unwrap_or_else(|| "0".to_string()),
        ),
        _ => None,
    }
}

fn unset_parameter_error(name: &str) -> ShellError {
    ShellError::execution(format!("{name}: parameter not set"))
}

fn joined_positionals(state: &ShellState) -> String {
    state.positionals().join(" ")
}

fn joined_star_positionals(state: &ShellState, mode: PositionalStarJoin) -> String {
    match mode {
        PositionalStarJoin::Space => joined_positionals(state),
        PositionalStarJoin::IfsFirst => state.positionals().join(&quoted_star_separator(state)),
    }
}

fn quoted_star_separator(state: &ShellState) -> String {
    state
        .lookup("IFS")
        .map(|ifs| {
            ifs.chars()
                .next()
                .map(|ch| ch.to_string())
                .unwrap_or_default()
        })
        .unwrap_or_else(|| " ".to_string())
}

fn read_backtick_command(chars: &[char], start_i: usize) -> Option<(String, usize)> {
    let mut command = String::new();
    let mut i = start_i + 1;
    while i < chars.len() {
        match chars[i] {
            '`' => return Some((command, i + 1)),
            '\\' if chars.get(i + 1).is_some() => {
                i += 1;
                command.push(chars[i]);
            }
            ch => command.push(ch),
        }
        i += 1;
    }
    None
}

fn find_command_substitution_end(chars: &[char], open_index: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    // Skip metacharacters inside quotes so `$(echo ')')` / `$(echo "a)b")` aren't
    // closed at a parenthesis that's actually inside a quoted string.
    let mut quote: Option<char> = None;
    for (index, ch) in chars.iter().enumerate().skip(open_index) {
        if let Some(q) = quote {
            if *ch == q {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(*ch),
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some((index, index + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn find_arithmetic_end(chars: &[char], expr_start: usize) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut index = expr_start;
    while index < chars.len() {
        match chars[index] {
            '(' => depth += 1,
            ')' if chars.get(index + 1).is_some_and(|ch| *ch == ')') && depth == 0 => {
                return Some((index, index + 2));
            }
            ')' => depth = depth.checked_sub(1)?,
            _ => {}
        }
        index += 1;
    }
    None
}

/// Implement input process substitution `<(cmd)` via a temp file: run `cmd`,
/// capture its raw stdout, write it to a temp file, and return the file path
/// (registered for cleanup at the command boundary). This is non-streaming but
/// behaviorally equivalent to bash's `/dev/fd` for finite output, and stays
/// unsafe-free.
fn process_substitution_path(inner: &str, state: &mut ShellState) -> Result<String, ShellError> {
    let graph = parse_line(inner)?;
    let mut sub_state = state.clone();
    sub_state.replace_streaming_stdout(None);
    let mut executor = Executor::new();
    let outcome = executor.run_graph(
        &graph,
        &mut sub_state,
        &ExecutionOptions {
            output_mode: OutputMode::Clean,
            allow_process_replacement: false,
        },
    )?;
    let path = std::env::temp_dir().join(format!(
        "agsh-procsub-{}-{}",
        std::process::id(),
        state.next_random()
    ));
    std::fs::write(&path, &outcome.stdout)
        .map_err(|e| ShellError::execution(format!("process substitution: {e}")))?;
    let display = path.display().to_string();
    state.register_proc_sub_temp(path);
    Ok(display)
}

fn run_command_substitution(
    command_text: &str,
    state: &mut ShellState,
) -> Result<String, ShellError> {
    let graph = parse_line(command_text)?;
    let mut substitution_state = state.clone();
    substitution_state.replace_streaming_stdout(None);
    let mut executor = Executor::new();
    let outcome = executor.run_graph(
        &graph,
        &mut substitution_state,
        &ExecutionOptions {
            output_mode: OutputMode::Clean,
            allow_process_replacement: false,
        },
    )?;
    // Record the status so `x=$(cmd)` can report it as `$?`.
    state.set_command_substitution_status(outcome.exit_code);
    let mut text = String::from_utf8_lossy(&outcome.stdout).to_string();
    while text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// Adapter so float expressions can read shell variables as numbers.
pub(crate) struct StateFloatVars<'a>(pub(crate) &'a ShellState);

impl crate::math::FloatVars for StateFloatVars<'_> {
    fn get(&self, name: &str) -> Option<f64> {
        self.0
            .lookup(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
    }
}

pub(crate) fn eval_arithmetic(expression: &str, state: &mut ShellState) -> Result<i64, ShellError> {
    ArithmeticParser::new(expression, state).parse()
}

struct ArithmeticParser<'a> {
    chars: Vec<char>,
    index: usize,
    state: &'a mut ShellState,
    /// Active recursion depth; bounds pathological nesting (`((((…))))`, `~~~…`,
    /// `1**1**…`) before it can overflow the stack.
    depth: usize,
}

/// Max recursive-descent depth for `$(( … ))`; keeps the worst case well under
/// the stack while exceeding any real expression.
const MAX_ARITH_DEPTH: usize = 512;

impl<'a> ArithmeticParser<'a> {
    fn new(expression: &str, state: &'a mut ShellState) -> Self {
        Self {
            chars: expression.chars().collect(),
            index: 0,
            state,
            depth: 0,
        }
    }

    /// Run `body` one recursion level deeper, erroring if too deeply nested.
    fn guarded_arith(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<i64, ShellError>,
    ) -> Result<i64, ShellError> {
        self.depth += 1;
        if self.depth > MAX_ARITH_DEPTH {
            self.depth -= 1;
            return Err(ShellError::parse("arithmetic expression nested too deeply"));
        }
        let r = body(self);
        self.depth -= 1;
        r
    }

    fn parse(mut self) -> Result<i64, ShellError> {
        // An empty arithmetic expression evaluates to 0.
        self.skip_ws();
        if self.index == self.chars.len() {
            return Ok(0);
        }
        let value = self.parse_expr()?;
        self.skip_ws();
        if self.index != self.chars.len() {
            return Err(ShellError::parse("invalid arithmetic expression"));
        }
        Ok(value)
    }

    fn parse_expr(&mut self) -> Result<i64, ShellError> {
        self.parse_assignment()
    }

    /// Lowest precedence, right-associative: `lvalue [op]= rhs`.
    fn parse_assignment(&mut self) -> Result<i64, ShellError> {
        let save = self.index;
        self.skip_ws();
        if let Some(name) = self.try_read_identifier() {
            self.skip_ws();
            if let Some(op) = self.try_read_assignment_op() {
                // Guard the right-associative RHS recursion (`a=a=…=1`) so a long
                // assignment chain errors cleanly instead of overflowing the stack.
                let rhs = self.guarded_arith(|s| s.parse_assignment())?;
                let value = self.apply_assignment(&name, op, rhs)?;
                return Ok(value);
            }
        }
        self.index = save;
        self.parse_ternary()
    }

    fn try_read_identifier(&mut self) -> Option<String> {
        let start = self.index;
        match self.peek() {
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
            _ => return None,
        }
        self.index += 1;
        while self
            .peek()
            .is_some_and(|c| c == '_' || c.is_ascii_alphanumeric())
        {
            self.index += 1;
        }
        Some(self.chars[start..self.index].iter().collect())
    }

    /// Read an assignment operator (`=`, `+=`, `<<=`, ...) but never `==`.
    fn try_read_assignment_op(&mut self) -> Option<&'static str> {
        let two = (self.peek(), self.peek_at(1));
        let three = self.peek_at(2);
        let op: &'static str = match two {
            (Some('<'), Some('<')) if three == Some('=') => "<<=",
            (Some('>'), Some('>')) if three == Some('=') => ">>=",
            (Some('+'), Some('=')) => "+=",
            (Some('-'), Some('=')) => "-=",
            (Some('*'), Some('=')) => "*=",
            (Some('/'), Some('=')) => "/=",
            (Some('%'), Some('=')) => "%=",
            (Some('&'), Some('=')) => "&=",
            (Some('|'), Some('=')) => "|=",
            (Some('^'), Some('=')) => "^=",
            (Some('='), next) if next != Some('=') => "=",
            _ => return None,
        };
        self.index += op.len();
        Some(op)
    }

    fn apply_assignment(&mut self, name: &str, op: &str, rhs: i64) -> Result<i64, ShellError> {
        let current = self.read_var(name);
        let value = match op {
            "=" => rhs,
            "+=" => current.wrapping_add(rhs),
            "-=" => current.wrapping_sub(rhs),
            "*=" => current.wrapping_mul(rhs),
            "/=" if rhs == 0 => return Err(ShellError::execution("division by zero")),
            "/=" => current.wrapping_div(rhs),
            "%=" if rhs == 0 => return Err(ShellError::execution("division by zero")),
            "%=" => current.wrapping_rem(rhs),
            "&=" => current & rhs,
            "|=" => current | rhs,
            "^=" => current ^ rhs,
            "<<=" => current.wrapping_shl(rhs as u32),
            ">>=" => current.wrapping_shr(rhs as u32),
            _ => return Err(ShellError::parse("invalid assignment operator")),
        };
        self.write_var(name, value);
        Ok(value)
    }

    fn read_var(&self, name: &str) -> i64 {
        self.state
            .lookup(name)
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(0)
    }

    fn write_var(&mut self, name: &str, value: i64) {
        self.state.set_var(name, value.to_string());
    }

    fn parse_ternary(&mut self) -> Result<i64, ShellError> {
        let condition = self.parse_logical_or()?;
        self.skip_ws();
        if self.peek() == Some('?') {
            self.index += 1;
            // Guard the ternary recursion (`1?1?…?1`) against stack overflow.
            let when_true = self.guarded_arith(|s| s.parse_ternary())?;
            self.skip_ws();
            if self.peek() != Some(':') {
                return Err(ShellError::parse("missing ':' in arithmetic ternary"));
            }
            self.index += 1;
            let when_false = self.guarded_arith(|s| s.parse_ternary())?;
            return Ok(if condition != 0 {
                when_true
            } else {
                when_false
            });
        }
        Ok(condition)
    }

    fn parse_logical_or(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_logical_and()?;
        loop {
            self.skip_ws();
            if self.eat2('|', '|') {
                let rhs = self.parse_logical_and()?;
                value = i64::from(value != 0 || rhs != 0);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_logical_and(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_bit_or()?;
        loop {
            self.skip_ws();
            if self.eat2('&', '&') {
                let rhs = self.parse_bit_or()?;
                value = i64::from(value != 0 && rhs != 0);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_bit_or(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_bit_xor()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') && self.peek_at(1) != Some('|') {
                self.index += 1;
                value |= self.parse_bit_xor()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_bit_xor(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_bit_and()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('^') {
                self.index += 1;
                value ^= self.parse_bit_and()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_bit_and(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_equality()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('&') && self.peek_at(1) != Some('&') {
                self.index += 1;
                value &= self.parse_equality()?;
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_equality(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_relational()?;
        loop {
            self.skip_ws();
            if self.eat2('=', '=') {
                value = i64::from(value == self.parse_relational()?);
            } else if self.eat2('!', '=') {
                value = i64::from(value != self.parse_relational()?);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_relational(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_shift()?;
        loop {
            self.skip_ws();
            if self.eat2('<', '=') {
                value = i64::from(value <= self.parse_shift()?);
            } else if self.eat2('>', '=') {
                value = i64::from(value >= self.parse_shift()?);
            } else if self.peek() == Some('<') && self.peek_at(1) != Some('<') {
                self.index += 1;
                value = i64::from(value < self.parse_shift()?);
            } else if self.peek() == Some('>') && self.peek_at(1) != Some('>') {
                self.index += 1;
                value = i64::from(value > self.parse_shift()?);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_shift(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_additive()?;
        loop {
            self.skip_ws();
            if self.eat2('<', '<') {
                let rhs = self.parse_additive()?;
                value = value.wrapping_shl(rhs as u32);
            } else if self.eat2('>', '>') {
                let rhs = self.parse_additive()?;
                value = value.wrapping_shr(rhs as u32);
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_additive(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.index += 1;
                    value = value.wrapping_add(self.parse_term()?);
                }
                Some('-') => {
                    self.index += 1;
                    value = value.wrapping_sub(self.parse_term()?);
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_term(&mut self) -> Result<i64, ShellError> {
        let mut value = self.parse_power()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('*') if self.peek_at(1) != Some('*') => {
                    self.index += 1;
                    value = value.wrapping_mul(self.parse_power()?);
                }
                Some('/') => {
                    self.index += 1;
                    let rhs = self.parse_power()?;
                    if rhs == 0 {
                        return Err(ShellError::execution("division by zero"));
                    }
                    value = value.wrapping_div(rhs);
                }
                Some('%') => {
                    self.index += 1;
                    let rhs = self.parse_power()?;
                    if rhs == 0 {
                        return Err(ShellError::execution("division by zero"));
                    }
                    value = value.wrapping_rem(rhs);
                }
                _ => return Ok(value),
            }
        }
    }

    fn parse_power(&mut self) -> Result<i64, ShellError> {
        self.guarded_arith(Self::parse_power_inner)
    }

    fn parse_power_inner(&mut self) -> Result<i64, ShellError> {
        let base = self.parse_factor()?;
        self.skip_ws();
        if self.eat2('*', '*') {
            let exponent = self.parse_power()?;
            if exponent < 0 {
                return Ok(0);
            }
            return Ok(base.wrapping_pow(exponent.min(u32::MAX as i64) as u32));
        }
        Ok(base)
    }

    fn parse_factor(&mut self) -> Result<i64, ShellError> {
        self.guarded_arith(Self::parse_factor_inner)
    }

    fn parse_factor_inner(&mut self) -> Result<i64, ShellError> {
        self.skip_ws();
        // Prefix increment/decrement: ++var / --var.
        if self.eat2('+', '+') {
            self.skip_ws();
            let name = self
                .try_read_identifier()
                .ok_or_else(|| ShellError::parse("arithmetic ++ requires a variable"))?;
            let value = self.read_var(&name).wrapping_add(1);
            self.write_var(&name, value);
            return Ok(value);
        }
        if self.eat2('-', '-') {
            self.skip_ws();
            let name = self
                .try_read_identifier()
                .ok_or_else(|| ShellError::parse("arithmetic -- requires a variable"))?;
            let value = self.read_var(&name).wrapping_sub(1);
            self.write_var(&name, value);
            return Ok(value);
        }
        match self.peek() {
            Some('+') => {
                self.index += 1;
                self.parse_factor()
            }
            Some('-') => {
                self.index += 1;
                Ok(self.parse_factor()?.wrapping_neg())
            }
            Some('!') => {
                self.index += 1;
                Ok(i64::from(self.parse_factor()? == 0))
            }
            Some('~') => {
                self.index += 1;
                Ok(!self.parse_factor()?)
            }
            Some('(') => {
                self.index += 1;
                let value = self.parse_expr()?;
                self.skip_ws();
                if self.peek() != Some(')') {
                    return Err(ShellError::parse("missing ')' in arithmetic expression"));
                }
                self.index += 1;
                Ok(value)
            }
            Some(ch) if ch.is_ascii_digit() => self.parse_number(),
            Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => self.parse_variable(),
            _ => Err(ShellError::parse("invalid arithmetic expression")),
        }
    }

    fn parse_number(&mut self) -> Result<i64, ShellError> {
        let start = self.index;
        // Hexadecimal: 0x.. / 0X..
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x') | Some('X')) {
            self.index += 2;
            let digits_start = self.index;
            while self.peek().is_some_and(|ch| ch.is_ascii_hexdigit()) {
                self.index += 1;
            }
            let digits = self.chars[digits_start..self.index]
                .iter()
                .collect::<String>();
            return i64::from_str_radix(&digits, 16)
                .map_err(|_| ShellError::parse("invalid arithmetic number"));
        }

        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.index += 1;
        }
        let text = self.chars[start..self.index].iter().collect::<String>();
        // Octal: a leading 0 with more digits (POSIX arithmetic).
        if text.len() > 1 && text.starts_with('0') {
            return i64::from_str_radix(&text, 8)
                .map_err(|_| ShellError::parse("invalid arithmetic number"));
        }
        text.parse::<i64>()
            .map_err(|_| ShellError::parse("invalid arithmetic number"))
    }

    fn parse_variable(&mut self) -> Result<i64, ShellError> {
        let start = self.index;
        self.index += 1;
        while self
            .peek()
            .is_some_and(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        {
            self.index += 1;
        }
        let name = self.chars[start..self.index].iter().collect::<String>();
        let current = self.read_var(&name);
        // Postfix increment/decrement: var++ / var-- (returns the old value).
        if self.peek() == Some('+') && self.peek_at(1) == Some('+') {
            self.index += 2;
            self.write_var(&name, current.wrapping_add(1));
            return Ok(current);
        }
        if self.peek() == Some('-') && self.peek_at(1) == Some('-') {
            self.index += 2;
            self.write_var(&name, current.wrapping_sub(1));
            return Ok(current);
        }
        Ok(current)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.index += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.index + offset).copied()
    }

    /// Consume a two-character operator if it is next, after skipping whitespace.
    fn eat2(&mut self, first: char, second: char) -> bool {
        if self.peek() == Some(first) && self.peek_at(1) == Some(second) {
            self.index += 2;
            true
        } else {
            false
        }
    }
}

fn apply_external_redirections(
    command: &mut Command,
    redirections: &[ExpandedRedirection],
    stdin_is_piped: &mut bool,
    merge_stderr_to_stdout: &mut bool,
    merge_stdout_to_stderr: &mut bool,
    capture_outputs: bool,
    noclobber: bool,
) -> Result<(), ShellError> {
    let mut stdout_file: Option<File> = None;
    let mut stderr_file: Option<File> = None;

    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            (RedirectionMode::Read, ExpandedRedirectionTarget::Path(path)) => {
                command.stdin(Stdio::from(File::open(path)?));
                *stdin_is_piped = false;
            }
            // Heredoc/herestring bytes are written to the child's piped stdin by
            // the caller (via stdin_data), so leave the piped stdin in place.
            (
                RedirectionMode::HereDoc | RedirectionMode::HereString,
                ExpandedRedirectionTarget::Bytes(_),
            ) => {}
            (RedirectionMode::Write, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, false)?;
                if redirection.fd == 1 {
                    stdout_file = Some(file.try_clone()?);
                    command.stdout(Stdio::from(file));
                } else if redirection.fd == 2 {
                    stderr_file = Some(file.try_clone()?);
                    command.stderr(Stdio::from(file));
                }
            }
            (RedirectionMode::WriteClobber, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, true)?;
                if redirection.fd == 1 {
                    stdout_file = Some(file.try_clone()?);
                    command.stdout(Stdio::from(file));
                } else if redirection.fd == 2 {
                    stderr_file = Some(file.try_clone()?);
                    command.stderr(Stdio::from(file));
                }
            }
            (RedirectionMode::Append, ExpandedRedirectionTarget::Path(path)) => {
                let file = OpenOptions::new().create(true).append(true).open(path)?;
                if redirection.fd == 1 {
                    stdout_file = Some(file.try_clone()?);
                    command.stdout(Stdio::from(file));
                } else if redirection.fd == 2 {
                    stderr_file = Some(file.try_clone()?);
                    command.stderr(Stdio::from(file));
                }
            }
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, noclobber, false)?;
                stdout_file = Some(file.try_clone()?);
                stderr_file = Some(file.try_clone()?);
                command.stderr(Stdio::from(file.try_clone()?));
                command.stdout(Stdio::from(file));
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {
                command.stdin(Stdio::null());
                *stdin_is_piped = false;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 1 => {
                stdout_file = None;
                *merge_stdout_to_stderr = false;
                command.stdout(Stdio::null());
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 2 => {
                stderr_file = None;
                *merge_stderr_to_stdout = false;
                command.stderr(Stdio::null());
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1)) if redirection.fd == 2 => {
                if let Some(file) = &stdout_file {
                    command.stderr(Stdio::from(file.try_clone()?));
                } else if capture_outputs {
                    *merge_stderr_to_stdout = true;
                    command.stderr(Stdio::piped());
                } else {
                    // `2>&1` with fd1 still at the process stdout: send stderr to
                    // a duplicate of the current stdout, so `2>&1 1>file` leaves
                    // stderr on the terminal's stdout (matches bash ordering).
                    command.stderr(Stdio::from(std::io::stdout().as_fd().try_clone_to_owned()?));
                }
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2)) if redirection.fd == 1 => {
                if let Some(file) = &stderr_file {
                    command.stdout(Stdio::from(file.try_clone()?));
                } else if capture_outputs {
                    *merge_stdout_to_stderr = true;
                    command.stdout(Stdio::piped());
                } else {
                    command.stdout(Stdio::from(std::io::stderr().as_fd().try_clone_to_owned()?));
                }
            }
            _ => {
                return Err(ShellError::unsupported(format!(
                    "unsupported redirection for fd {}",
                    redirection.fd
                )));
            }
        }
    }

    Ok(())
}

fn open_write_redirection(path: &str, noclobber: bool, force: bool) -> Result<File, ShellError> {
    if noclobber && !force {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(ShellError::from)
    } else {
        File::create(path).map_err(ShellError::from)
    }
}

/// Where a builtin's fd points after applying redirections, resolved in order.
enum BuiltinSink {
    Stdout,
    Stderr,
    Discard,
    File(File),
}

impl BuiltinSink {
    fn try_clone(&self) -> Result<BuiltinSink, ShellError> {
        Ok(match self {
            BuiltinSink::Stdout => BuiltinSink::Stdout,
            BuiltinSink::Stderr => BuiltinSink::Stderr,
            BuiltinSink::Discard => BuiltinSink::Discard,
            BuiltinSink::File(file) => BuiltinSink::File(file.try_clone()?),
        })
    }
}

fn apply_builtin_redirections(
    outcome: &mut CommandOutcome,
    redirections: &[ExpandedRedirection],
    state: &ShellState,
) -> Result<(), ShellError> {
    if redirections.is_empty() {
        return Ok(());
    }

    // Track the live destination of fd1 and fd2, mutating them in source order
    // so `>file 2>&1` and `2>&1 1>file` resolve with correct ordering semantics.
    let mut dest1 = BuiltinSink::Stdout;
    let mut dest2 = BuiltinSink::Stderr;

    for redirection in redirections {
        match (&redirection.mode, &redirection.target) {
            // stdin redirections (file, heredoc, herestring) are handled via the
            // builtin's buffered stdin, not the output sinks.
            (RedirectionMode::Read | RedirectionMode::HereDoc | RedirectionMode::HereString, _) => {
            }
            (
                RedirectionMode::Write | RedirectionMode::WriteClobber | RedirectionMode::Append,
                ExpandedRedirectionTarget::Path(path),
            ) => {
                let file = match redirection.mode {
                    RedirectionMode::Append => {
                        OpenOptions::new().create(true).append(true).open(path)?
                    }
                    RedirectionMode::WriteClobber => {
                        open_write_redirection(path, state.noclobber(), true)?
                    }
                    _ => open_write_redirection(path, state.noclobber(), false)?,
                };
                if redirection.fd == 1 {
                    dest1 = BuiltinSink::File(file);
                } else if redirection.fd == 2 {
                    dest2 = BuiltinSink::File(file);
                }
            }
            (RedirectionMode::WriteBoth, ExpandedRedirectionTarget::Path(path)) => {
                let file = open_write_redirection(path, state.noclobber(), false)?;
                dest2 = BuiltinSink::File(file.try_clone()?);
                dest1 = BuiltinSink::File(file);
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 1 => {
                dest1 = BuiltinSink::Discard;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 2 => {
                dest2 = BuiltinSink::Discard;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Close) if redirection.fd == 0 => {}
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(1)) if redirection.fd == 2 => {
                dest2 = dest1.try_clone()?;
            }
            (RedirectionMode::DupFd, ExpandedRedirectionTarget::Fd(2)) if redirection.fd == 1 => {
                dest1 = dest2.try_clone()?;
            }
            _ => {
                return Err(ShellError::unsupported(format!(
                    "unsupported redirection for fd {}",
                    redirection.fd
                )));
            }
        }
    }

    // Resolve buffers against final destinations. Take both buffers out first so
    // a swap (1>&2 with 2>&1) routes correctly.
    let stdout_bytes = std::mem::take(&mut outcome.stdout);
    let stderr_bytes = std::mem::take(&mut outcome.stderr);
    resolve_builtin_sink(dest1, stdout_bytes, outcome)?;
    resolve_builtin_sink(dest2, stderr_bytes, outcome)?;
    Ok(())
}

fn resolve_builtin_sink(
    sink: BuiltinSink,
    bytes: Vec<u8>,
    outcome: &mut CommandOutcome,
) -> Result<(), ShellError> {
    match sink {
        BuiltinSink::Stdout => outcome.stdout.extend_from_slice(&bytes),
        BuiltinSink::Stderr => outcome.stderr.extend_from_slice(&bytes),
        BuiltinSink::Discard => {}
        BuiltinSink::File(mut file) => file.write_all(&bytes)?,
    }
    Ok(())
}

fn has_unquoted_brace(segments: &[WordSegment]) -> bool {
    segments.iter().any(|segment| {
        segment.quote == QuoteKind::None
            && segment.text.contains('{')
            && segment.text.contains('}')
            // A comma (list `{a,b}`) or `..` (range `{1..5}`) makes it expandable.
            && (segment.text.contains(',') || segment.text.contains(".."))
    })
}

/// Caps that keep pathological brace input (`{1..1000000000}`, `{a,b}` repeated
/// dozens of times, `{{{{…}}}}`) from hanging or OOM-ing the shell. Both sit far
/// above any realistic use; input that would exceed them is left un-expanded
/// (literal) rather than crashing — never silently truncated to a wrong result.
const MAX_BRACE_ELEMENTS: usize = 1 << 20; // 1,048,576 generated words
const MAX_BRACE_DEPTH: usize = 64; // nested `{…}` levels

fn expand_braces(input: &str) -> Vec<String> {
    expand_braces_depth(input, 0)
}

fn expand_braces_depth(input: &str, depth: usize) -> Vec<String> {
    if depth > MAX_BRACE_DEPTH {
        return vec![input.to_string()];
    }
    let Some((open, close)) = find_brace_pair(input) else {
        return vec![input.to_string()];
    };
    let inner = &input[open + 1..close];
    let split = split_brace_alternatives(inner);
    let alternatives = if split.len() > 1 {
        split
    } else if let Some(range) = expand_brace_range(inner) {
        // `{1..5}`, `{5..1}`, `{0..10..2}`, `{a..e}` numeric/char sequences.
        range
    } else {
        return vec![input.to_string()];
    };

    let prefix = &input[..open];
    let suffix = &input[close + 1..];
    let suffix_expanded = expand_braces_depth(suffix, depth + 1);
    let mut out = Vec::new();
    for alternative in alternatives {
        // Each alternative may itself contain nested braces, e.g. {a,b{1,2}}.
        for expanded_alt in expand_braces_depth(&alternative, depth + 1) {
            for expanded_suffix in &suffix_expanded {
                if out.len() >= MAX_BRACE_ELEMENTS {
                    // Combinatorial blow-up: leave the original text literal.
                    return vec![input.to_string()];
                }
                out.push(format!("{prefix}{expanded_alt}{expanded_suffix}"));
            }
        }
    }
    out
}

fn find_brace_pair(input: &str) -> Option<(usize, usize)> {
    let mut depth = 0usize;
    let mut open = None;
    for (index, ch) in input.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    open = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return open.map(|open| (open, index));
                }
            }
            _ => {}
        }
    }
    None
}

/// Expand a brace range `a..b` or `a..b..step` into a sequence, or `None` if
/// `inner` is not a valid numeric or single-char range. Numeric ranges honor
/// leading-zero padding (`{01..10}`); char ranges cover single ASCII letters.
fn expand_brace_range(inner: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if parts.len() != 2 && parts.len() != 3 {
        return None;
    }
    let step_tok = parts.get(2).copied();

    // Numeric range.
    if let (Ok(start), Ok(end)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
        let step = match step_tok {
            None => 1,
            Some(s) => {
                let v = s.parse::<i64>().ok()?;
                // saturating_abs avoids a panic on i64::MIN.
                if v == 0 {
                    1
                } else {
                    v.saturating_abs()
                }
            }
        };
        // Bail out (leaving the brace literal) if the sequence would exceed the
        // element cap — computed without allocating, so a giant range like
        // `{1..1000000000}` can't OOM the shell.
        if start.abs_diff(end) / (step as u64) >= MAX_BRACE_ELEMENTS as u64 {
            return None;
        }
        let pad = brace_pad_width(parts[0], parts[1]);
        let mut out = Vec::new();
        let mut n = start;
        if start <= end {
            while n <= end {
                out.push(format_brace_number(n, pad));
                // checked_add: near i64::MAX this would otherwise overflow —
                // a panic in debug, an infinite loop via wraparound in release.
                match n.checked_add(step) {
                    Some(next) => n = next,
                    None => break,
                }
            }
        } else {
            while n >= end {
                out.push(format_brace_number(n, pad));
                match n.checked_sub(step) {
                    Some(next) => n = next,
                    None => break,
                }
            }
        }
        return Some(out);
    }

    // Single-character range (ASCII letters).
    let (sb, eb) = (parts[0].as_bytes(), parts[1].as_bytes());
    if sb.len() == 1 && eb.len() == 1 && sb[0].is_ascii_alphabetic() && eb[0].is_ascii_alphabetic()
    {
        let step: i32 = match step_tok {
            None => 1,
            Some(s) => {
                let v = s.parse::<i64>().ok()?;
                // Clamp the magnitude to a sane positive i32 (a char range spans
                // at most 26): avoids the `as i32` wrap that could make the step
                // negative and spin the loop forever.
                v.unsigned_abs().min(i32::MAX as u64).max(1) as i32
            }
        };
        let (s, e) = (sb[0] as i32, eb[0] as i32);
        let mut out = Vec::new();
        let mut c = s;
        if s <= e {
            while c <= e {
                out.push((c as u8 as char).to_string());
                match c.checked_add(step) {
                    Some(next) => c = next,
                    None => break,
                }
            }
        } else {
            while c >= e {
                out.push((c as u8 as char).to_string());
                match c.checked_sub(step) {
                    Some(next) => c = next,
                    None => break,
                }
            }
        }
        return Some(out);
    }
    None
}

/// Zero-pad width for a numeric brace range: nonzero only when an operand has an
/// explicit leading zero (e.g. `{01..10}` -> width 2).
fn brace_pad_width(a: &str, b: &str) -> usize {
    fn digits(s: &str) -> &str {
        s.trim_start_matches(['-', '+'])
    }
    fn leading_zero(s: &str) -> bool {
        let d = digits(s);
        d.len() > 1 && d.starts_with('0')
    }
    if leading_zero(a) || leading_zero(b) {
        digits(a).len().max(digits(b).len())
    } else {
        0
    }
}

fn format_brace_number(n: i64, pad: usize) -> String {
    if pad == 0 {
        n.to_string()
    } else if n < 0 {
        format!("-{:0>width$}", -n, width = pad)
    } else {
        format!("{:0>width$}", n, width = pad)
    }
}

fn split_brace_alternatives(input: &str) -> Vec<String> {
    let mut alternatives = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in input.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                alternatives.push(current);
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    alternatives.push(current);
    alternatives
}

#[derive(Debug, Clone)]
struct GlobPart {
    text: String,
    mask: Vec<bool>,
}

impl GlobPart {
    fn has_active_glob(&self) -> bool {
        self.text
            .chars()
            .zip(&self.mask)
            .any(|(ch, active)| *active && is_glob_metachar(ch))
    }

    fn starts_with_literal_dot(&self) -> bool {
        self.text.starts_with('.')
    }

    fn tokens(&self) -> Vec<GlobToken> {
        self.text
            .chars()
            .zip(&self.mask)
            .map(|(ch, active)| GlobToken {
                ch,
                active: *active,
            })
            .collect()
    }

    fn tokens_lowercased(&self) -> Vec<GlobToken> {
        self.text
            .chars()
            .zip(&self.mask)
            .map(|(ch, active)| GlobToken {
                ch: ch.to_ascii_lowercase(),
                active: *active,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy)]
struct GlobToken {
    ch: char,
    active: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct GlobOpts {
    globstar: bool,
    dotglob: bool,
    nocaseglob: bool,
}

fn expand_glob(pattern: &str, glob_mask: &[bool], cwd: &Path, opts: GlobOpts) -> Vec<String> {
    let mut matches = Vec::new();
    let is_absolute = pattern.starts_with('/');
    let parts = split_glob_parts(pattern, glob_mask);
    let real_base = if is_absolute {
        PathBuf::from("/")
    } else {
        cwd.to_path_buf()
    };
    let display_base = if is_absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    };

    expand_glob_parts(&real_base, &display_base, &parts, &mut matches, opts);
    matches.sort();
    matches
}

/// Recursively emit every file and directory under `real_base` (for a trailing
/// `**` globstar component). Hidden entries are skipped unless `dotglob`.
fn glob_recurse_all(
    real_base: &Path,
    display_base: &Path,
    matches: &mut Vec<String>,
    dotglob: bool,
) {
    let Ok(entries) = std::fs::read_dir(real_base) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') && !dotglob {
            continue;
        }
        let display = display_base.join(&name);
        matches.push(display.display().to_string());
        if entry.path().is_dir() {
            glob_recurse_all(&entry.path(), &display, matches, dotglob);
        }
    }
}

fn split_glob_parts(pattern: &str, glob_mask: &[bool]) -> Vec<GlobPart> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut mask = Vec::new();

    for (ch, active) in pattern.chars().zip(glob_mask) {
        if ch == '/' {
            parts.push(GlobPart {
                text: std::mem::take(&mut text),
                mask: std::mem::take(&mut mask),
            });
        } else {
            text.push(ch);
            mask.push(*active);
        }
    }

    parts.push(GlobPart { text, mask });
    parts
}

fn expand_glob_parts(
    real_base: &Path,
    display_base: &Path,
    parts: &[GlobPart],
    matches: &mut Vec<String>,
    opts: GlobOpts,
) {
    let Some((part, rest)) = parts.split_first() else {
        let display = display_base.display().to_string();
        if !display.is_empty() {
            matches.push(display);
        }
        return;
    };

    if part.text.is_empty() {
        expand_glob_parts(real_base, display_base, rest, matches, opts);
        return;
    }

    // Globstar `**`: matches zero or more path segments (shopt globstar).
    if opts.globstar && part.text == "**" && part.has_active_glob() {
        if rest.is_empty() {
            // `**` at the end: every file and dir recursively, plus the base.
            let display = display_base.display().to_string();
            if !display.is_empty() {
                matches.push(display);
            }
            glob_recurse_all(real_base, display_base, matches, opts.dotglob);
        } else {
            // Zero segments: match the rest here.
            expand_glob_parts(real_base, display_base, rest, matches, opts);
            // One or more segments: descend into each subdirectory, keeping `**`.
            if let Ok(entries) = std::fs::read_dir(real_base) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if (name.starts_with('.') && !opts.dotglob) || !entry.path().is_dir() {
                        continue;
                    }
                    expand_glob_parts(
                        &entry.path(),
                        &display_base.join(&name),
                        parts,
                        matches,
                        opts,
                    );
                }
            }
        }
        return;
    }

    if part.has_active_glob() {
        let Ok(entries) = std::fs::read_dir(real_base) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if glob_component_matches(part, &name, opts) {
                expand_glob_parts(
                    &entry.path(),
                    &display_base.join(&name),
                    rest,
                    matches,
                    opts,
                );
            }
        }
    } else {
        // A literal path component in a glob pattern must actually exist for the
        // overall pattern to match; otherwise the whole pattern stays literal.
        let candidate = real_base.join(&part.text);
        if candidate.symlink_metadata().is_ok() {
            expand_glob_parts(
                &candidate,
                &display_base.join(&part.text),
                rest,
                matches,
                opts,
            );
        }
    }
}

/// Whether `pattern` contains an extended-glob group opener (`?(`/`*(`/etc.).
fn pattern_has_extglob(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    bytes
        .windows(2)
        .any(|w| matches!(w[0], b'?' | b'*' | b'+' | b'@' | b'!') && w[1] == b'(')
}

fn glob_component_matches(pattern: &GlobPart, name: &str, opts: GlobOpts) -> bool {
    if name.starts_with('.') && !pattern.starts_with_literal_dot() && !opts.dotglob {
        return false;
    }
    // Extended-glob components (?(..) *(..) +(..) @(..) !(..)) use the byte
    // matcher, which understands extglob groups.
    if pattern_has_extglob(&pattern.text) {
        return glob_match_bytes(pattern.text.as_bytes(), name.as_bytes());
    }
    if opts.nocaseglob {
        let tokens = pattern.tokens_lowercased();
        let name_chars = name.to_lowercase().chars().collect::<Vec<_>>();
        return glob_match_tokens(&tokens, &name_chars);
    }
    let tokens = pattern.tokens();
    let name_chars = name.chars().collect::<Vec<_>>();
    glob_match_tokens(&tokens, &name_chars)
}

fn glob_match_tokens(pattern: &[GlobToken], name: &[char]) -> bool {
    match (pattern.split_first(), name.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (
            Some((
                GlobToken {
                    ch: '*',
                    active: true,
                },
                rest,
            )),
            _,
        ) => {
            glob_match_tokens(rest, name)
                || name
                    .split_first()
                    .is_some_and(|(_, name_rest)| glob_match_tokens(pattern, name_rest))
        }
        (
            Some((
                GlobToken {
                    ch: '?',
                    active: true,
                },
                rest,
            )),
            Some((_, name_rest)),
        ) => glob_match_tokens(rest, name_rest),
        (
            Some((
                GlobToken {
                    ch: '[',
                    active: true,
                },
                _,
            )),
            Some((name_ch, name_rest)),
        ) => {
            if let Some((matched, rest)) = match_char_class(pattern, *name_ch) {
                matched && glob_match_tokens(rest, name_rest)
            } else {
                pattern.first().is_some_and(|token| token.ch == *name_ch)
                    && glob_match_tokens(&pattern[1..], name_rest)
            }
        }
        (Some((pattern_ch, pattern_rest)), Some((name_ch, name_rest)))
            if pattern_ch.ch == *name_ch =>
        {
            glob_match_tokens(pattern_rest, name_rest)
        }
        _ => false,
    }
}

fn match_char_class(pattern: &[GlobToken], name_ch: char) -> Option<(bool, &[GlobToken])> {
    if !pattern
        .first()
        .is_some_and(|token| token.ch == '[' && token.active)
    {
        return None;
    }
    let mut index = 1;
    let negate = matches!(
        pattern.get(index),
        Some(GlobToken {
            ch: '!' | '^',
            active: true
        })
    );
    if negate {
        index += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while index < pattern.len() {
        if pattern[index].ch == ']' && pattern[index].active && saw_member {
            return Some((
                if negate { !matched } else { matched },
                &pattern[index + 1..],
            ));
        }

        // POSIX character class `[:name:]`.
        if pattern[index].ch == '[' && pattern.get(index + 1).is_some_and(|t| t.ch == ':') {
            let mut j = index + 2;
            let mut class = String::new();
            let mut close = None;
            while j + 1 < pattern.len() {
                if pattern[j].ch == ':' && pattern[j + 1].ch == ']' {
                    close = Some(j);
                    break;
                }
                class.push(pattern[j].ch);
                j += 1;
            }
            if let Some(close) = close {
                if name_ch.is_ascii() && char_class_matches(class.as_bytes(), name_ch as u8) {
                    matched = true;
                }
                index = close + 2; // past ":]"
                saw_member = true;
                continue;
            }
        }

        let start = pattern[index].ch;
        if pattern
            .get(index + 1)
            .is_some_and(|token| token.ch == '-' && token.active)
            && pattern.get(index + 2).is_some()
            && pattern[index + 2].ch != ']'
        {
            let end = pattern[index + 2].ch;
            let (low, high) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            if (low..=high).contains(&name_ch) {
                matched = true;
            }
            index += 3;
        } else {
            if start == name_ch {
                matched = true;
            }
            index += 1;
        }
        saw_member = true;
    }

    None
}

pub fn print_captured_if_needed(
    outcome: &CommandOutcome,
    _options: &ExecutionOptions,
) -> Result<(), ShellError> {
    if let Some(observation) = &outcome.observation {
        if !observation.display.is_empty() {
            print!("{}", observation.display);
            pipe_ok(std::io::stdout().flush())?;
        }
    } else {
        pipe_ok(std::io::stdout().write_all(&outcome.stdout))?;
        pipe_ok(std::io::stderr().write_all(&outcome.stderr))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_core::parse_line;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn read_capped_returns_small_input_exactly() {
        let out = read_capped(std::io::Cursor::new(b"hello world".to_vec())).unwrap();
        assert_eq!(out, b"hello world");
    }

    #[test]
    fn read_capped_bounds_large_input_keeping_head_and_tail() {
        // SHIP_READINESS_PLAN P0-9: capture is bounded, keeping head + tail with a
        // marker rather than the whole (potentially huge) stream.
        let total = CAPTURE_HEAD + CAPTURE_TAIL + 3 * 1024 * 1024;
        let mut data = vec![b'x'; total];
        data[..5].copy_from_slice(b"START");
        let n = data.len();
        data[n - 3..].copy_from_slice(b"END");
        let out = read_capped(std::io::Cursor::new(data)).unwrap();
        assert!(out.len() < total, "not truncated: {} >= {total}", out.len());
        assert!(
            out.len() <= CAPTURE_HEAD + CAPTURE_TAIL + 512,
            "over cap: {}",
            out.len()
        );
        assert!(out.starts_with(b"START"), "head lost");
        assert!(out.ends_with(b"END"), "tail lost");
        assert!(
            String::from_utf8_lossy(&out).contains("bytes of output elided"),
            "missing truncation marker"
        );
    }

    #[test]
    fn brace_range_expansion() {
        assert_eq!(
            expand_brace_range("1..5").unwrap(),
            ["1", "2", "3", "4", "5"]
        );
        assert_eq!(
            expand_brace_range("5..1").unwrap(),
            ["5", "4", "3", "2", "1"]
        );
        assert_eq!(
            expand_brace_range("0..10..2").unwrap(),
            ["0", "2", "4", "6", "8", "10"]
        );
        assert_eq!(
            expand_brace_range("01..10").unwrap(),
            ["01", "02", "03", "04", "05", "06", "07", "08", "09", "10"]
        );
        assert_eq!(
            expand_brace_range("a..e").unwrap(),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(expand_brace_range("a..e..2").unwrap(), ["a", "c", "e"]);
        assert_eq!(
            expand_brace_range("-2..2").unwrap(),
            ["-2", "-1", "0", "1", "2"]
        );
        assert_eq!(expand_brace_range("5..5").unwrap(), ["5"]);
        assert!(expand_brace_range("a").is_none());
        assert!(expand_brace_range("not..a..b..range").is_none());
    }

    #[test]
    fn assignment_without_command_sets_shell_var() {
        let graph = parse_line("FOO=bar").unwrap();
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        executor
            .run_graph(&graph, &mut state, &ExecutionOptions::default())
            .unwrap();
        assert_eq!(state.lookup("FOO"), Some("bar"));
    }

    #[test]
    fn allexport_exports_subsequent_shell_assignments() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_ALL_EXPORT");
        state.unset("AGSH_STILL_LOCAL");
        let mut executor = Executor::new();
        let capture_options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            ..ExecutionOptions::default()
        };

        let local = executor
            .run_graph(
                &parse_line(
                    r#"AGSH_STILL_LOCAL=one; sh -c 'printf %s "${AGSH_STILL_LOCAL-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&local.stdout), "unset");
        assert_eq!(state.lookup("AGSH_STILL_LOCAL"), Some("one"));

        executor
            .run_graph(
                &parse_line("set -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.allexport());

        let exported = executor
            .run_graph(
                &parse_line(r#"AGSH_ALL_EXPORT=two; sh -c 'printf %s "$AGSH_ALL_EXPORT"'"#)
                    .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&exported.stdout), "two");

        executor
            .run_graph(
                &parse_line("set +a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.allexport());

        let still_local = executor
            .run_graph(
                &parse_line(
                    r#"AGSH_STILL_LOCAL=three; sh -c 'printf %s "${AGSH_STILL_LOCAL-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&still_local.stdout), "unset");
        assert_eq!(state.lookup("AGSH_STILL_LOCAL"), Some("three"));
    }

    #[test]
    fn allexport_does_not_persist_temporary_command_assignments() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_TEMP_EXPORT");
        let mut executor = Executor::new();
        let capture_options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            ..ExecutionOptions::default()
        };

        executor
            .run_graph(
                &parse_line("set -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let temporary = executor
            .run_graph(
                &parse_line(
                    r#"AGSH_TEMP_EXPORT=tmp sh -c 'printf %s "$AGSH_TEMP_EXPORT"'; sh -c 'printf %s "${AGSH_TEMP_EXPORT-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&temporary.stdout), "tmpunset");
        assert_eq!(state.lookup("AGSH_TEMP_EXPORT"), None);
    }

    #[test]
    fn command_v_reports_builtin_and_external() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let builtin = executor
            .run_graph(
                &parse_line("command -v echo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&builtin.stdout), "echo\n");

        let external = executor
            .run_graph(
                &parse_line("command -v sh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&external.stdout).contains("/sh"));
    }

    #[test]
    fn command_v_verbose_reports_resolution_details() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let alias = executor
            .run_graph(
                &parse_line("command -V hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&alias.stdout),
            "hi is aliased to echo hello\n"
        );

        let builtin = executor
            .run_graph(
                &parse_line("command -V echo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&builtin.stdout),
            "echo is an agsh builtin\n"
        );
    }

    #[test]
    fn command_double_dash_executes_wrapper_and_bad_options_fail() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let wrapped = executor
            .run_graph(
                &parse_line("command -- echo ok").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&wrapped.stdout), "ok\n");

        let err = executor
            .run_graph(
                &parse_line("command -Z echo").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();
        assert!(err.message.contains("unsupported option"));
    }

    #[test]
    fn command_p_uses_default_path_for_execution_and_introspection() {
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "");
        let mut executor = Executor::new();

        let executed = executor
            .run_graph(
                &parse_line("command -p sh -c 'printf ok'").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(executed.stdout, b"ok");

        let described = executor
            .run_graph(
                &parse_line("command -p -v sh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&described.stdout).contains("/sh\n"));

        let verbose = executor
            .run_graph(
                &parse_line("command -p -V sh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&verbose.stdout).contains("sh is "));
    }

    #[test]
    fn wrappers_force_external_and_builtin_lookup() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let external = executor
            .run_graph(
                &parse_line("external echo ok").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&external.stdout), "ok\n");

        let builtin = executor
            .run_graph(
                &parse_line("builtin pwd").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(String::from_utf8_lossy(&builtin.stdout).contains('/'));
    }

    #[test]
    fn job_control_builtins_exist_without_jobs() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let jobs = executor
            .run_graph(
                &parse_line("jobs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(jobs.exit_code, 0);
        assert!(jobs.stdout.is_empty());
        assert!(jobs.stderr.is_empty());

        let wait_empty = executor
            .run_graph(
                &parse_line("wait").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(wait_empty.exit_code, 0);

        let fg = executor
            .run_graph(
                &parse_line("fg").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(fg.exit_code, 1);
        assert!(String::from_utf8_lossy(&fg.stderr).contains("no such job"));

        let bg = executor
            .run_graph(
                &parse_line("bg %1").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bg.exit_code, 1);
        assert!(String::from_utf8_lossy(&bg.stderr).contains("no such job"));

        let wait_job = executor
            .run_graph(
                &parse_line("wait 12345").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(wait_job.exit_code, 127);
        assert!(String::from_utf8_lossy(&wait_job.stderr).contains("no such job"));
    }

    #[test]
    fn job_control_builtins_report_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v jobs fg bg wait").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "jobs\nfg\nbg\nwait\n"
        );

        let type_out = executor
            .run_graph(
                &parse_line("type jobs fg bg wait").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "jobs is an agsh builtin\nfg is an agsh builtin\nbg is an agsh builtin\nwait is an agsh builtin\n"
        );

        let builtin_jobs = executor
            .run_graph(
                &parse_line("builtin jobs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_jobs.exit_code, 0);
    }

    #[test]
    fn loop_control_builtins_report_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v break continue").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "break\ncontinue\n"
        );

        let type_out = executor
            .run_graph(
                &parse_line("type break continue").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "break is an agsh builtin\ncontinue is an agsh builtin\n"
        );
    }

    #[test]
    fn kill_builtin_delegates_to_platform_kill_and_reserves_job_specs() {
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "");
        let mut executor = Executor::new();

        let listed = executor
            .run_graph(
                &parse_line("kill -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(listed.exit_code, 0);
        let signal_list = format!(
            "{}{}",
            String::from_utf8_lossy(&listed.stdout),
            String::from_utf8_lossy(&listed.stderr)
        );
        assert!(signal_list.contains("TERM"));

        let job_spec = executor
            .run_graph(
                &parse_line("kill %1").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(job_spec.exit_code, 1);
        assert!(String::from_utf8_lossy(&job_spec.stderr).contains("no such job"));
    }

    #[test]
    fn kill_builtin_reports_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v kill").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&command_v.stdout), "kill\n");

        let type_out = executor
            .run_graph(
                &parse_line("type kill").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "kill is an agsh builtin\n"
        );

        let builtin_out = executor
            .run_graph(
                &parse_line("builtin kill -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_out.exit_code, 0);
    }

    #[test]
    fn exec_without_command_applies_assignments_without_replacing_process() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("FOO=bar exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FOO"), Some("bar"));
    }

    #[test]
    fn exec_command_is_disabled_without_process_replacement_permission() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("exec sh -c 'printf should-not-run'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 126);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("disabled"));
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn exec_builtin_reports_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&command_v.stdout), "exec\n");

        let type_out = executor
            .run_graph(
                &parse_line("type exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "exec is an agsh builtin\n"
        );

        let builtin_out = executor
            .run_graph(
                &parse_line("builtin exec").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_out.exit_code, 0);
    }

    #[test]
    fn ulimit_and_umask_support_safe_queries() {
        let mut state = ShellState::from_current_process();
        state.export_var("PATH", "");
        let mut executor = Executor::new();

        let ulimit = executor
            .run_graph(
                &parse_line("ulimit").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ulimit.exit_code, 0);
        assert!(!ulimit.stdout.is_empty());

        let ulimit_all = executor
            .run_graph(
                &parse_line("ulimit -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ulimit_all.exit_code, 0);
        assert!(
            String::from_utf8_lossy(&ulimit_all.stdout).contains("open files")
                || String::from_utf8_lossy(&ulimit_all.stdout).contains("file")
        );

        let umask = executor
            .run_graph(
                &parse_line("umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(umask.exit_code, 0);
        let mask = String::from_utf8_lossy(&umask.stdout);
        assert!(mask.trim().chars().all(|ch| ('0'..='7').contains(&ch)));

        let umask_symbolic = executor
            .run_graph(
                &parse_line("umask -S").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(umask_symbolic.exit_code, 0);
        assert!(String::from_utf8_lossy(&umask_symbolic.stdout).contains("u="));
    }

    #[test]
    fn ulimit_and_umask_reject_mutating_forms_without_safe_backend() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let ulimit_set = executor
            .run_graph(
                &parse_line("ulimit 1024").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ulimit_set.exit_code, 2);
        assert!(String::from_utf8_lossy(&ulimit_set.stderr).contains("unsupported"));

        let umask_set = executor
            .run_graph(
                &parse_line("umask 077").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(umask_set.exit_code, 2);
        assert!(String::from_utf8_lossy(&umask_set.stderr).contains("unsupported"));
    }

    #[test]
    fn ulimit_and_umask_report_resolution_consistently() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let command_v = executor
            .run_graph(
                &parse_line("command -v ulimit umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "ulimit\numask\n"
        );

        let type_out = executor
            .run_graph(
                &parse_line("type ulimit umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "ulimit is an agsh builtin\numask is an agsh builtin\n"
        );

        let builtin_umask = executor
            .run_graph(
                &parse_line("builtin umask").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(builtin_umask.exit_code, 0);
    }

    #[test]
    fn set_lists_shell_variables() {
        let mut state = ShellState::from_current_process();
        state.set_var("AGSH_SET_TEST", "value with space");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(
            String::from_utf8_lossy(&outcome.stdout).contains("AGSH_SET_TEST='value with space'\n")
        );
    }

    #[test]
    fn set_replaces_and_clears_positionals() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let outcome = executor
            .run_graph(
                &parse_line("echo \"$1/$2/$@\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "one/two/one two\n"
        );

        executor
            .run_graph(
                &parse_line("set --").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let cleared = executor
            .run_graph(
                &parse_line("echo \"${1:-empty}/${@:-empty}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&cleared.stdout), "empty/empty\n");
    }

    #[test]
    fn set_rejects_unsupported_options() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set -z").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("unsupported option"));
    }

    #[test]
    fn set_allexport_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.allexport());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\ton\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +a").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.allexport());

        executor
            .run_graph(
                &parse_line("set -o allexport").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.allexport());

        executor
            .run_graph(
                &parse_line("set +o allexport").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.allexport());
    }

    #[test]
    fn set_errexit_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -e").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.errexit());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\ton\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +e").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.errexit());

        executor
            .run_graph(
                &parse_line("set -o errexit").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.errexit());

        executor
            .run_graph(
                &parse_line("set +o errexit").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.errexit());
    }

    #[test]
    fn set_nounset_controls_unset_parameter_expansion() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_NOUNSET_MISSING");
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -u").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.nounset());

        let err = executor
            .run_graph(
                &parse_line("echo $AGSH_NOUNSET_MISSING").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();
        assert!(err
            .message
            .contains("AGSH_NOUNSET_MISSING: parameter not set"));

        let defaulted = executor
            .run_graph(
                &parse_line("echo ${AGSH_NOUNSET_MISSING:-fallback}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&defaulted.stdout), "fallback\n");

        executor
            .run_graph(
                &parse_line("set +u").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.nounset());

        let disabled = executor
            .run_graph(
                &parse_line("echo $AGSH_NOUNSET_MISSING").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&disabled.stdout), "\n");
    }

    #[test]
    fn set_o_nounset_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let initial = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&initial.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set -o nounset").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.nounset());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\ton\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +o nounset").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.nounset());
    }

    #[test]
    fn set_o_pipefail_controls_pipeline_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.pipefail());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\ton\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.pipefail());
    }

    #[test]
    fn set_noclobber_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -C").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.noclobber());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\ton\nnoglob\toff\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +C").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noclobber());

        executor
            .run_graph(
                &parse_line("set -o noclobber").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.noclobber());

        executor
            .run_graph(
                &parse_line("set +o noclobber").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noclobber());
    }

    #[test]
    fn set_noglob_controls_option_and_reports_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let enabled = executor
            .run_graph(
                &parse_line("set -f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(enabled.exit_code, 0);
        assert!(state.noglob());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\ton\npipefail\toff\nxtrace\toff\n"
        );

        executor
            .run_graph(
                &parse_line("set +f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noglob());

        executor
            .run_graph(
                &parse_line("set -o noglob").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.noglob());

        executor
            .run_graph(
                &parse_line("set +o noglob").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noglob());
    }

    #[test]
    fn set_xtrace_traces_expanded_commands_to_stderr() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -x").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.xtrace());

        let traced = executor
            .run_graph(
                &parse_line(r#"echo "hello world""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&traced.stdout), "hello world\n");
        assert_eq!(
            String::from_utf8_lossy(&traced.stderr),
            "+ echo 'hello world'\n"
        );

        let disabled = executor
            .run_graph(
                &parse_line("set +x").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&disabled.stderr), "+ set +x\n");
        assert!(!state.xtrace());

        let quiet = executor
            .run_graph(
                &parse_line("echo quiet").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(quiet.stderr.is_empty());
    }

    #[test]
    fn set_o_xtrace_controls_tracing_option() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -o xtrace").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.xtrace());

        let listed = executor
            .run_graph(
                &parse_line("set -o").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout),
            "allexport\toff\nerrexit\toff\nnounset\toff\nnoclobber\toff\nnoglob\toff\npipefail\toff\nxtrace\ton\n"
        );
        assert_eq!(String::from_utf8_lossy(&listed.stderr), "+ set -o\n");

        executor
            .run_graph(
                &parse_line("set +o xtrace").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.xtrace());
    }

    #[test]
    fn nounset_errors_on_braced_length_of_unset_parameter() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_NOUNSET_LENGTH_MISSING");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -u").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let err = executor
            .run_graph(
                &parse_line("echo ${#AGSH_NOUNSET_LENGTH_MISSING}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();

        assert!(err
            .message
            .contains("AGSH_NOUNSET_LENGTH_MISSING: parameter not set"));
    }

    #[test]
    fn read_assigns_fields_from_pipeline_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one two three\n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("one"));
        assert_eq!(state.lookup("REST"), Some("two three"));
    }

    #[test]
    fn final_pipeline_read_stage_can_update_parent_state() {
        let mut state = ShellState::from_current_process();
        state.unset("PIPE_READ_VALUE");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'value\n' | read PIPE_READ_VALUE").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("PIPE_READ_VALUE"), Some("value"));
    }

    #[test]
    fn final_pipeline_while_read_stage_consumes_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | while read line; do echo \"<$line>\"; done")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<one>\n<two>\n");
    }

    #[test]
    fn final_pipeline_while_read_stage_uses_input_redirection() {
        let temp = unique_temp_dir("while-input-redir");
        std::fs::write(temp.join("in.txt"), "file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf 'pipe\n' | while read line; do echo "$line"; done < in.txt"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn non_final_compound_pipeline_stage_feeds_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf 'one\ntwo\n' | while read line; do echo \"$line\"; done | wc -l",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn non_final_compound_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"yes x | while read line; do echo "$line"; break; done | wc -l"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_final_for_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes ignored | for item in one two three; do echo "$item"; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "3");
    }

    #[test]
    fn buffered_pipeline_preflights_missing_command_before_starting_producer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        // A missing command in a pipeline stage yields exit 127 (and an error on
        // stderr) without aborting the command list, matching POSIX shells.
        let outcome = executor
            .run_graph(
                &parse_line("yes x | AGSH_MISSING_PIPE_COMMAND | wc -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 127);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("AGSH_MISSING_PIPE_COMMAND"));
    }

    #[test]
    fn final_pipeline_select_stage_consumes_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '2\n' | select item in one two; do echo \"$REPLY:$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "2:two\n");
        assert_eq!(state.lookup("REPLY"), Some("2"));
        assert_eq!(state.lookup("item"), Some("two"));
    }

    #[test]
    fn external_prefix_streams_into_final_compound_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"yes x | while read line; do echo "$line"; break; done"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "x\n");
    }

    #[test]
    fn external_prefix_streams_into_final_function_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"reader() { read line; echo "$line"; }; yes y | reader"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "y\n");
    }

    #[test]
    fn non_final_select_pipeline_stage_feeds_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '1\n' | select item in one two; do echo \"$item\"; break; done | wc -l",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_final_select_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes 1 | select item in one two; do echo "$item"; break; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn adjacent_non_final_shell_stages_stream_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes x | while read line; do echo "$line"; break; done | while read line; do echo "$line"; break; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn first_stage_compound_producer_streams_to_external_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item in one two; do echo "$item"; done | wc -l"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn first_stage_function_producer_streams_to_external_consumer() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"producer() { echo one; echo two; }; producer | wc -l"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn first_stage_shell_producer_streams_through_later_shell_stage() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"for item in one; do echo "$item"; done | cat | while read line; do echo "$line"; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_contiguous_shell_stages_stream_through_external_middle_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes x | while read line; do echo "$line"; break; done | cat | while read line; do echo "$line"; break; done | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn non_contiguous_function_stages_stream_through_external_middle_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"reader() { read line; echo "$line"; }; yes y | reader | cat | reader | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[cfg(unix)]
    #[test]
    fn non_final_shell_stage_with_output_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-shell-redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes x | while read line; do echo "$line"; break; done > out.txt | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "0");
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "x\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_final_shell_stage_with_input_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-shell-input-redir");
        std::fs::write(temp.join("in.txt"), "file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"yes ignored | while read line; do echo "$line"; break; done < in.txt | cat"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_final_function_stage_with_input_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-function-input-redir");
        std::fs::write(temp.join("in.txt"), "file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"reader() { read line; echo "$line"; }; yes ignored | reader < in.txt | cat"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_final_function_stage_with_output_redirection_streams_to_next_command() {
        let temp = unique_temp_dir("stream-function-redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"reader() { read line; echo "$line"; }; yes y | reader > out.txt | wc -l"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "0");
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "y\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn non_final_pipeline_stages_do_not_leak_shell_state() {
        let mut state = ShellState::from_current_process();
        state.unset("AGSH_PIPE_LEAK");
        let mut executor = Executor::new();
        let capture_options = ExecutionOptions {
            output_mode: OutputMode::Clean,
            ..ExecutionOptions::default()
        };

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"export AGSH_PIPE_LEAK=bad | sh -c 'printf %s "${AGSH_PIPE_LEAK-unset}"'"#,
                )
                .unwrap(),
                &mut state,
                &capture_options,
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "unset");
        assert_eq!(state.lookup("AGSH_PIPE_LEAK"), None);
    }

    #[test]
    fn read_uses_shell_ifs_for_field_splitting() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", ":");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'a:b:c\n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("a"));
        assert_eq!(state.lookup("REST"), Some("b:c"));
    }

    #[test]
    fn read_uses_input_redirection() {
        let temp = unique_temp_dir("read-input-redir");
        std::fs::write(temp.join("in.txt"), "from-file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("read LINE < in.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("LINE"), Some("from-file"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn function_uses_input_redirection() {
        let temp = unique_temp_dir("function-input-redir");
        std::fs::write(temp.join("in.txt"), "from-file\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"reader() { read line; echo "$line"; }; reader < in.txt"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "from-file\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn read_preserves_interior_empty_ifs_fields() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", ":");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'a::b:\n' | read FIRST SECOND THIRD FOURTH").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("a"));
        assert_eq!(state.lookup("SECOND"), Some(""));
        assert_eq!(state.lookup("THIRD"), Some("b"));
        assert_eq!(state.lookup("FOURTH"), Some(""));
    }

    #[test]
    fn read_trims_ifs_whitespace_around_non_whitespace_delimiters() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " :");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf ' a : b : \n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("a"));
        assert_eq!(state.lookup("REST"), Some("b"));
    }

    #[test]
    fn read_uses_reply_by_default_and_reports_eof() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let reply = executor
            .run_graph(
                &parse_line("printf 'hello\n' | read").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(reply.exit_code, 0);
        assert_eq!(state.lookup("REPLY"), Some("hello"));

        let eof = executor
            .run_graph(
                &parse_line("printf '' | read EMPTY").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(eof.exit_code, 1);
        assert_eq!(state.lookup("EMPTY"), None);
    }

    #[test]
    fn read_supports_raw_mode_and_rejects_bad_names() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let raw = executor
            .run_graph(
                &parse_line("printf 'a\\\\b\n' | read -r RAW").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(raw.exit_code, 0);
        assert_eq!(state.lookup("RAW"), Some("a\\b"));

        let invalid = executor
            .run_graph(
                &parse_line("printf 'x\n' | read 1BAD").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(invalid.exit_code, 2);
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("invalid identifier"));
    }

    #[test]
    fn read_supports_prompt_option_for_non_terminal_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let separated = executor
            .run_graph(
                &parse_line("printf 'value\n' | read -p 'Name: ' NAME").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(separated.exit_code, 0);
        assert!(separated.stderr.is_empty());
        assert_eq!(state.lookup("NAME"), Some("value"));

        let inline = executor
            .run_graph(
                &parse_line("printf 'other\n' | read -pPrompt: OTHER").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(inline.exit_code, 0);
        assert!(inline.stderr.is_empty());
        assert_eq!(state.lookup("OTHER"), Some("other"));
    }

    #[test]
    fn read_prompt_option_requires_prompt_argument() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'value\n' | read -p").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("requires a prompt"));
    }

    #[test]
    fn read_joins_backslash_continuations_by_default() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\\\\\ntwo three\n' | read FIRST REST").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("FIRST"), Some("onetwo"));
        assert_eq!(state.lookup("REST"), Some("three"));
    }

    #[test]
    fn read_raw_mode_does_not_join_backslash_continuations() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\\\\\ntwo\n' | read -r RAW").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(state.lookup("RAW"), Some("one\\"));
    }

    #[test]
    fn history_lists_limits_and_clears_entries() {
        let mut state = ShellState::from_current_process();
        state.record_history("echo one");
        state.record_history("echo two");
        state.record_history("echo three");
        let mut executor = Executor::new();

        let all = executor
            .run_graph(
                &parse_line("history").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let all_text = String::from_utf8_lossy(&all.stdout);
        assert!(all_text.contains("    1  echo one\n"));
        assert!(all_text.contains("    3  echo three\n"));

        let tail = executor
            .run_graph(
                &parse_line("history 2").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let tail_text = String::from_utf8_lossy(&tail.stdout);
        assert!(!tail_text.contains("echo one"));
        assert!(tail_text.contains("    2  echo two\n"));
        assert!(tail_text.contains("    3  echo three\n"));

        let cleared = executor
            .run_graph(
                &parse_line("history -c").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(cleared.exit_code, 0);
        assert_eq!(state.history_len(), 0);
    }

    #[test]
    fn history_rejects_invalid_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("history nope").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("invalid count"));
    }

    #[test]
    fn test_builtin_evaluates_strings_numbers_and_brackets() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let non_empty = executor
            .run_graph(
                &parse_line("test -n value").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(non_empty.exit_code, 0);

        let empty = executor
            .run_graph(
                &parse_line("test -z ''").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(empty.exit_code, 0);

        let numeric = executor
            .run_graph(
                &parse_line("[ 4 -gt 2 ]").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(numeric.exit_code, 0);

        let mismatch = executor
            .run_graph(
                &parse_line("[ left = right ]").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(mismatch.exit_code, 1);
    }

    #[test]
    fn test_builtin_uses_shell_cwd_for_file_checks() {
        let temp = unique_temp_dir("test-builtin");
        std::fs::write(temp.join("file.txt"), "content").unwrap();
        std::fs::create_dir(temp.join("dir")).unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let file = executor
            .run_graph(
                &parse_line("test -f file.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(file.exit_code, 0);

        let dir = executor
            .run_graph(
                &parse_line("test -d dir").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(dir.exit_code, 0);

        let missing = executor
            .run_graph(
                &parse_line("test -e missing.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(missing.exit_code, 1);

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn eval_executes_joined_arguments_in_current_shell() {
        let mut state = ShellState::from_current_process();
        state.set_var("WORD", "expanded");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line("eval echo '$WORD'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(expanded.stdout, b"expanded\n");

        let assigned = executor
            .run_graph(
                &parse_line("eval FOO=from_eval").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(assigned.exit_code, 0);
        assert_eq!(state.lookup("FOO"), Some("from_eval"));
    }

    #[test]
    fn eval_pipeline_consumer_reads_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | eval 'read A; read B; echo \"$A/$B\"'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one/two\n");
    }

    #[test]
    fn source_executes_file_in_current_shell() {
        let temp = unique_temp_dir("source");
        std::fs::write(
            temp.join("script.agsh"),
            "FOO=from_source\nalias hi='echo sourced'\nhi\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"sourced\n");
        assert_eq!(state.lookup("FOO"), Some("from_source"));
        assert_eq!(state.alias("hi"), Some("echo sourced"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_pipeline_consumer_reads_buffered_stdin() {
        let temp = unique_temp_dir("source-pipeline-stdin");
        std::fs::write(temp.join("script.agsh"), "read A\nread B\necho \"$A/$B\"\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one/two\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_joins_backslash_continued_lines() {
        let temp = unique_temp_dir("source-continuation");
        std::fs::write(
            temp.join("script.agsh"),
            "VALUE=one\\\ntwo\nprintf '%s %s\\n' \"$VALUE\" \\\nthree\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"onetwo three\n");
        assert_eq!(state.lookup("VALUE"), Some("onetwo"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_function_definitions() {
        let temp = unique_temp_dir("source-multiline-function");
        std::fs::write(
            temp.join("script.agsh"),
            "hi() {\n  echo one\n  false && echo no || echo two\n}\nhi\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
        assert!(state.function("hi").is_some());

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_if_blocks() {
        let temp = unique_temp_dir("source-multiline-if");
        std::fs::write(
            temp.join("script.agsh"),
            "if false\nthen\n  echo no\nelif true\nthen\n  FOO=from_elif\nelse\n  FOO=from_else\nfi\necho \"$FOO\"\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "from_elif\n");
        assert_eq!(state.lookup("FOO"), Some("from_elif"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_while_blocks() {
        let temp = unique_temp_dir("source-multiline-while");
        std::fs::write(
            temp.join("script.agsh"),
            "COUNT=0\nwhile [ \"$COUNT\" -lt 2 ]\ndo\n  echo \"$COUNT\"\n  COUNT=$(expr \"$COUNT\" + 1)\ndone\necho done:$COUNT\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\ndone:2\n");
        assert_eq!(state.lookup("COUNT"), Some("2"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_until_blocks() {
        let temp = unique_temp_dir("source-multiline-until");
        std::fs::write(
            temp.join("script.agsh"),
            "COUNT=0\nuntil [ \"$COUNT\" -ge 2 ]\ndo\n  echo \"$COUNT\"\n  COUNT=$(expr \"$COUNT\" + 1)\ndone\necho done:$COUNT\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\ndone:2\n");
        assert_eq!(state.lookup("COUNT"), Some("2"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_for_blocks() {
        let temp = unique_temp_dir("source-multiline-for");
        std::fs::write(
            temp.join("script.agsh"),
            "for item in one \"two words\"\ndo\n  printf '<%s>\\n' \"$item\"\ndone\necho last:$item\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>\n<two words>\nlast:two words\n"
        );
        assert_eq!(state.lookup("item"), Some("two words"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_select_blocks() {
        let temp = unique_temp_dir("source-multiline-select");
        std::fs::write(
            temp.join("script.agsh"),
            "select item in one \"two words\"\ndo\n  printf '<%s:%s>\\n' \"$REPLY\" \"$item\"\n  break\ndone\necho last:$item\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("printf '2\n' | source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<2:two words>\nlast:two words\n"
        );
        assert_eq!(state.lookup("REPLY"), Some("2"));
        assert_eq!(state.lookup("item"), Some("two words"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_multiline_case_blocks() {
        let temp = unique_temp_dir("source-multiline-case");
        std::fs::write(
            temp.join("script.agsh"),
            "KIND=rs\ncase \"$KIND\" in\n  rust|rs)\n    echo rust\n    ;;\n  *)\n    echo other\n    ;;\nesac\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "rust\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_supports_nested_multiline_case_blocks() {
        let temp = unique_temp_dir("source-nested-multiline-case");
        std::fs::write(
            temp.join("script.agsh"),
            "case outer in\n  outer)\n    case inner in\n      inner)\n        echo nested\n        ;;\n    esac\n    echo after\n    ;;\nesac\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "nested\nafter\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_errexit_stops_after_failed_command() {
        let temp = unique_temp_dir("source-errexit");
        std::fs::write(temp.join("script.agsh"), "set -e\nfalse\nFOO=after\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert!(state.errexit());
        assert_ne!(state.lookup("FOO"), Some("after"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn source_plus_errexit_continues_after_failed_command() {
        let temp = unique_temp_dir("source-plus-errexit");
        std::fs::write(
            temp.join("script.agsh"),
            "set -e\nset +e\nfalse\nFOO=after\n",
        )
        .unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("source script.agsh").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(!state.errexit());
        assert_eq!(state.lookup("FOO"), Some("after"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn dot_source_sets_positional_arguments_temporarily() {
        let temp = unique_temp_dir("dot-source");
        std::fs::write(temp.join("script.agsh"), "echo \"$1\"\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("1", "outer");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(". script.agsh inner").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"inner\n");
        assert_eq!(state.lookup("1"), Some("outer"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn bracket_test_reports_missing_closer_as_syntax_error() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("[ value").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 2);
        assert!(String::from_utf8_lossy(&outcome.stderr).contains("missing ]"));
    }

    #[cfg(unix)]
    #[test]
    fn path_cache_invalidates_when_path_changes() {
        let temp = unique_temp_dir("path-cache");
        let bin_one = temp.join("one");
        let bin_two = temp.join("two");
        std::fs::create_dir_all(&bin_one).unwrap();
        std::fs::create_dir_all(&bin_two).unwrap();
        write_executable(&bin_one.join("agsh-cache-cmd"), "printf %s one");
        write_executable(&bin_two.join("agsh-cache-cmd"), "printf %s two");

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin_one.display().to_string());
        let mut executor = Executor::new();

        let first = executor
            .run_graph(
                &parse_line("agsh-cache-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(first.stdout, b"one");
        assert_eq!(state.path_cache_len_for_tests(), 1);

        state.export_var("PATH", bin_two.display().to_string());
        assert_eq!(state.path_cache_len_for_tests(), 0);
        let second = executor
            .run_graph(
                &parse_line("agsh-cache-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(second.stdout, b"two");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn temporary_path_assignment_does_not_poison_path_cache() {
        let temp = unique_temp_dir("path-cache-temp");
        let bin_one = temp.join("one");
        let bin_two = temp.join("two");
        std::fs::create_dir_all(&bin_one).unwrap();
        std::fs::create_dir_all(&bin_two).unwrap();
        write_executable(&bin_one.join("agsh-cache-temp-cmd"), "printf %s one");
        write_executable(&bin_two.join("agsh-cache-temp-cmd"), "printf %s two");

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin_one.display().to_string());
        let mut executor = Executor::new();

        let cached = executor
            .run_graph(
                &parse_line("agsh-cache-temp-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(cached.stdout, b"one");
        assert_eq!(state.path_cache_len_for_tests(), 1);

        let temporary = executor
            .run_graph(
                &parse_line(&format!("PATH={} agsh-cache-temp-cmd", bin_two.display())).unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(temporary.stdout, b"two");
        assert_eq!(state.path_cache_len_for_tests(), 1);

        let still_cached = executor
            .run_graph(
                &parse_line("agsh-cache-temp-cmd").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(still_cached.stdout, b"one");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn aliases_expand_before_builtins_and_can_be_removed() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias false=true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let aliased = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(aliased.exit_code, 0);

        let bypassed = executor
            .run_graph(
                &parse_line("command false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bypassed.exit_code, 1);

        executor
            .run_graph(
                &parse_line("unalias false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let unaliased = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(unaliased.exit_code, 1);
    }

    #[test]
    fn alias_values_can_include_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi world").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "hello world\n");
    }

    #[test]
    fn type_and_command_v_report_aliases() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let type_out = executor
            .run_graph(
                &parse_line("type hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "hi is aliased to echo hello\n"
        );

        let command_v = executor
            .run_graph(
                &parse_line("command -v hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "alias hi='echo hello'\n"
        );
    }

    #[test]
    fn abbreviations_expand_before_builtins_and_can_be_removed() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr false=true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let abbreviated = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(abbreviated.exit_code, 0);

        let bypassed = executor
            .run_graph(
                &parse_line("command false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bypassed.exit_code, 1);

        executor
            .run_graph(
                &parse_line("unabbr false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let unabbreviated = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(unabbreviated.exit_code, 1);
    }

    #[test]
    fn abbreviation_values_can_include_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi world").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "hello world\n");
    }

    #[test]
    fn aliases_take_precedence_over_abbreviations() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr hi='echo abbreviation'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("alias hi='echo alias'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "alias\n");
    }

    #[test]
    fn type_and_command_v_report_abbreviations() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("abbr hi='echo hello'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let type_out = executor
            .run_graph(
                &parse_line("type hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "hi is abbreviated to echo hello\n"
        );

        let command_v = executor
            .run_graph(
                &parse_line("command -v hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&command_v.stdout),
            "abbr hi='echo hello'\n"
        );
    }

    #[test]
    fn functions_define_and_execute_with_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("greet() { echo \"hi $1 $@\" }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("greet dev agent").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "hi dev dev agent\n"
        );
    }

    #[test]
    fn function_pipeline_consumer_reads_buffered_stdin() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("reader() { read A; read B; echo \"$A/$B\"; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | reader").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one/two\n");
    }

    #[test]
    fn non_final_function_pipeline_stage_feeds_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("reader() { while read line; do echo \"$line\"; done; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("printf 'one\ntwo\n' | reader | wc -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "2");
    }

    #[test]
    fn non_final_function_pipeline_stage_streams_to_next_command() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("reader() { read line; echo \"$line\"; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("yes y | reader | wc -l").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout).trim(), "1");
    }

    #[test]
    fn function_bodies_support_command_lists() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("hi() { echo one; false && echo no || echo two; }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line("hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
    }

    #[test]
    fn functions_scope_positionals_and_positional_count() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- outer-one outer-two outer-three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line(
                    r#"show_args() { printf '<%s>|<%s>|<%s>|<%s>|<%s>\n' "$#" "$1" "$2" "$3" "$@" }"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let scoped = executor
            .run_graph(
                &parse_line("show_args inner").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&scoped.stdout),
            "<1>|<inner>|<>|<>|<inner>\n"
        );

        let restored = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>|<%s>|<%s>\n' "$#" "$1" "$2" "$3" "$*""#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&restored.stdout),
            "<3>|<outer-one>|<outer-two>|<outer-three>|<outer-one outer-two outer-three>\n"
        );
    }

    #[test]
    fn functions_take_precedence_over_aliases_and_builtins() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("alias false=true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("false() { echo function-false }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let function = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&function.stdout),
            "function-false\n"
        );

        let bypassed = executor
            .run_graph(
                &parse_line("command false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(bypassed.exit_code, 1);
    }

    #[test]
    fn type_and_command_v_report_functions() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("hi() { echo hello }").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let type_out = executor
            .run_graph(
                &parse_line("type hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&type_out.stdout),
            "hi is a function\n"
        );

        let command_v = executor
            .run_graph(
                &parse_line("command -v hi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&command_v.stdout), "hi\n");
    }

    #[test]
    fn expands_variables_with_quote_rules() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo \"$FOO x\" '$FOO'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "bar x $FOO\n");
    }

    #[test]
    fn unquoted_expansions_are_field_split() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_var("WORDS", "one two\tthree\nfour");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>|<%s>\n' $WORDS"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>|<two>|<three>|<four>\n"
        );
    }

    #[test]
    fn native_list_values_expand_to_argv_without_ifs_splitting() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_value(
            "ITEMS",
            Value::List(vec![
                Value::String("one word".to_string()),
                Value::String("two".to_string()),
                Value::String(String::new()),
            ]),
        );
        let mut executor = Executor::new();

        let dollar_name = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' $ITEMS"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&dollar_name.stdout),
            "<one word>|<two>|<>\n"
        );

        let braced = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' ${ITEMS}"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&braced.stdout),
            "<one word>|<two>|<>\n"
        );
    }

    #[test]
    fn native_list_values_expand_through_set_and_for_items() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_value(
            "ITEMS",
            Value::List(vec![
                Value::String("one word".to_string()),
                Value::String("two".to_string()),
                Value::String(String::new()),
            ]),
        );
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- $ITEMS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(state.lookup("1"), Some("one word"));
        assert_eq!(state.lookup("2"), Some("two"));
        assert_eq!(state.lookup("3"), Some(""));
        assert_eq!(state.lookup("4"), None);

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item in $ITEMS; do printf '<%s>\n' "$item"; done"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one word>\n<two>\n<>\n"
        );
    }

    #[test]
    fn native_list_values_expand_inside_literal_prefix_suffix_words() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_value(
            "ITEMS",
            Value::List(vec![
                Value::String("one word".to_string()),
                Value::String("two".to_string()),
            ]),
        );
        let mut executor = Executor::new();

        let braced = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' pre${ITEMS}post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&braced.stdout),
            "<preone wordpost>|<pretwopost>\n"
        );

        let unbraced_with_prefix = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' pre$ITEMS"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&unbraced_with_prefix.stdout),
            "<preone word>|<pretwo>\n"
        );
    }

    #[test]
    fn non_whitespace_ifs_preserves_interior_empty_fields() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", ":");
        state.set_var("WORDS", "a::b:");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- $WORDS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(state.lookup("1"), Some("a"));
        assert_eq!(state.lookup("2"), Some(""));
        assert_eq!(state.lookup("3"), Some("b"));
        assert_eq!(state.lookup("4"), None);
    }

    #[test]
    fn ifs_whitespace_around_non_whitespace_delimiters_is_trimmed() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " :");
        state.set_var("WORDS", " a : b : ");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- $WORDS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(state.lookup("1"), Some("a"));
        assert_eq!(state.lookup("2"), Some("b"));
        assert_eq!(state.lookup("3"), None);
    }

    #[test]
    fn quoted_expansions_preserve_fields_and_empty_values() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_var("WORDS", "one two");
        state.set_var("EMPTY", "");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let quoted = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$WORDS" "$EMPTY""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "<one two>|<>\n");

        let unquoted_empty = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' a $EMPTY b "$MISSING""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&unquoted_empty.stdout),
            "<a>|<b>|<>\n"
        );
    }

    #[test]
    fn field_splitting_preserves_literal_word_parts() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        state.set_var("WORDS", "one two");
        let mut executor = Executor::new();

        let mixed = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' pre${WORDS}post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&mixed.stdout),
            "<preone>|<twopost>\n"
        );

        let escaped_space = executor
            .run_graph(
                &parse_line(r#"printf '<%s>\n' a\ b"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&escaped_space.stdout), "<a b>\n");
    }

    #[test]
    fn unquoted_command_substitution_is_field_split() {
        let mut state = ShellState::from_current_process();
        state.set_var("IFS", " \t\n");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' $(printf 'one two')"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<one>|<two>\n");
    }

    #[test]
    fn expands_parameter_default_and_alternate_forms() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        state.set_var("EMPTY", "");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let defaults = executor
            .run_graph(
                &parse_line("echo \"${MISSING:-fallback}/${EMPTY:-fallback}/${EMPTY-fallback}/${FOO:-fallback}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&defaults.stdout),
            "fallback/fallback//bar\n"
        );

        let alternates = executor
            .run_graph(
                &parse_line("echo \"${FOO:+yes}/${EMPTY:+yes}/${EMPTY+yes}/${MISSING+yes}\"")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&alternates.stdout), "yes//yes/\n");
    }

    #[test]
    fn expands_parameter_length_and_nested_default_word() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo \"${#FOO}/${MISSING:-$FOO}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "3/bar\n");
    }

    #[test]
    fn expands_positional_count_parameter() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let empty = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$#" "${#}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&empty.stdout), "<0>|<0>\n");

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let populated = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$#" "${#}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&populated.stdout), "<3>|<3>\n");
    }

    #[test]
    fn expands_positional_star_parameter() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one two three>|<one two three>\n"
        );
    }

    #[test]
    fn quoted_star_joins_positionals_with_first_ifs_character() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- "one word" two three"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("IFS=:").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one word:two:three>|<one word:two:three>\n"
        );
    }

    #[test]
    fn quoted_star_uses_empty_separator_when_ifs_is_empty() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        executor
            .run_graph(
                &parse_line("IFS=").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<onetwothree>|<onetwothree>\n"
        );
    }

    #[test]
    fn quoted_star_uses_space_separator_when_ifs_is_unset() {
        let mut state = ShellState::from_current_process();
        state.unset("IFS");
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -- one two three").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>\n' "$*" "${*}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one two three>|<one two three>\n"
        );
    }

    #[test]
    fn quoted_at_expands_to_separate_fields() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one "two words" three"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let dollar_at = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' "$@""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&dollar_at.stdout),
            "<one>|<two words>|<three>\n"
        );

        let braced_at = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' "${@}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&braced_at.stdout),
            "<one>|<two words>|<three>\n"
        );
    }

    #[test]
    fn quoted_at_with_no_positionals_expands_to_zero_fields() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set --").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo pre "$@" post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "pre post\n");
    }

    #[test]
    fn quoted_at_mixed_words_attach_prefix_and_suffix() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one two three"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let separated_segments = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' pre"$@"post"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&separated_segments.stdout),
            "<preone>|<two>|<threepost>\n"
        );

        let embedded_segment = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' "pre${@}post""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&embedded_segment.stdout),
            "<preone>|<two>|<threepost>\n"
        );
    }

    #[test]
    fn quoted_at_mixed_words_collapse_when_no_positionals() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set --").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '<%s>|<%s>|<%s>\n' pre"$@"post x"$@" "$@"y"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<prepost>|<x>|<y>\n"
        );
    }

    #[test]
    fn expands_shell_pid_parameter() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"printf '%s\n' "$$""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            format!("{}\n", std::process::id())
        );
    }

    #[test]
    fn expands_parameter_assignment_defaults_and_persists_values() {
        let mut state = ShellState::from_current_process();
        state.set_var("FOO", "bar");
        state.set_var("EMPTY", "");
        state.unset("MISSING");
        state.unset("MISSING2");
        let mut executor = Executor::new();

        let assigned = executor
            .run_graph(
                &parse_line(
                    "echo \"${MISSING:=fallback}/${EMPTY=kept}/${EMPTY:=filled}/${MISSING2=$FOO}\"",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&assigned.stdout),
            "fallback//filled/bar\n"
        );
        assert_eq!(state.lookup("MISSING"), Some("fallback"));
        assert_eq!(state.lookup("EMPTY"), Some("filled"));
        assert_eq!(state.lookup("MISSING2"), Some("bar"));

        let reused = executor
            .run_graph(
                &parse_line("echo \"$MISSING/$EMPTY/$MISSING2\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&reused.stdout),
            "fallback/filled/bar\n"
        );
    }

    #[test]
    fn parameter_error_form_fails_expansion() {
        let mut state = ShellState::from_current_process();
        state.unset("MISSING");
        let mut executor = Executor::new();

        let err = executor
            .run_graph(
                &parse_line("echo \"${MISSING:?need value}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();

        assert!(err.message.contains("MISSING: need value"));
    }

    #[test]
    fn parameter_assignment_rejects_special_targets() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let err = executor
            .run_graph(
                &parse_line("echo \"${1:=bad}\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap_err();

        assert!(err.message.contains("cannot assign default"));
    }

    #[test]
    fn expands_parameter_pattern_removal_forms() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATHLIKE", "src/bin/tool.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"echo "${PATHLIKE#*/}/${PATHLIKE##*/}/${PATHLIKE%/*}/${PATHLIKE%%/*}""#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "bin/tool.rs/tool.rs/src/bin/src\n"
        );
    }

    #[test]
    fn parameter_pattern_removal_expands_pattern_words() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATHLIKE", "src/bin/tool.rs");
        state.set_var("PREFIX_PATTERN", "src/*/");
        state.unset("MISSING");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"echo "${PATHLIKE#$PREFIX_PATTERN}/${PATHLIKE#nomatch}/${MISSING#*}""#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "tool.rs/src/bin/tool.rs/\n"
        );
    }

    #[test]
    fn parameter_pattern_removal_honors_empty_star_matches() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATHLIKE", "src/bin/tool.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo "${PATHLIKE#*}/${PATHLIKE##*}/${PATHLIKE%*}/${PATHLIKE%%*}""#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "src/bin/tool.rs//src/bin/tool.rs/\n"
        );
    }

    #[test]
    fn expands_parameter_pattern_substitution_forms() {
        let mut state = ShellState::from_current_process();
        state.set_var("VALUE", "one-two-two");
        state.set_var("REPL", "2");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"echo "${VALUE/two/$REPL}/${VALUE//two/$REPL}/${VALUE/#one/1}/${VALUE/%two/2}""#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "one-2-two/one-2-2/1-two-two/one-two-2\n"
        );
    }

    #[test]
    fn parameter_pattern_substitution_uses_shell_patterns() {
        let mut state = ShellState::from_current_process();
        state.set_var("VALUE", "one-two-two");
        state.set_var("STAR", "abc");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo "${VALUE/t*/X}/${VALUE//t*/X}/${STAR//*/X}""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one-X/one-X/X\n");
    }

    #[test]
    fn expands_last_status_parameter_between_commands() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let failed = executor
            .run_graph(
                &parse_line("false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failed.exit_code, 1);
        assert_eq!(state.last_status(), 1);

        let status = executor
            .run_graph(
                &parse_line("echo $? ${?}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(status.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&status.stdout), "1 1\n");
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn command_lists_execute_in_order_and_update_last_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("FOO=one; echo $FOO; false; echo $?; FOO=two").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\n1\n");
        assert_eq!(state.lookup("FOO"), Some("two"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn boolean_command_lists_short_circuit() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("false && echo no || echo yes; true || echo no; false && echo no")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "yes\n");
        assert_eq!(state.last_status(), 1);
    }

    #[test]
    fn if_executes_matching_branch_and_updates_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if false; then echo no; else echo yes; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "yes\n");
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn if_supports_elif_and_branch_state_changes() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "if false; then FOO=no; elif true; then FOO=elif; else FOO=else; fi; echo $FOO",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "elif\n");
        assert_eq!(state.lookup("FOO"), Some("elif"));
    }

    #[test]
    fn if_without_matching_branch_returns_zero() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if false; then echo no; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn if_treats_if_and_fi_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if true; then echo if fi; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "if fi\n");
    }

    #[test]
    fn compound_bodies_keep_unquoted_expansions_active() {
        let mut state = ShellState::from_current_process();
        state.set_var("WORD", "expanded");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("if true; then echo $WORD; fi").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "expanded\n");
    }

    #[test]
    fn while_repeats_until_condition_fails_and_updates_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "COUNT=0; while [ \"$COUNT\" -lt 3 ]; do echo $COUNT; COUNT=$(expr \"$COUNT\" + 1); done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\n2\n");
        assert_eq!(state.lookup("COUNT"), Some("3"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn while_without_matching_condition_returns_zero() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("while false; do echo no; done").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn while_treats_while_and_done_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "COUNT=0; while [ \"$COUNT\" -lt 1 ]; do echo while done; COUNT=1; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "while done\n");
    }

    #[test]
    fn until_repeats_until_condition_succeeds_and_updates_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "COUNT=0; until [ \"$COUNT\" -ge 3 ]; do echo $COUNT; COUNT=$(expr \"$COUNT\" + 1); done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "0\n1\n2\n");
        assert_eq!(state.lookup("COUNT"), Some("3"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn break_stops_current_loop_body() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "for item in one two three; do echo $item; break; echo no; done; echo after",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\nafter\n");
        assert_eq!(state.lookup("item"), Some("one"));
    }

    #[test]
    fn continue_skips_remaining_loop_body() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "for item in one two; do echo before:$item; continue; echo no; done; echo after",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "before:one\nbefore:two\nafter\n"
        );
        assert_eq!(state.lookup("item"), Some("two"));
    }

    #[test]
    fn loop_control_counts_cross_nested_loops() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "for outer in a b; do for inner in 1 2; do echo $outer:$inner; break 2; done; echo outer; done; echo after",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a:1\nafter\n");
        assert_eq!(state.lookup("outer"), Some("a"));
        assert_eq!(state.lookup("inner"), Some("1"));
    }

    #[test]
    fn break_and_continue_outside_loop_report_errors() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        // `break` with no count outside a loop is a successful no-op (bash/sh).
        let break_outcome = executor
            .run_graph(
                &parse_line("break").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        // `continue 0` is rejected for the invalid count regardless of context.
        let continue_outcome = executor
            .run_graph(
                &parse_line("continue 0").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(break_outcome.exit_code, 0);
        assert!(break_outcome.stderr.is_empty());
        assert_eq!(continue_outcome.exit_code, 2);
        assert_eq!(
            String::from_utf8_lossy(&continue_outcome.stderr),
            "continue: loop count must be a positive number\n"
        );
    }

    #[test]
    fn for_iterates_explicit_words_and_updates_variable() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    r#"for item in one "two words" three; do printf '<%s>\n' "$item"; done"#,
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>\n<two words>\n<three>\n"
        );
        assert_eq!(state.lookup("item"), Some("three"));
    }

    #[test]
    fn for_defaults_to_positional_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one "two words""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item; do printf '<%s>\n' "$item"; done"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<one>\n<two words>\n"
        );
    }

    #[test]
    fn select_runs_body_for_valid_choice_and_sets_reply() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '2\n' | select item in one \"two words\"; do printf '<%s:%s>\\n' \"$REPLY\" \"$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<2:two words>\n");
        assert_eq!(state.lookup("REPLY"), Some("2"));
        assert_eq!(state.lookup("item"), Some("two words"));
    }

    #[test]
    fn select_sets_empty_variable_for_invalid_choice() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '3\n' | select item in one two; do printf '<%s:%s>\\n' \"$REPLY\" \"$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<3:>\n");
        assert_eq!(state.lookup("REPLY"), Some("3"));
        assert_eq!(state.lookup("item"), Some(""));
    }

    #[test]
    fn select_defaults_to_positional_arguments() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line(r#"set -- one "two words""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "printf '2\n' | select item; do printf '<%s>\\n' \"$item\"; break; done",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "<two words>\n");
    }

    #[test]
    fn for_expands_unquoted_item_words_and_globs() {
        let temp = unique_temp_dir("for-glob");
        std::fs::write(temp.join("one.txt"), "").unwrap();
        std::fs::write(temp.join("two.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("WORDS", "alpha beta");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"for item in $WORDS *.txt; do printf '<%s>\n' "$item"; done"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "<alpha>\n<beta>\n<one.txt>\n<two.txt>\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn for_treats_for_and_done_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("for item in one; do echo for done; done").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "for done\n");
    }

    #[test]
    fn case_matches_alternative_pattern_and_runs_body() {
        let mut state = ShellState::from_current_process();
        state.set_var("KIND", "rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"case "$KIND" in rust|rs) echo rust ;; py) echo python ;; esac"#)
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "rust\n");
    }

    #[test]
    fn case_uses_wildcard_fallback_and_no_match_is_success() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let fallback = executor
            .run_graph(
                &parse_line("case txt in *.rs) echo rust ;; *) echo other ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(fallback.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&fallback.stdout), "other\n");

        let no_match = executor
            .run_graph(
                &parse_line("case txt in *.rs) echo rust ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(no_match.exit_code, 0);
        assert!(no_match.stdout.is_empty());
    }

    #[test]
    fn case_expands_patterns_and_allows_leading_paren() {
        let mut state = ShellState::from_current_process();
        state.set_var("PATTERN", "*.rs");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line(r#"case main.rs in $PATTERN) echo expanded ;; esac"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(expanded.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&expanded.stdout), "expanded\n");

        let leading_paren = executor
            .run_graph(
                &parse_line("case one in (one) echo paren ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(leading_paren.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&leading_paren.stdout), "paren\n");
    }

    #[test]
    fn case_treats_case_and_esac_arguments_as_normal_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("case x in x) echo case esac ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "case esac\n");
    }

    #[test]
    fn case_arm_bodies_support_nested_case_blocks() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "case outer in outer) case inner in inner) echo nested ;; esac; echo after ;; esac",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "nested\nafter\n");
    }

    #[test]
    fn case_semicolon_ampersand_falls_through_to_next_body() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("case x in x) echo one ;& y) echo two ;; *) echo other ;; esac")
                    .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
    }

    #[test]
    fn case_semicolon_semicolon_ampersand_continues_pattern_testing() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("case x in x) echo one ;;& y) echo no ;; x) echo two ;; esac").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one\ntwo\n");
    }

    #[test]
    fn command_list_errexit_stops_after_unhandled_failure() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set -e; false; FOO=after").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 1);
        assert!(state.errexit());
        assert_ne!(state.lookup("FOO"), Some("after"));
        assert_eq!(state.last_status(), 1);
    }

    #[test]
    fn command_list_errexit_allows_boolean_operands() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("set -e; false || echo recovered; FOO=after").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "recovered\n");
        assert_eq!(state.lookup("FOO"), Some("after"));
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn last_status_tracks_pipeline_exit_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let failed = executor
            .run_graph(
                &parse_line("true | false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failed.exit_code, 1);
        assert_eq!(state.last_status(), 1);

        let status = executor
            .run_graph(
                &parse_line("printf '%s\n' $?").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&status.stdout), "1\n");

        let ok = executor
            .run_graph(
                &parse_line("false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ok.exit_code, 0);
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn negated_pipelines_invert_status_without_changing_output() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let success = executor
            .run_graph(
                &parse_line("! false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(success.exit_code, 0);
        assert_eq!(state.last_status(), 0);

        let failure = executor
            .run_graph(
                &parse_line("! true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failure.exit_code, 1);
        assert_eq!(state.last_status(), 1);

        let output = executor
            .run_graph(
                &parse_line("! printf %s visible").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(output.exit_code, 1);
        assert_eq!(output.stdout, b"visible");
    }

    #[test]
    fn negated_pipelines_participate_in_command_lists() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("! false && echo recovered; ! true || echo failed").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "recovered\nfailed\n"
        );
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn negated_pipeline_applies_after_pipefail_status() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let default_status = executor
            .run_graph(
                &parse_line("! false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(default_status.exit_code, 1);

        executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let pipefailed = executor
            .run_graph(
                &parse_line("! false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(pipefailed.exit_code, 0);
        assert_eq!(state.last_status(), 0);
    }

    #[test]
    fn pipefail_uses_rightmost_nonzero_pipeline_status_for_buffered_pipelines() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let default_status = executor
            .run_graph(
                &parse_line("false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(default_status.exit_code, 0);
        assert_eq!(state.last_status(), 0);

        executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let pipefailed = executor
            .run_graph(
                &parse_line("false | true").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(pipefailed.exit_code, 1);
        assert_eq!(state.last_status(), 1);
    }

    #[test]
    fn pipefail_uses_rightmost_nonzero_pipeline_status_for_streaming_external_pipelines() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let default_status = executor
            .run_graph(
                &parse_line("sh -c 'exit 7' | sh -c 'exit 0'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(default_status.exit_code, 0);

        executor
            .run_graph(
                &parse_line("set -o pipefail").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        let pipefailed = executor
            .run_graph(
                &parse_line("sh -c 'exit 7' | sh -c 'exit 0'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(pipefailed.exit_code, 7);
        assert_eq!(state.last_status(), 7);
    }

    #[test]
    fn expands_command_substitution_in_unquoted_and_double_quoted_words() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let unquoted = executor
            .run_graph(
                &parse_line("echo $(printf %s hello)").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&unquoted.stdout), "hello\n");

        let double_quoted = executor
            .run_graph(
                &parse_line("echo \"a$(printf %s b)c\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&double_quoted.stdout), "abc\n");
    }

    #[test]
    fn expands_backtick_command_substitution_with_quote_rules() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let unquoted = executor
            .run_graph(
                &parse_line("echo `printf %s hi`").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&unquoted.stdout), "hi\n");

        let double_quoted = executor
            .run_graph(
                &parse_line("echo \"a`printf %s b`c\"").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&double_quoted.stdout), "abc\n");

        let single_quoted = executor
            .run_graph(
                &parse_line("echo '`printf %s hi`'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&single_quoted.stdout),
            "`printf %s hi`\n"
        );
    }

    #[test]
    fn single_quotes_suppress_command_substitution() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo '$(printf %s hello)'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&outcome.stdout),
            "$(printf %s hello)\n"
        );
    }

    #[test]
    fn expands_arithmetic_substitution_with_variables() {
        let mut state = ShellState::from_current_process();
        state.set_var("COUNT", "4");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo $((COUNT * 2 + (3 - 1)))").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "10\n");
    }

    #[test]
    fn expands_simple_globs() {
        let temp = unique_temp_dir("glob");
        std::fs::write(temp.join("one.txt"), "").unwrap();
        std::fs::write(temp.join("two.log"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo *.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one.txt\n");

        let quoted = executor
            .run_graph(
                &parse_line("echo '*.txt'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "*.txt\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn noglob_disables_pathname_expansion_without_disabling_field_splitting() {
        let temp = unique_temp_dir("noglob");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("PATTERNS", "*.rs literal");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line("echo *.rs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&expanded.stdout), "one.rs two.rs\n");

        executor
            .run_graph(
                &parse_line("set -f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(state.noglob());

        let literal = executor
            .run_graph(
                &parse_line("echo *.rs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&literal.stdout), "*.rs\n");

        let split_without_glob = executor
            .run_graph(
                &parse_line("echo $PATTERNS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&split_without_glob.stdout),
            "*.rs literal\n"
        );

        executor
            .run_graph(
                &parse_line("set +f").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(!state.noglob());

        let reenabled = executor
            .run_graph(
                &parse_line("echo *.rs").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&reenabled.stdout),
            "one.rs two.rs\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expands_globs_from_unquoted_expansions() {
        let temp = unique_temp_dir("expanded-glob");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();
        std::fs::write(temp.join("notes.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("IFS", " \t\n");
        state.set_var("RS_PATTERN", "*.rs");
        state.set_var("PATTERNS", "*.rs *.txt");
        let mut executor = Executor::new();

        let expanded = executor
            .run_graph(
                &parse_line("echo $RS_PATTERN").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&expanded.stdout), "one.rs two.rs\n");

        let split_patterns = executor
            .run_graph(
                &parse_line("echo $PATTERNS").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&split_patterns.stdout),
            "one.rs two.rs notes.txt\n"
        );

        let quoted = executor
            .run_graph(
                &parse_line(r#"echo "$RS_PATTERN""#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "*.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expanded_glob_patterns_compose_with_literal_word_parts() {
        let temp = unique_temp_dir("expanded-glob-mixed");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();
        std::fs::write(temp.join("other.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("STEM", "o*");
        state.set_var("NO_MATCH", "*.md");
        let mut executor = Executor::new();

        let mixed = executor
            .run_graph(
                &parse_line(r#"echo ${STEM}.rs"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&mixed.stdout), "one.rs\n");

        let no_match = executor
            .run_graph(
                &parse_line("echo $NO_MATCH").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&no_match.stdout), "*.md\n");

        let quoted_pattern_with_literal_suffix = executor
            .run_graph(
                &parse_line(r#"echo "$STEM".rs"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&quoted_pattern_with_literal_suffix.stdout),
            "o*.rs\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn quoted_glob_metachars_stay_literal_in_mixed_words() {
        let temp = unique_temp_dir("quoted-glob-mask");
        std::fs::write(temp.join("literal*x.rs"), "").unwrap();
        std::fs::write(temp.join("literalAx.rs"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("SUFFIX_PATTERN", "?.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo "literal*"$SUFFIX_PATTERN"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "literal*x.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn escaped_unquoted_glob_metachars_stay_literal_in_mixed_words() {
        let temp = unique_temp_dir("escaped-glob-mask");
        std::fs::write(temp.join("literal*x.rs"), "").unwrap();
        std::fs::write(temp.join("literalAx.rs"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.set_var("SUFFIX_PATTERN", "?.rs");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo literal\*$SUFFIX_PATTERN"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "literal*x.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn escaped_unquoted_dollar_stays_literal() {
        let mut state = ShellState::from_current_process();
        state.set_var("WORD", "expanded");
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo \$WORD $WORD"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "$WORD expanded\n");
    }

    #[test]
    fn unquoted_command_substitution_glob_patterns_expand() {
        let temp = unique_temp_dir("command-substitution-glob");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("two.rs"), "").unwrap();
        std::fs::write(temp.join("notes.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(r#"echo $(printf '*.rs')"#).unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one.rs two.rs\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expands_simple_braces_before_globbing() {
        let temp = unique_temp_dir("brace");
        std::fs::write(temp.join("one.rs"), "").unwrap();
        std::fs::write(temp.join("one.txt"), "").unwrap();
        std::fs::write(temp.join("one.md"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo one.{rs,txt}").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "one.rs one.txt\n");

        let quoted = executor
            .run_graph(
                &parse_line("echo 'one.{rs,txt}'").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&quoted.stdout), "one.{rs,txt}\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn expands_character_class_globs() {
        let temp = unique_temp_dir("class-glob");
        std::fs::write(temp.join("file1.txt"), "").unwrap();
        std::fs::write(temp.join("file2.txt"), "").unwrap();
        std::fs::write(temp.join("filea.txt"), "").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let range = executor
            .run_graph(
                &parse_line("echo file[0-9].txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&range.stdout),
            "file1.txt file2.txt\n"
        );

        let negated = executor
            .run_graph(
                &parse_line("echo file[!0-9].txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&negated.stdout), "filea.txt\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn redirects_builtin_stdout_to_file() {
        let temp = unique_temp_dir("redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo hello > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(outcome.stdout.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "hello\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn noclobber_blocks_normal_write_and_clobber_operator_overrides() {
        let temp = unique_temp_dir("noclobber");
        std::fs::write(temp.join("out.txt"), "old\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -C").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let blocked = executor
            .run_graph(
                &parse_line("echo new > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(blocked.exit_code, 1);
        assert!(String::from_utf8_lossy(&blocked.stderr).contains("exists"));
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "old\n"
        );

        let appended = executor
            .run_graph(
                &parse_line("echo append >> out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(appended.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "old\nappend\n"
        );

        let forced = executor
            .run_graph(
                &parse_line("echo forced >| out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(forced.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "forced\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn noclobber_applies_to_external_and_streaming_redirections() {
        let temp = unique_temp_dir("noclobber-external");
        std::fs::write(temp.join("external.txt"), "old").unwrap();
        std::fs::write(temp.join("stream.txt"), "old").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        executor
            .run_graph(
                &parse_line("set -o noclobber").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        let external = executor
            .run_graph(
                &parse_line("sh -c 'printf new' > external.txt").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(external.exit_code, 1);
        assert!(String::from_utf8_lossy(&external.stderr).contains("exists"));
        assert_eq!(
            std::fs::read_to_string(temp.join("external.txt")).unwrap(),
            "old"
        );

        let streaming = executor
            .run_graph(
                &parse_line("sh -c 'printf new' | cat > stream.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(streaming.exit_code, 1);
        assert!(String::from_utf8_lossy(&streaming.stderr).contains("exists"));
        assert_eq!(
            std::fs::read_to_string(temp.join("stream.txt")).unwrap(),
            "old"
        );

        let forced_external = executor
            .run_graph(
                &parse_line("sh -c 'printf forced' >| external.txt").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(forced_external.exit_code, 0);
        assert_eq!(
            std::fs::read_to_string(temp.join("external.txt")).unwrap(),
            "forced"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn redirects_builtin_stdout_to_stderr() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo hello 1>&2").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert!(outcome.stdout.is_empty());
        assert_eq!(outcome.stderr, b"hello\n");
    }

    #[test]
    fn closes_builtin_output_descriptors() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let stdout_closed = executor
            .run_graph(
                &parse_line("echo hidden 1>&-").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(stdout_closed.stdout.is_empty());
        assert!(stdout_closed.stderr.is_empty());

        // `1>&2 2>&-` duplicates fd1 from fd2 *before* closing fd2, so output
        // still reaches the original stderr destination (matches bash/sh).
        let stderr_closed = executor
            .run_graph(
                &parse_line("echo hidden 1>&2 2>&-").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert!(stderr_closed.stdout.is_empty());
        assert_eq!(stderr_closed.stderr, b"hidden\n");
    }

    #[test]
    fn external_redirection_order_controls_duplication_target() {
        let temp = unique_temp_dir("redir-order");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let combined = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' > both.txt 2>&1").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert!(combined.stdout.is_empty());
        assert!(combined.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("both.txt")).unwrap(),
            "outerr"
        );

        let split = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' 2>&1 > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert_eq!(split.stdout, b"err");
        assert!(split.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "out"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn closes_external_stderr_descriptor() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' 2>&-").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"out");
        assert!(outcome.stderr.is_empty());
    }

    #[test]
    fn pipeline_respects_stdout_to_stderr_duplication() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("echo hidden 1>&2 | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(outcome.stderr, b"hidden\n");
    }

    #[test]
    fn pipeline_uses_last_exit_status_and_raw_bytes() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let ok = executor
            .run_graph(
                &parse_line("printf %s hello | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(ok.exit_code, 0);
        assert_eq!(ok.stdout, b"hello");

        let failed = executor
            .run_graph(
                &parse_line("true | false").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();
        assert_eq!(failed.exit_code, 1);
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_to_early_exiting_consumers() {
        let temp = unique_temp_dir("stream-pipe");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stream-producer"),
            "while :; do\n  if ! printf 'line\\n'; then\n    exit 0\n  fi\ndone",
        );
        write_executable(
            &bin.join("agsh-stream-consumer"),
            "IFS= read -r line\nprintf '%s\\n' \"$line\"",
        );

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stream-producer | agsh-stream-consumer").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"line\n");

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_into_final_read_builtin() {
        let temp = unique_temp_dir("stream-read");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stream-producer"),
            "while :; do\n  if ! printf 'line\\n'; then\n    exit 0\n  fi\ndone",
        );

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stream-producer | read LINE").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(state.lookup("LINE"), Some("line"));

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_with_final_stdout_redirection() {
        let temp = unique_temp_dir("stream-pipe-redir");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stream-producer"),
            "while :; do\n  if ! printf 'line\\n'; then\n    exit 0\n  fi\ndone",
        );
        write_executable(
            &bin.join("agsh-stream-consumer"),
            "IFS= read -r line\nprintf '%s\\n' \"$line\"",
        );

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stream-producer | agsh-stream-consumer > out.txt").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "line\n"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn external_pipeline_streams_with_input_and_stderr_redirections() {
        let temp = unique_temp_dir("stream-pipe-io-redir");
        std::fs::write(temp.join("in.txt"), "alpha\n").unwrap();

        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line(
                    "cat < in.txt | sh -c 'IFS= read -r line; printf \"%s\" \"$line\"; printf err >&2' 2> err.txt",
                )
                .unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"alpha");
        assert!(outcome.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("err.txt")).unwrap(),
            "err"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn external_pipeline_streams_with_stderr_dup_to_stdout() {
        let temp = unique_temp_dir("stream-pipe-dup");
        let bin = temp.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_executable(
            &bin.join("agsh-stderr-producer"),
            "while :; do\n  if ! printf 'line\\n' >&2; then\n    exit 0\n  fi\ndone",
        );
        write_executable(
            &bin.join("agsh-stream-consumer"),
            "IFS= read -r line\nprintf '%s\\n' \"$line\"",
        );

        let mut state = ShellState::from_current_process();
        state.export_var("PATH", bin.display().to_string());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("agsh-stderr-producer 2>&1 | agsh-stream-consumer").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"line\n");
        assert!(outcome.stderr.is_empty());

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn external_pipeline_fd_dup_order_uses_current_targets() {
        let temp = unique_temp_dir("stream-pipe-dup-order");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out; printf err >&2' 2>&1 > out.txt | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, b"err");
        assert!(outcome.stderr.is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.join("out.txt")).unwrap(),
            "out"
        );

        std::fs::remove_dir_all(temp).unwrap();
    }

    #[test]
    fn external_pipeline_stdout_dup_to_stderr_closes_pipe() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out' 1>&2 | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert_eq!(outcome.stderr, b"out");
    }

    #[test]
    fn external_pipeline_stdout_close_closes_pipe() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();

        let outcome = executor
            .run_graph(
                &parse_line("sh -c 'printf out' 1>&- | cat").unwrap(),
                &mut state,
                &ExecutionOptions::default(),
            )
            .unwrap();

        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty());
    }

    fn run_capture(line: &str) -> CommandOutcome {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        executor
            .run_graph(
                &parse_line(line).unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap()
    }

    #[test]
    fn printf_cycles_format_over_arguments() {
        let outcome = run_capture("printf '%s\\n' a b c");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a\nb\nc\n");
    }

    #[test]
    fn printf_supports_integer_and_padding_conversions() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '%d\\n' 42").stdout),
            "42\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '%03d\\n' 7").stdout),
            "007\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '%x %X\\n' 255 255").stdout),
            "ff FF\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '[%5s][%-5s]' hi hi").stdout),
            "[   hi][hi   ]"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("printf '100%%\\n'").stdout),
            "100%\n"
        );
    }

    #[test]
    fn printf_percent_b_decodes_escapes() {
        let outcome = run_capture("printf '%b' 'a\\tb'");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a\tb");
    }

    #[test]
    fn echo_handles_n_and_e_flags() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -n hi").stdout),
            "hi"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo plain text").stdout),
            "plain text\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -e 'a\\tb'").stdout),
            "a\tb\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -ne 'x\\ny'").stdout),
            "x\ny"
        );
        // A non-flag leading argument stops option parsing.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo -- x").stdout),
            "-- x\n"
        );
    }

    #[test]
    fn type_reports_failure_exit_code() {
        let outcome = run_capture("type definitely_not_a_command_zzz");
        assert_eq!(outcome.exit_code, 1);
        assert!(outcome.stdout.is_empty());
    }

    #[test]
    fn test_builtin_supports_and_or_negation_and_grouping() {
        assert_eq!(run_capture("[ -n x -a -n y ]").exit_code, 0);
        assert_eq!(run_capture("[ -z x -a -n y ]").exit_code, 1);
        assert_eq!(run_capture("[ -z x -o -n y ]").exit_code, 0);
        assert_eq!(run_capture("[ ! -z x ]").exit_code, 0);
        assert_eq!(run_capture("[ ( -n x -o -z y ) -a -n z ]").exit_code, 0);
    }

    #[test]
    fn function_definition_syntax_variations() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f(){ echo hi; }; f").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f () { echo hi; }; f").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("greet(){ echo \"hi $1\"; }; greet bob").stdout),
            "hi bob\n"
        );
        // A subshell at command position is still not a function definition.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(echo sub)").stdout),
            "sub\n"
        );
    }

    #[test]
    fn temporary_prefix_assignment_applies_to_read() {
        let outcome =
            run_capture("printf 'a:1\\nb:2\\n' | while IFS=: read -r k v; do echo \"$k=$v\"; done");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "a=1\nb=2\n");
    }

    #[test]
    fn assignment_command_substitution_sets_status() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=$(exit 42); echo $?").stdout),
            "42\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=$(false) || echo handled").stdout),
            "handled\n"
        );
        // A plain assignment resets status to 0.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("false; x=5; echo $?").stdout),
            "0\n"
        );
    }

    #[test]
    fn redirections_inside_compound_and_function_bodies_stay_inside() {
        // Redirection inside a function body applies to that command only and
        // does not break the function definition.
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("f() { echo gone >/dev/null; echo kept; }; f").stdout
            ),
            "kept\n"
        );
        // Redirection inside a brace group applies to the inner command only.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ echo a >/dev/null; echo b; }").stdout),
            "b\n"
        );
        // Redirection inside a subshell applies to the inner command only.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("( echo gone >/dev/null; echo keep )").stdout),
            "keep\n"
        );
    }

    #[test]
    fn parameter_expansion_handles_spaces_and_nesting() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo ${UNSET:-a b}").stdout),
            "a b\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \"${UNSET:-a b}\"").stdout),
            "a b\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=1; echo ${X:+a b c}").stdout),
            "a b c\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("V=hi; echo \"${UNSET:-${V}}\"").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("V=hi; echo ${UNSET:-${V}}").stdout),
            "hi\n"
        );
    }

    #[test]
    fn captures_raw_trace_and_resolves_reference() {
        let mut state = ShellState::from_current_process();
        let mut executor = Executor::new();
        let graph = parse_line("printf 'hello\\nworld\\n'").unwrap();
        executor
            .run_graph(
                &graph,
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        let id = graph.id.to_string();
        let raw = state
            .resolve_trace(&format!("trace://{id}/stdout"))
            .expect("trace recorded");
        assert_eq!(String::from_utf8_lossy(&raw), "hello\nworld\n");
        // Raw mode does not capture, so it records no trace.
        assert!(state.trace_summaries().iter().any(|(tid, _, _)| tid == &id));
    }

    #[test]
    fn arithmetic_assignment_and_increment() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=5; echo $((x++)); echo $x").stdout),
            "5\n6\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=5; echo $((++x)); echo $x").stdout),
            "6\n6\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $((a=5)); echo $a").stdout),
            "5\n5\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=2; echo $((x+=3)); echo $x").stdout),
            "5\n5\n"
        );
    }

    #[test]
    fn arithmetic_expands_dollar_references_and_wraps_overflow() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=10; echo $(($x+5))").stdout),
            "15\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("set -- 7; echo $(($1-1))").stdout),
            "6\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $(( $((2+3)) * 2 ))").stdout),
            "10\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $(( ))").stdout),
            "0\n"
        );
        // Overflow wraps (two's complement) instead of panicking.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo $(( 9223372036854775807 + 1 ))").stdout),
            "-9223372036854775808\n"
        );
    }

    #[test]
    fn quoted_metacharacters_and_comments_are_literal() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \";\"").stdout),
            ";\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \"|\"").stdout),
            "|\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo a \">\" b").stdout),
            "a > b\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo hi #comment").stdout),
            "hi\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("# all comment").stdout),
            ""
        );
        // Real operators still work.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo a; echo b").stdout),
            "a\nb\n"
        );
    }

    #[test]
    fn posix_helper_builtins_and_runtime_errors() {
        // `:` null builtin succeeds.
        assert_eq!(run_capture(":; echo $?").exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&run_capture("if :; then echo yes; fi").stdout),
            "yes\n"
        );
        // shift adjusts positionals and $#.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("set -- a b c; shift; echo \"$# $@\"").stdout),
            "2 b c\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f() { shift; echo $1; }; f a b c").stdout),
            "b\n"
        );
        // A failed cd reports nonzero and continues the command list.
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("cd /no_such_dir_zzz 2>/dev/null; echo AFTER=$?").stdout
            ),
            "AFTER=1\n"
        );
        // cd updates PWD logically.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cd /usr && echo \"$PWD\"").stdout),
            "/usr\n"
        );
        // break/continue outside a loop are successful no-ops.
        assert_eq!(run_capture("break; echo $?").exit_code, 0);
    }

    #[test]
    fn nested_braces_and_cmdsub_in_double_quotes() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo {a,b{1,2}}").stdout),
            "a b1 b2\n"
        );
        // Command substitution inside double quotes may contain its own quotes.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo \"r: $(echo \"inner\")\"").stdout),
            "r: inner\n"
        );
        // $0 is the shell name, not an empty positional.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo ${0:+set}").stdout),
            "set\n"
        );
    }

    #[test]
    fn heredocs_and_herestrings_feed_stdin() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<< hello").stdout),
            "hello\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=world; cat <<< \"hi $X\"").stdout),
            "hi world\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<EOF\nline1\nline2\nEOF").stdout),
            "line1\nline2\n"
        );
        // Unquoted delimiter expands the body.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=v; cat <<EOF\nval=$X\nEOF").stdout),
            "val=v\n"
        );
        // Quoted delimiter keeps the body literal.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<'EOF'\nliteral $X\nEOF").stdout),
            "literal $X\n"
        );
        // A command after the heredoc terminator still runs.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("cat <<EOF\nbody\nEOF\necho after").stdout),
            "body\nafter\n"
        );
    }

    #[test]
    fn newlines_separate_commands() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("echo a\necho b").stdout),
            "a\nb\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("if true\nthen\necho yes\nfi").stdout),
            "yes\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("for i in 1 2 3\ndo\necho $i\ndone").stdout),
            "1\n2\n3\n"
        );
        // A newline after a list operator is a continuation, not a separator.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("true &&\necho ok").stdout),
            "ok\n"
        );
    }

    #[test]
    fn subshell_isolates_state_and_carries_status() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(echo a; echo b)").stdout),
            "a\nb\n"
        );
        // Variable changes inside a subshell do not leak out.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("X=1; (X=2); echo $X").stdout),
            "1\n"
        );
        // The subshell's exit status propagates.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(exit 5); echo $?").stdout),
            "5\n"
        );
        // A subshell can be a pipeline producer.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("(echo b; echo a) | sort").stdout),
            "a\nb\n"
        );
    }

    #[test]
    fn brace_group_runs_in_current_state() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ echo a; echo b; }").stdout),
            "a\nb\n"
        );
        // Brace groups do not create a subshell: assignments persist.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ X=5; }; echo $X").stdout),
            "5\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("{ echo b; echo a; } | sort").stdout),
            "a\nb\n"
        );
    }

    #[test]
    fn return_builtin_stops_function_with_status() {
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f() { return 3; }; f; echo $?").stdout),
            "3\n"
        );
        assert_eq!(
            String::from_utf8_lossy(&run_capture("f() { echo a; return; echo b; }; f").stdout),
            "a\n"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("f() { for i in 1 2 3; do [ $i = 2 ] && return; echo $i; done; }; f")
                    .stdout
            ),
            "1\n"
        );
        // Nested functions consume their own return without leaking.
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture(
                    "inner() { return 2; }; outer() { inner; echo a=$?; return 9; }; outer; echo b=$?"
                )
                .stdout
            ),
            "a=2\nb=9\n"
        );
    }

    #[test]
    fn local_variables_are_function_scoped() {
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("f() { local x=5; echo $x; }; f; echo \"[${x}]\"").stdout
            ),
            "5\n[]\n"
        );
        assert_eq!(
            String::from_utf8_lossy(
                &run_capture("x=g; f() { local x=l; echo $x; }; f; echo $x").stdout
            ),
            "l\ng\n"
        );
        // Non-local assignments still mutate the global binding.
        assert_eq!(
            String::from_utf8_lossy(&run_capture("x=1; f() { x=2; }; f; echo $x").stdout),
            "2\n"
        );
    }

    #[test]
    fn command_not_found_returns_127_and_continues() {
        let outcome = run_capture("definitely_missing_zzz; echo after");
        assert_eq!(String::from_utf8_lossy(&outcome.stdout), "after\n");

        let status = run_capture("definitely_missing_zzz 2>/dev/null; echo $?");
        assert_eq!(String::from_utf8_lossy(&status.stdout), "127\n");

        // A missing command inside a function yields 127, not a hard abort.
        let in_fn = run_capture("f() { missing_inner_zzz; }; f; echo $?");
        assert_eq!(String::from_utf8_lossy(&in_fn.stdout), "127\n");
    }

    #[test]
    fn arithmetic_supports_full_operator_set() {
        let cases = [
            ("$((3 > 2))", "1"),
            ("$((3 < 2))", "0"),
            ("$((5 == 5))", "1"),
            ("$((5 != 4))", "1"),
            ("$((6 & 3))", "2"),
            ("$((6 | 1))", "7"),
            ("$((6 ^ 3))", "5"),
            ("$((1 << 4))", "16"),
            ("$((256 >> 2))", "64"),
            ("$((1 ? 7 : 8))", "7"),
            ("$((0 ? 7 : 8))", "8"),
            ("$((1 && 0))", "0"),
            ("$((0 || 3))", "1"),
            ("$((!0))", "1"),
            ("$((~5))", "-6"),
            ("$((2 ** 10))", "1024"),
            ("$((0xff))", "255"),
            ("$((010))", "8"),
            ("$(( (5 > 3) && (2 < 4) ))", "1"),
        ];
        for (expr, expected) in cases {
            let outcome = run_capture(&format!("echo {expr}"));
            assert_eq!(
                String::from_utf8_lossy(&outcome.stdout).trim(),
                expected,
                "expr {expr}"
            );
        }
    }

    #[test]
    fn builtin_redirection_orders_dup_after_file() {
        let temp = unique_temp_dir("builtin-redir");
        let mut state = ShellState::from_current_process();
        state.set_cwd(temp.clone());
        let mut executor = Executor::new();
        let outcome = executor
            .run_graph(
                &parse_line("type missing_cmd_zzz >out.txt 2>&1").unwrap(),
                &mut state,
                &ExecutionOptions {
                    output_mode: OutputMode::Clean,
                    allow_process_replacement: false,
                },
            )
            .unwrap();
        assert!(outcome.stdout.is_empty());
        assert!(outcome.stderr.is_empty());
        let written = std::fs::read_to_string(temp.join("out.txt")).unwrap();
        assert!(
            written.contains("not found"),
            "expected error in file: {written}"
        );
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("agsh-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}
