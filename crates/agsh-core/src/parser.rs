use crate::ir::{
    Assignment, CommandGraph, CommandInvocation, CommandList, CommandListItem, ListOperator,
    Pipeline, Redirection, RedirectionMode, RedirectionTarget,
};
use crate::lexer::{lex, WordSegment};
use crate::{ShellError, SourceSpan};

pub fn parse_line(input: &str) -> Result<CommandGraph, ShellError> {
    let (command_src, heredoc_bodies) = extract_heredoc_bodies(input)?;
    let tokens = lex(&command_src)?;
    let mut items = Vec::new();
    let mut commands = Vec::new();
    let mut current = Vec::new();
    let mut operator = ListOperator::Always;
    let mut negated = false;
    let mut function_body_depth = 0usize;
    let mut if_block_depth = 0usize;
    let mut while_block_depth = 0usize;
    let mut for_block_depth = 0usize;
    let mut case_block_depth = 0usize;
    let mut subshell_depth: isize = 0;
    let mut brace_group_depth: isize = 0;
    let mut in_double_bracket = false;

    for token in tokens {
        // `[[ ... ]]`: collect every inner token as a word so operators like
        // `<`, `>`, `&&`, `||`, `(`, `)` are conditional operators, not shell
        // redirections/separators. push_command builds it specially.
        if in_double_bracket {
            let ends = token.text == "]]" && token.quote == crate::QuoteKind::None;
            current.push(token);
            if ends {
                in_double_bracket = false;
            }
            continue;
        }

        if subshell_depth > 0 {
            subshell_depth += paren_delta(&token);
            current.push(token);
            continue;
        }
        if brace_group_depth > 0 {
            brace_group_depth += brace_group_delta(&token);
            current.push(token);
            continue;
        }
        if function_body_depth > 0 {
            update_function_body_depth(&token, &mut function_body_depth);
            current.push(token);
            continue;
        }
        if if_block_depth > 0 {
            let command_position = is_reserved_command_position(&current);
            update_if_block_depth(&token, &mut if_block_depth, command_position);
            current.push(token);
            continue;
        }
        if while_block_depth > 0 {
            let command_position = is_reserved_command_position(&current);
            update_while_block_depth(&token, &mut while_block_depth, command_position);
            current.push(token);
            continue;
        }
        if for_block_depth > 0 {
            let command_position = is_reserved_command_position(&current);
            update_for_block_depth(&token, &mut for_block_depth, command_position);
            current.push(token);
            continue;
        }
        if case_block_depth > 0 {
            let command_position = is_case_reserved_command_position(&current);
            update_case_block_depth(&token, &mut case_block_depth, command_position);
            current.push(token);
            continue;
        }

        match token.text.as_str() {
            "!" if token.quote == crate::QuoteKind::None
                && current.is_empty()
                && commands.is_empty() =>
            {
                negated = !negated;
            }
            "|" if token.quote == crate::QuoteKind::None => {
                if current.is_empty() {
                    return Err(
                        ShellError::parse("empty command in pipeline").with_span(token.span)
                    );
                }
                push_command(&mut commands, &current)?;
                current.clear();
            }
            ";" | "&&" | "||" | "&" if token.quote == crate::QuoteKind::None => {
                if current.is_empty() && commands.is_empty() {
                    if negated {
                        return Err(
                            ShellError::parse("missing command after !").with_span(token.span)
                        );
                    }
                    if token.text == ";" {
                        operator = ListOperator::Always;
                        continue;
                    }
                    return Err(ShellError::parse(format!(
                        "missing command before {}",
                        token.text
                    ))
                    .with_span(token.span));
                }

                let background = token.text == "&";
                push_command(&mut commands, &current)?;
                current.clear();
                if !commands.is_empty() {
                    items.push(CommandListItem {
                        operator,
                        pipeline: Pipeline::new(commands, negated),
                        background,
                    });
                    commands = Vec::new();
                    negated = false;
                }
                operator = match token.text.as_str() {
                    "&&" => ListOperator::And,
                    "||" => ListOperator::Or,
                    _ => ListOperator::Always,
                };
            }
            _ => {
                let starts_if_block = token.text == "if"
                    && token.quote == crate::QuoteKind::None
                    && current.is_empty();
                let starts_while_block = matches!(token.text.as_str(), "while" | "until")
                    && token.quote == crate::QuoteKind::None
                    && current.is_empty();
                let starts_for_block = matches!(token.text.as_str(), "for" | "select")
                    && token.quote == crate::QuoteKind::None
                    && current.is_empty();
                let starts_case_block = token.text == "case"
                    && token.quote == crate::QuoteKind::None
                    && current.is_empty();
                let starts_subshell = current.is_empty() && token_starts_subshell(&token);
                let starts_brace_group = current.is_empty()
                    && token.text == "{"
                    && token.quote == crate::QuoteKind::None;
                let starts_double_bracket = current.is_empty()
                    && token.text == "[["
                    && token.quote == crate::QuoteKind::None;
                current.push(token);
                if starts_double_bracket {
                    in_double_bracket = true;
                } else if starts_subshell {
                    let last = current.last().expect("just pushed");
                    subshell_depth = paren_delta(last).max(0);
                } else if starts_brace_group {
                    brace_group_depth = 1;
                } else if starts_function_body(&current) {
                    function_body_depth = 1;
                } else if starts_if_block {
                    if_block_depth = 1;
                } else if starts_while_block {
                    while_block_depth = 1;
                } else if starts_for_block {
                    for_block_depth = 1;
                } else if starts_case_block {
                    case_block_depth = 1;
                }
            }
        }
    }

    if current.is_empty() && commands.is_empty() && negated {
        return Err(ShellError::parse("missing command after !"));
    }

    if current.is_empty()
        && commands.is_empty()
        && matches!(operator, ListOperator::And | ListOperator::Or)
    {
        return Err(ShellError::parse("missing command after list operator"));
    }

    push_command(&mut commands, &current)?;
    if !commands.is_empty() {
        items.push(CommandListItem {
            operator,
            pipeline: Pipeline::new(commands, negated),
            background: false,
        });
    }

    attach_heredoc_bodies(&mut items, heredoc_bodies)?;

    Ok(CommandGraph::with_list(input, CommandList { items }))
}

/// True when `input` is a syntactically incomplete command that needs more
/// lines (unterminated quote, open compound block, open heredoc, or a trailing
/// pipe/`&&`/`||`/line-continuation). Used to drive multi-line input.
pub fn is_incomplete(input: &str) -> bool {
    // Open heredoc: a delimiter whose terminator line has not arrived yet.
    if let Err(error) = extract_heredoc_bodies(input) {
        return error.message.contains("end-of-file");
    }
    let Ok((command_src, _)) = extract_heredoc_bodies(input) else {
        return false;
    };

    let tokens = match lex(&command_src) {
        Ok(tokens) => tokens,
        // An unterminated quote/substitution means more input is needed.
        Err(error) => return error.message.contains("unterminated"),
    };

    if ends_with_line_continuation(&command_src) {
        return true;
    }

    let mut function_body_depth = 0usize;
    let mut if_block_depth = 0usize;
    let mut while_block_depth = 0usize;
    let mut for_block_depth = 0usize;
    let mut case_block_depth = 0usize;
    let mut subshell_depth: isize = 0;
    let mut brace_group_depth: isize = 0;
    let mut current: Vec<crate::lexer::Token> = Vec::new();
    let mut trailing_operator: Option<String> = None;

    for token in tokens {
        if subshell_depth > 0 {
            subshell_depth += paren_delta(&token);
            current.push(token);
            continue;
        }
        if brace_group_depth > 0 {
            brace_group_depth += brace_group_delta(&token);
            current.push(token);
            continue;
        }
        if function_body_depth > 0 {
            update_function_body_depth(&token, &mut function_body_depth);
            current.push(token);
            continue;
        }
        if if_block_depth > 0 {
            let cp = is_reserved_command_position(&current);
            update_if_block_depth(&token, &mut if_block_depth, cp);
            current.push(token);
            continue;
        }
        if while_block_depth > 0 {
            let cp = is_reserved_command_position(&current);
            update_while_block_depth(&token, &mut while_block_depth, cp);
            current.push(token);
            continue;
        }
        if for_block_depth > 0 {
            let cp = is_reserved_command_position(&current);
            update_for_block_depth(&token, &mut for_block_depth, cp);
            current.push(token);
            continue;
        }
        if case_block_depth > 0 {
            let cp = is_case_reserved_command_position(&current);
            update_case_block_depth(&token, &mut case_block_depth, cp);
            current.push(token);
            continue;
        }

        if token.quote == crate::QuoteKind::None
            && matches!(token.text.as_str(), "|" | ";" | "&&" | "||" | "&")
        {
            trailing_operator = Some(token.text.clone());
            current.clear();
            continue;
        }

        let starts_if_block =
            token.text == "if" && token.quote == crate::QuoteKind::None && current.is_empty();
        let starts_while_block = matches!(token.text.as_str(), "while" | "until")
            && token.quote == crate::QuoteKind::None
            && current.is_empty();
        let starts_for_block = matches!(token.text.as_str(), "for" | "select")
            && token.quote == crate::QuoteKind::None
            && current.is_empty();
        let starts_case_block =
            token.text == "case" && token.quote == crate::QuoteKind::None && current.is_empty();
        let starts_subshell = current.is_empty() && token_starts_subshell(&token);
        let starts_brace_group =
            current.is_empty() && token.text == "{" && token.quote == crate::QuoteKind::None;
        current.push(token);
        trailing_operator = None;
        if starts_subshell {
            subshell_depth = paren_delta(current.last().expect("just pushed")).max(0);
        } else if starts_brace_group {
            brace_group_depth = 1;
        } else if starts_function_body(&current) {
            function_body_depth = 1;
        } else if starts_if_block {
            if_block_depth = 1;
        } else if starts_while_block {
            while_block_depth = 1;
        } else if starts_for_block {
            for_block_depth = 1;
        } else if starts_case_block {
            case_block_depth = 1;
        }
    }

    if function_body_depth > 0
        || if_block_depth > 0
        || while_block_depth > 0
        || for_block_depth > 0
        || case_block_depth > 0
        || subshell_depth > 0
        || brace_group_depth > 0
    {
        return true;
    }

    matches!(trailing_operator.as_deref(), Some("|" | "||" | "&&"))
}

/// True if `input` ends with an unescaped (odd count) trailing backslash, i.e.
/// a line continuation.
fn ends_with_line_continuation(input: &str) -> bool {
    let trailing = input.chars().rev().take_while(|&c| c == '\\').count();
    trailing % 2 == 1
}

#[derive(Debug, Clone)]
struct HeredocOp {
    delimiter: String,
    strip_tabs: bool,
    expand: bool,
}

/// Split heredoc bodies out of the raw input. Returns the command text (with
/// body lines removed) and the collected `(body, expand)` pairs in source order.
fn extract_heredoc_bodies(input: &str) -> Result<(String, Vec<(String, bool)>), ShellError> {
    if !input.contains("<<") {
        return Ok((input.to_string(), Vec::new()));
    }

    // Walk the input line by line. A heredoc operator may appear on any command
    // line (not just the first), and its body is the lines immediately following
    // that line, up to the delimiter. Multiple heredocs on one line consume
    // their bodies in order. Body and delimiter lines are removed from the
    // reconstructed command text.
    let lines: Vec<&str> = input.split('\n').collect();
    let mut command_lines: Vec<String> = Vec::new();
    let mut bodies: Vec<(String, bool)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let command_line = lines[i];
        command_lines.push(command_line.to_string());
        i += 1;

        for op in scan_heredoc_ops(command_line) {
            let mut body = String::new();
            let mut found = false;
            while i < lines.len() {
                let line = lines[i];
                i += 1;
                let measured = if op.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line
                };
                if measured == op.delimiter {
                    found = true;
                    break;
                }
                body.push_str(measured);
                body.push('\n');
            }
            if !found {
                return Err(ShellError::parse(format!(
                    "heredoc delimited by end-of-file (wanted `{}`)",
                    op.delimiter
                )));
            }
            bodies.push((body, op.expand));
        }
    }

    Ok((command_lines.join("\n"), bodies))
}

/// Scan the command line (before the first newline) for `<<`/`<<-` operators,
/// honoring quotes so `echo "<<x"` is not treated as a heredoc. `<<<` is a
/// herestring and is skipped here.
fn scan_heredoc_ops(input: &str) -> Vec<HeredocOp> {
    let line_end = input.find('\n').unwrap_or(input.len());
    let chars: Vec<char> = input[..line_end].chars().collect();
    let mut ops = Vec::new();
    let mut i = 0;
    let mut quote: Option<char> = None;

    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                i += 1;
            }
            '\\' => i += 2,
            // Skip `$(...)` / `$((...))` command and arithmetic substitutions so a
            // left-shift `<<` inside arithmetic is not mistaken for a heredoc.
            '$' if chars.get(i + 1) == Some(&'(') => {
                let mut depth = 0usize;
                let mut j = i + 1;
                while j < chars.len() {
                    match chars[j] {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                i = j;
            }
            // Skip backtick command substitutions.
            '`' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '`' {
                    j += 1;
                }
                i = j + 1;
            }
            // `<<<` is a herestring, not a heredoc: skip all three characters so
            // the trailing `<<` is not mistaken for a heredoc operator.
            '<' if chars.get(i + 1) == Some(&'<') && chars.get(i + 2) == Some(&'<') => {
                i += 3;
            }
            '<' if chars.get(i + 1) == Some(&'<') => {
                let strip_tabs = chars.get(i + 2) == Some(&'-');
                let mut j = i + 2 + usize::from(strip_tabs);
                while chars.get(j).is_some_and(|c| *c == ' ' || *c == '\t') {
                    j += 1;
                }
                let (delimiter, expand, next) = read_heredoc_delimiter(&chars, j);
                if delimiter.is_empty() {
                    i = next.max(i + 1);
                    continue;
                }
                ops.push(HeredocOp {
                    delimiter,
                    strip_tabs,
                    expand,
                });
                i = next;
            }
            _ => i += 1,
        }
    }

    ops
}

/// Read a heredoc delimiter word starting at `start`. A quoted (or
/// backslash-prefixed) delimiter disables body expansion.
fn read_heredoc_delimiter(chars: &[char], start: usize) -> (String, bool, usize) {
    let mut i = start;
    let mut delimiter = String::new();
    let mut expand = true;

    while i < chars.len() {
        match chars[i] {
            '\'' | '"' => {
                expand = false;
                let quote = chars[i];
                i += 1;
                while i < chars.len() && chars[i] != quote {
                    delimiter.push(chars[i]);
                    i += 1;
                }
                i += 1; // closing quote
            }
            '\\' => {
                expand = false;
                i += 1;
                if i < chars.len() {
                    delimiter.push(chars[i]);
                    i += 1;
                }
            }
            c if c.is_whitespace() || matches!(c, '<' | '>' | '|' | ';' | '&' | '(' | ')') => break,
            c => {
                delimiter.push(c);
                i += 1;
            }
        }
    }

    (delimiter, expand, i)
}

/// Attach extracted heredoc bodies to `HereDoc` redirections in source order.
fn attach_heredoc_bodies(
    items: &mut [CommandListItem],
    bodies: Vec<(String, bool)>,
) -> Result<(), ShellError> {
    let mut bodies = bodies.into_iter();
    for item in items.iter_mut() {
        for command in item.pipeline.commands.iter_mut() {
            for redirection in command.redirections.iter_mut() {
                if redirection.mode != RedirectionMode::HereDoc {
                    continue;
                }
                let Some((body, expand)) = bodies.next() else {
                    return Err(ShellError::parse("heredoc body missing for redirection"));
                };
                let quote = if expand {
                    crate::QuoteKind::Double
                } else {
                    crate::QuoteKind::Single
                };
                redirection.target = RedirectionTarget::Word {
                    text: body.clone(),
                    quote,
                    segments: vec![WordSegment::new(body, quote)],
                };
            }
        }
    }
    Ok(())
}

fn update_if_block_depth(token: &crate::lexer::Token, depth: &mut usize, command_position: bool) {
    if token.quote != crate::QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "if" if command_position => *depth += 1,
        "fi" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn update_while_block_depth(
    token: &crate::lexer::Token,
    depth: &mut usize,
    command_position: bool,
) {
    if token.quote != crate::QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "while" | "until" | "for" | "select" if command_position => *depth += 1,
        "done" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn update_for_block_depth(token: &crate::lexer::Token, depth: &mut usize, command_position: bool) {
    if token.quote != crate::QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "while" | "until" | "for" | "select" if command_position => *depth += 1,
        "done" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn update_case_block_depth(token: &crate::lexer::Token, depth: &mut usize, command_position: bool) {
    if token.quote != crate::QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "case" if command_position => *depth += 1,
        "esac" if command_position => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn is_reserved_command_position(tokens: &[crate::lexer::Token]) -> bool {
    let Some(previous) = tokens
        .iter()
        .rev()
        .find(|token| token.quote == crate::QuoteKind::None)
    else {
        return true;
    };

    matches!(
        previous.text.as_str(),
        ";" | "&&" | "||" | "|" | "then" | "elif" | "else" | "do"
    )
}

fn is_case_reserved_command_position(tokens: &[crate::lexer::Token]) -> bool {
    let Some(previous) = tokens
        .iter()
        .rev()
        .find(|token| token.quote == crate::QuoteKind::None)
    else {
        return true;
    };

    matches!(
        previous.text.as_str(),
        ";" | "&&" | "||" | "|" | "then" | "elif" | "else" | "do"
    ) || previous.text.ends_with(')')
}

/// For a compound command (subshell, brace group, or function definition),
/// return the index of the token that closes its body. Redirections at or
/// before this index belong inside the body; later ones apply to the compound.
/// Returns `None` for non-compound commands.
fn compound_body_end(tokens: &[crate::lexer::Token]) -> Option<usize> {
    let first = tokens.first()?;
    if first.quote != crate::QuoteKind::None {
        return None;
    }

    // Subshell: balance parentheses across token text.
    if token_starts_subshell(first) {
        let mut depth: isize = 0;
        for (index, token) in tokens.iter().enumerate() {
            depth += paren_delta(token);
            if depth == 0 {
                return Some(index);
            }
        }
        return None;
    }

    // Brace group or function definition body: balance `{`/`}` tokens. The body
    // opens at the first `{` (the group itself, or the function's body brace).
    let opens_brace_group = first.text == "{";
    let spaced_function_def =
        is_identifier(&first.text) && tokens.get(1).is_some_and(|token| token.text == "()");
    let is_function_def = first.text.ends_with("()")
        || (first.text == "function" && tokens.len() > 2)
        || spaced_function_def;
    if opens_brace_group || is_function_def {
        let mut depth: isize = 0;
        let mut seen_open = false;
        for (index, token) in tokens.iter().enumerate() {
            let delta = brace_group_delta(token);
            depth += delta;
            if delta > 0 {
                seen_open = true;
            }
            if seen_open && depth == 0 {
                return Some(index);
            }
        }
        return None;
    }

    None
}

/// True if a token at command position opens a subshell, i.e. its first
/// unquoted character is `(`.
fn token_starts_subshell(token: &crate::lexer::Token) -> bool {
    // `(( ... ))` is an arithmetic group, not a subshell.
    if token.text.starts_with("((") && token.text.ends_with("))") {
        return false;
    }
    match token.segments.first() {
        Some(segment) => segment.quote == crate::QuoteKind::None && segment.text.starts_with('('),
        None => token.quote == crate::QuoteKind::None && token.text.starts_with('('),
    }
}

/// Net change in subshell paren depth contributed by a token, counting only
/// unquoted `(`/`)` characters. Balanced `$(...)`/`$((...))` substitutions live
/// in a single unquoted segment and net to zero, so they are naturally ignored.
fn paren_delta(token: &crate::lexer::Token) -> isize {
    let count_in = |text: &str| -> isize {
        text.chars()
            .map(|c| match c {
                '(' => 1,
                ')' => -1,
                _ => 0,
            })
            .sum()
    };
    if token.segments.is_empty() {
        if token.quote == crate::QuoteKind::None {
            count_in(&token.text)
        } else {
            0
        }
    } else {
        token
            .segments
            .iter()
            .filter(|segment| segment.quote == crate::QuoteKind::None)
            .map(|segment| count_in(&segment.text))
            .sum()
    }
}

/// Net change in brace-group depth: unquoted `{`/`}` tokens.
fn brace_group_delta(token: &crate::lexer::Token) -> isize {
    if token.quote != crate::QuoteKind::None {
        return 0;
    }
    match token.text.as_str() {
        "{" => 1,
        "}" => -1,
        _ => 0,
    }
}

fn update_function_body_depth(token: &crate::lexer::Token, depth: &mut usize) {
    if token.quote != crate::QuoteKind::None {
        return;
    }
    match token.text.as_str() {
        "{" => *depth += 1,
        "}" => *depth = depth.saturating_sub(1),
        _ => {}
    }
}

fn starts_function_body(tokens: &[crate::lexer::Token]) -> bool {
    let Some(last) = tokens.last() else {
        return false;
    };
    if last.text != "{" || last.quote != crate::QuoteKind::None {
        return false;
    }
    if tokens.len() == 2 {
        return tokens[0].text.ends_with("()") && tokens[0].quote == crate::QuoteKind::None;
    }
    if tokens.len() == 3
        && tokens[0].quote == crate::QuoteKind::None
        && tokens[1].quote == crate::QuoteKind::None
    {
        // `function name {` or `name () {` (space before the parentheses).
        return tokens[0].text == "function"
            || (is_identifier(&tokens[0].text) && tokens[1].text == "()");
    }
    false
}

fn push_command(
    commands: &mut Vec<CommandInvocation>,
    tokens: &[crate::lexer::Token],
) -> Result<(), ShellError> {
    if tokens.is_empty() {
        return Ok(());
    }

    // `[[ ... ]]`: every token is a conditional operand/operator word; do not
    // parse redirections or assignments inside it.
    if tokens[0].text == "[[" && tokens[0].quote == crate::QuoteKind::None {
        commands.push(CommandInvocation {
            assignments: Vec::new(),
            argv: tokens.iter().map(|t| t.text.clone()).collect(),
            argv_quote: tokens.iter().map(|t| t.quote).collect(),
            // Operator tokens (`&&`, `<`, …) carry no word segments; synthesize
            // one from their text so they expand to themselves.
            argv_segments: tokens
                .iter()
                .map(|t| {
                    if t.segments.is_empty() {
                        vec![WordSegment::new(t.text.clone(), t.quote)]
                    } else {
                        t.segments.clone()
                    }
                })
                .collect(),
            redirections: Vec::new(),
            span: tokens.first().map(|t| t.span),
        });
        return Ok(());
    }

    let mut assignments = Vec::new();
    let mut argv = Vec::new();
    let mut argv_quote = Vec::new();
    let mut argv_segments = Vec::new();
    let mut redirections = Vec::new();
    let mut saw_argv = false;
    let mut index = 0;

    // For compound commands (subshell/brace group/function definition), keep
    // redirections that appear *inside* the body as part of the body; only
    // redirections after the closing delimiter apply to the compound itself.
    let redir_start = compound_body_end(tokens).map_or(0, |end| end + 1);

    while index < tokens.len() {
        let token = &tokens[index];
        // A quoted operator (e.g. `echo ">"`) is a literal argument, not a
        // redirection.
        if token.quote == crate::QuoteKind::None && index >= redir_start {
            if let Some((fd, mode, inline_target, needs_target)) =
                parse_redirection_operator(&token.text)
            {
                let target = if let Some(inline_target) = inline_target {
                    inline_target
                } else if needs_target {
                    index += 1;
                    let Some(target_token) = tokens.get(index) else {
                        return Err(ShellError::parse(format!(
                            "missing target for redirection {}",
                            token.text
                        ))
                        .with_span(token.span));
                    };
                    if is_control_operator(&target_token.text)
                        || parse_redirection_operator(&target_token.text).is_some()
                    {
                        return Err(ShellError::parse(format!(
                            "missing target for redirection {}",
                            token.text
                        ))
                        .with_span(target_token.span));
                    }
                    RedirectionTarget::Word {
                        text: target_token.text.clone(),
                        quote: target_token.quote,
                        segments: target_token.segments.clone(),
                    }
                } else {
                    return Err(
                        ShellError::parse(format!("invalid redirection {}", token.text))
                            .with_span(token.span),
                    );
                };
                redirections.push(Redirection::new(fd, mode, target));
                index += 1;
                continue;
            }
        }

        if !saw_argv {
            if let Some((name, value)) = parse_assignment(&token.text) {
                assignments.push(Assignment::with_segments(
                    name,
                    value,
                    segments_after_prefix(&token.segments, name.len() + 1),
                ));
                index += 1;
                continue;
            }
        }
        saw_argv = true;
        argv.push(token.text.clone());
        argv_quote.push(token.quote);
        argv_segments.push(token.segments.clone());
        index += 1;
    }

    let span = match (tokens.first(), tokens.last()) {
        (Some(first), Some(last)) => Some(SourceSpan::new(first.span.start, last.span.end)),
        _ => None,
    };

    commands.push(CommandInvocation::new(
        assignments,
        argv,
        argv_quote,
        argv_segments,
        redirections,
        span,
    ));
    Ok(())
}

fn is_control_operator(text: &str) -> bool {
    matches!(text, "|" | ";" | "&&" | "||")
}

fn parse_assignment(text: &str) -> Option<(&str, &str)> {
    let (name, value) = text.split_once('=')?;
    is_assignment_target(name).then_some((name, value))
}

/// A valid assignment left-hand side: a plain identifier, an append (`name+`),
/// or an array element (`name[subscript]`, optionally `+`).
fn is_assignment_target(name: &str) -> bool {
    let base = name.strip_suffix('+').unwrap_or(name);
    if let Some(open) = base.find('[') {
        return base.ends_with(']') && open > 0 && is_identifier(&base[..open]);
    }
    is_identifier(base)
}

fn parse_redirection_operator(
    text: &str,
) -> Option<(u8, RedirectionMode, Option<RedirectionTarget>, bool)> {
    match text {
        "<" => Some((0, RedirectionMode::Read, None, true)),
        // `<>` opens fd 0 for reading and writing (POSIX 2.7.7). In agsh's
        // buffered redirection model only the read side (stdin from file, no
        // truncation) is realized; the write-back side and high-fd `exec`/`n<>`
        // forms are not (see docs/MILESTONE_POSIX.md).
        "<>" => Some((0, RedirectionMode::Read, None, true)),
        "<<" | "<<-" => Some((0, RedirectionMode::HereDoc, None, true)),
        "<<<" => Some((0, RedirectionMode::HereString, None, true)),
        ">" => Some((1, RedirectionMode::Write, None, true)),
        ">|" => Some((1, RedirectionMode::WriteClobber, None, true)),
        ">>" => Some((1, RedirectionMode::Append, None, true)),
        "&>" => Some((1, RedirectionMode::WriteBoth, None, true)),
        // `>&` / `<&` with no leading fd default to stdout / stdin. A bare
        // `>&`/`<&` redirects to a following word (both streams for `>&`).
        ">&" => Some((1, RedirectionMode::WriteBoth, None, true)),
        "<&" => Some((0, RedirectionMode::Read, None, true)),
        _ if text.starts_with(">&") => {
            let target = parse_fd_redirection_target(&text[2..])?;
            Some((1, RedirectionMode::DupFd, Some(target), false))
        }
        _ if text.starts_with("<&") => {
            let target = parse_fd_redirection_target(&text[2..])?;
            Some((0, RedirectionMode::DupFd, Some(target), false))
        }
        _ => {
            let (fd_text, rest) = split_leading_digits(text)?;
            let fd = fd_text.parse::<u8>().ok()?;
            match rest {
                ">" => Some((fd, RedirectionMode::Write, None, true)),
                ">|" => Some((fd, RedirectionMode::WriteClobber, None, true)),
                ">>" => Some((fd, RedirectionMode::Append, None, true)),
                "<" => Some((fd, RedirectionMode::Read, None, true)),
                _ if rest.starts_with(">&") || rest.starts_with("<&") => {
                    let target = parse_fd_redirection_target(&rest[2..])?;
                    Some((fd, RedirectionMode::DupFd, Some(target), false))
                }
                _ => None,
            }
        }
    }
}

fn parse_fd_redirection_target(text: &str) -> Option<RedirectionTarget> {
    if text == "-" {
        return Some(RedirectionTarget::Close);
    }
    Some(RedirectionTarget::Fd(text.parse::<u8>().ok()?))
}

fn split_leading_digits(text: &str) -> Option<(&str, &str)> {
    let digit_len = text
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .map(|(idx, c)| idx + c.len_utf8())
        .last()?;
    if digit_len == text.len() {
        return None;
    }
    Some(text.split_at(digit_len))
}

fn segments_after_prefix(segments: &[WordSegment], prefix_len: usize) -> Vec<WordSegment> {
    let mut remaining = prefix_len;
    let mut out = Vec::new();

    for segment in segments {
        if remaining >= segment.text.len() {
            remaining -= segment.text.len();
            continue;
        }
        if remaining > 0 {
            out.push(WordSegment::new(
                segment.text[remaining..].to_string(),
                segment.quote,
            ));
            remaining = 0;
        } else {
            out.push(segment.clone());
        }
    }

    if out.is_empty() {
        out.push(WordSegment::new(String::new(), crate::QuoteKind::None));
    }
    out
}

fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_external_command() {
        let graph = parse_line("echo hello").unwrap();
        assert_eq!(graph.pipeline.commands.len(), 1);
        assert_eq!(graph.pipeline.commands[0].argv, vec!["echo", "hello"]);
    }

    #[test]
    fn heredoc_body_after_a_preceding_command() {
        // Regression: a heredoc on any line (not just the first) must find its
        // body. Previously only the first line was scanned for heredoc ops.
        let (cmd, bodies) =
            extract_heredoc_bodies("echo first\ncat <<H\nbody\nH\necho after").unwrap();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].0, "body\n");
        assert_eq!(cmd, "echo first\ncat <<H\necho after");
    }

    #[test]
    fn multiple_heredocs_on_one_line() {
        let (_cmd, bodies) = extract_heredoc_bodies("cat <<A; cat <<B\naaa\nA\nbbb\nB").unwrap();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0].0, "aaa\n");
        assert_eq!(bodies[1].0, "bbb\n");
    }

    #[test]
    fn left_shift_is_not_a_heredoc() {
        let (cmd, bodies) = extract_heredoc_bodies("echo $((1 << 4))").unwrap();
        assert!(bodies.is_empty());
        assert_eq!(cmd, "echo $((1 << 4))");
    }

    #[test]
    fn parses_assignment() {
        let graph = parse_line("FOO=bar echo $FOO").unwrap();
        let cmd = &graph.pipeline.commands[0];
        assert_eq!(cmd.assignments[0].name, "FOO");
        assert_eq!(cmd.assignments[0].value, "bar");
        assert_eq!(cmd.argv, vec!["echo", "$FOO"]);
    }

    #[test]
    fn parses_pipeline() {
        let graph = parse_line("echo hello | cat").unwrap();
        assert_eq!(graph.pipeline.commands.len(), 2);
        assert!(!graph.pipeline.negated);
        assert_eq!(graph.pipeline.commands[0].argv[0], "echo");
        assert_eq!(graph.pipeline.commands[1].argv[0], "cat");
    }

    #[test]
    fn parses_negated_pipeline() {
        let graph = parse_line("! false | true").unwrap();
        assert!(graph.pipeline.negated);
        assert_eq!(graph.pipeline.commands.len(), 2);
        assert_eq!(graph.pipeline.commands[0].argv[0], "false");
        assert_eq!(graph.pipeline.commands[1].argv[0], "true");
    }

    #[test]
    fn parses_redirection_out_of_argv() {
        let graph = parse_line("echo hello > out").unwrap();
        let cmd = &graph.pipeline.commands[0];
        assert_eq!(cmd.argv, vec!["echo", "hello"]);
        assert_eq!(cmd.redirections.len(), 1);
        assert_eq!(cmd.redirections[0].fd, 1);
        assert_eq!(cmd.redirections[0].mode, RedirectionMode::Write);
    }

    #[test]
    fn parses_forced_clobber_redirections() {
        let graph = parse_line("echo hello >| out 2>| err").unwrap();
        let cmd = &graph.pipeline.commands[0];
        assert_eq!(cmd.argv, vec!["echo", "hello"]);
        assert_eq!(cmd.redirections.len(), 2);
        assert_eq!(cmd.redirections[0].fd, 1);
        assert_eq!(cmd.redirections[0].mode, RedirectionMode::WriteClobber);
        assert_eq!(cmd.redirections[1].fd, 2);
        assert_eq!(cmd.redirections[1].mode, RedirectionMode::WriteClobber);
    }

    #[test]
    fn parses_fd_close_redirections() {
        let graph = parse_line("echo hello 2>&- 0<&-").unwrap();
        let cmd = &graph.pipeline.commands[0];

        assert_eq!(cmd.argv, vec!["echo", "hello"]);
        assert_eq!(cmd.redirections.len(), 2);
        assert_eq!(cmd.redirections[0].fd, 2);
        assert_eq!(cmd.redirections[0].mode, RedirectionMode::DupFd);
        assert_eq!(cmd.redirections[0].target, RedirectionTarget::Close);
        assert_eq!(cmd.redirections[1].fd, 0);
        assert_eq!(cmd.redirections[1].mode, RedirectionMode::DupFd);
        assert_eq!(cmd.redirections[1].target, RedirectionTarget::Close);
    }

    #[test]
    fn parses_command_lists() {
        let graph = parse_line("false || echo fallback; true && echo ok").unwrap();
        assert_eq!(graph.pipeline.commands[0].argv, vec!["false"]);
        assert_eq!(graph.list.items.len(), 4);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[1].operator, ListOperator::Or);
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "fallback"]
        );
        assert_eq!(graph.list.items[2].operator, ListOperator::Always);
        assert_eq!(graph.list.items[3].operator, ListOperator::And);
        assert_eq!(
            graph.list.items[3].pipeline.commands[0].argv,
            vec!["echo", "ok"]
        );
    }

    #[test]
    fn keeps_function_body_lists_inside_definition() {
        let graph = parse_line("hi() { echo one; echo two || echo fallback; }; hi").unwrap();
        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "hi()");
        assert!(graph.list.items[0].pipeline.commands[0]
            .argv
            .iter()
            .any(|word| word == ";"));
        assert_eq!(graph.list.items[1].pipeline.commands[0].argv, vec!["hi"]);
    }

    #[test]
    fn keeps_if_block_lists_inside_invocation() {
        let graph = parse_line("if false; then echo no; else echo yes; fi; echo done").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "if");
        assert!(graph.list.items[0].pipeline.commands[0]
            .argv
            .iter()
            .any(|word| word == "else"));
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn if_block_depth_ignores_if_and_fi_arguments() {
        let graph = parse_line("if true; then echo if fi; fi; echo done").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "if");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn keeps_while_block_lists_inside_invocation() {
        let graph =
            parse_line("while false; do echo no || echo fallback; done; echo done").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "while");
        assert!(graph.list.items[0].pipeline.commands[0]
            .argv
            .iter()
            .any(|word| word == "do"));
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn keeps_pipeline_while_block_inside_pipeline_command() {
        let graph =
            parse_line("printf 'x\n' | while read line; do echo $line; done | wc -l").unwrap();

        assert_eq!(graph.list.items.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 3);
        assert_eq!(graph.list.items[0].pipeline.commands[1].argv[0], "while");
        assert!(graph.list.items[0].pipeline.commands[1]
            .argv
            .iter()
            .any(|word| word == "done"));
    }

    #[test]
    fn while_block_depth_ignores_while_and_done_arguments() {
        let graph = parse_line("while true; do echo while done; done; echo after").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "while");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "after"]
        );
    }

    #[test]
    fn keeps_until_block_lists_inside_invocation() {
        let graph = parse_line("until true; do echo no || echo fallback; done; echo done").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "until");
        assert!(graph.list.items[0].pipeline.commands[0]
            .argv
            .iter()
            .any(|word| word == "do"));
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn until_block_depth_ignores_until_and_done_arguments() {
        let graph = parse_line("until false; do echo until done; done; echo after").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "until");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "after"]
        );
    }

    #[test]
    fn keeps_for_block_lists_inside_invocation() {
        let graph =
            parse_line("for item in a b; do echo $item || echo fallback; done; echo done").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "for");
        assert!(graph.list.items[0].pipeline.commands[0]
            .argv
            .iter()
            .any(|word| word == "do"));
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn keeps_select_block_lists_inside_invocation() {
        let graph = parse_line(
            "select item in a b; do echo $REPLY:$item || echo fallback; done; echo done",
        )
        .unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "select");
        assert!(graph.list.items[0].pipeline.commands[0]
            .argv
            .iter()
            .any(|word| word == "do"));
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn keeps_pipeline_select_block_inside_pipeline_command() {
        let graph =
            parse_line("printf '2\n' | select item in a b; do echo $item; break; done | wc -l")
                .unwrap();

        assert_eq!(graph.list.items.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 3);
        assert_eq!(graph.list.items[0].pipeline.commands[1].argv[0], "select");
        assert!(graph.list.items[0].pipeline.commands[1]
            .argv
            .iter()
            .any(|word| word == "done"));
    }

    #[test]
    fn for_block_depth_ignores_for_and_done_arguments() {
        let graph = parse_line("for item in one; do echo for done; done; echo after").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "for");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "after"]
        );
    }

    #[test]
    fn select_block_depth_ignores_select_and_done_arguments() {
        let graph =
            parse_line("select item in one; do echo select done; done; echo after").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "select");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "after"]
        );
    }

    #[test]
    fn keeps_case_block_lists_inside_invocation() {
        let graph = parse_line(
            "case $kind in a) echo one ;; b) echo two || echo fallback ;; esac; echo done",
        )
        .unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].operator, ListOperator::Always);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "case");
        assert!(graph.list.items[0].pipeline.commands[0]
            .argv
            .iter()
            .any(|word| word == "esac"));
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn keeps_nested_case_blocks_inside_outer_case_invocation() {
        let graph = parse_line(
            "case outer in outer) case inner in inner) echo nested ;; esac; echo after ;; esac; echo done",
        )
        .unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "case");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn case_block_depth_ignores_case_and_esac_arguments() {
        let graph = parse_line("case x in x) echo case esac ;; esac; echo after").unwrap();

        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "case");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "after"]
        );
    }

    #[test]
    fn rejects_missing_commands_around_boolean_list_operators() {
        assert!(parse_line("&& echo bad").is_err());
        assert!(parse_line("echo bad ||").is_err());
    }

    #[test]
    fn rejects_missing_command_after_negation() {
        assert!(parse_line("!").is_err());
        assert!(parse_line("! ; echo bad").is_err());
    }

    #[test]
    fn preserves_assignment_value_segments() {
        let graph = parse_line("FOO=\"hello world\" sh").unwrap();
        let assignment = &graph.pipeline.commands[0].assignments[0];
        assert_eq!(assignment.value, "hello world");
        assert_eq!(assignment.value_segments[0].quote, crate::QuoteKind::Double);
    }

    #[test]
    fn groups_subshell_as_single_command() {
        let graph = parse_line("(echo a; echo b)").unwrap();
        assert_eq!(graph.list.items.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
        let argv = &graph.list.items[0].pipeline.commands[0].argv;
        assert_eq!(argv.first().map(String::as_str), Some("(echo"));
        assert!(argv.iter().any(|word| word == ";"));
        assert!(argv.last().is_some_and(|word| word.ends_with(')')));
    }

    #[test]
    fn groups_spaced_subshell_and_keeps_trailing_command() {
        let graph = parse_line("( echo a ) ; echo done").unwrap();
        assert_eq!(graph.list.items.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[0].argv[0], "(");
        assert_eq!(
            graph.list.items[1].pipeline.commands[0].argv,
            vec!["echo", "done"]
        );
    }

    #[test]
    fn subshell_paren_counting_ignores_command_substitution() {
        let graph = parse_line("( echo $(echo hi); echo b )").unwrap();
        assert_eq!(graph.list.items.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 1);
    }

    #[test]
    fn groups_brace_group_as_single_command() {
        let graph = parse_line("{ echo a; echo b; }").unwrap();
        assert_eq!(graph.list.items.len(), 1);
        let argv = &graph.list.items[0].pipeline.commands[0].argv;
        assert_eq!(argv.first().map(String::as_str), Some("{"));
        assert_eq!(argv.last().map(String::as_str), Some("}"));
    }

    #[test]
    fn is_incomplete_detects_continuation_cases() {
        // Complete commands.
        assert!(!is_incomplete("echo hello"));
        assert!(!is_incomplete("echo a; echo b"));
        assert!(!is_incomplete("if true; then echo hi; fi"));
        assert!(!is_incomplete("for i in 1 2; do echo $i; done"));
        assert!(!is_incomplete("echo a &"));
        assert!(!is_incomplete("(echo a)"));
        assert!(!is_incomplete("cat <<EOF\nbody\nEOF"));

        // Incomplete commands needing more input.
        assert!(is_incomplete("echo 'unterminated"));
        assert!(is_incomplete("echo \"open"));
        assert!(is_incomplete("if true; then echo hi"));
        assert!(is_incomplete("for i in 1 2; do echo $i"));
        assert!(is_incomplete("while true; do"));
        assert!(is_incomplete("case x in a) echo a"));
        assert!(is_incomplete("{ echo a"));
        assert!(is_incomplete("(echo a"));
        assert!(is_incomplete("echo a |"));
        assert!(is_incomplete("echo a &&"));
        assert!(is_incomplete("echo foo\\"));
        assert!(is_incomplete("cat <<EOF\nbody"));
    }

    #[test]
    fn subshell_in_pipeline_is_single_stage() {
        let graph = parse_line("(echo b; echo a) | sort").unwrap();
        assert_eq!(graph.list.items.len(), 1);
        assert_eq!(graph.list.items[0].pipeline.commands.len(), 2);
        assert_eq!(graph.list.items[0].pipeline.commands[1].argv, vec!["sort"]);
    }
}
