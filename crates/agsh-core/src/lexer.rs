use crate::{ShellError, SourceSpan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    None,
    Single,
    Double,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WordSegment {
    pub text: String,
    pub quote: QuoteKind,
}

impl WordSegment {
    pub fn new(text: impl Into<String>, quote: QuoteKind) -> Self {
        Self {
            text: text.into(),
            quote,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub quote: QuoteKind,
    pub segments: Vec<WordSegment>,
    pub span: SourceSpan,
}

pub fn lex(input: &str) -> Result<Vec<Token>, ShellError> {
    let mut tokens = Vec::new();
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut i = 0;

    while i < chars.len() {
        let (start, ch) = chars[i];
        if ch == '\n' {
            // A newline separates commands, except right after a list/pipe
            // operator (or at the start), where it is a line continuation.
            let is_continuation = tokens
                .last()
                .is_none_or(|token: &Token| matches!(token.text.as_str(), ";" | "|" | "||" | "&&"));
            if !is_continuation {
                tokens.push(operator_token(";".to_string(), start, start + 1));
            }
            i += 1;
            continue;
        }
        if ch.is_whitespace() {
            i += 1;
            continue;
        }

        // A `#` at the start of a word begins a comment running to end of line.
        if ch == '#' {
            while i < chars.len() && chars[i].1 != '\n' {
                i += 1;
            }
            continue;
        }

        // Process substitution `<( ... )` / `>( ... )`: one token (before the
        // `<`/`>` redirection operators are recognized).
        if let Some((text, next_i, end)) = lex_process_substitution(&chars, i) {
            tokens.push(Token {
                text: text.clone(),
                quote: QuoteKind::None,
                segments: vec![WordSegment::new(text, QuoteKind::None)],
                span: SourceSpan::new(start, end),
            });
            i = next_i;
            continue;
        }

        if let Some((operator, next_i, end)) = lex_operator(&chars, i) {
            tokens.push(operator_token(operator, start, end));
            i = next_i;
            continue;
        }

        // `name=( ... )` array assignment: read as one token (balanced parens,
        // incl. spaces) so elements are not split into separate command words.
        if let Some((text, next_i, end)) = lex_array_assignment(&chars, i) {
            tokens.push(Token {
                text: text.clone(),
                quote: QuoteKind::None,
                segments: vec![WordSegment::new(text, QuoteKind::None)],
                span: SourceSpan::new(start, end),
            });
            i = next_i;
            continue;
        }

        // `(( ... ))` arithmetic: read as one token so the inner `;`/`<`/`>` are
        // arithmetic, not shell separators/redirections. (`$((...))` starts with
        // `$` and is handled by the word reader below.)
        if let Some((text, next_i, end)) = lex_double_paren(&chars, i) {
            tokens.push(Token {
                text: text.clone(),
                quote: QuoteKind::None,
                segments: vec![WordSegment::new(text, QuoteKind::None)],
                span: SourceSpan::new(start, end),
            });
            i = next_i;
            continue;
        }

        let token_start = start;
        let mut text = String::new();
        let mut segments = Vec::new();
        let mut end = start;

        while i < chars.len() {
            let (idx, c) = chars[i];

            // Extended-glob group `?(..)`/`*(..)`/`+(..)`/`@(..)`/`!(..)`: read
            // the balanced parens as part of the word so inner `|`/`)` are
            // pattern syntax, not pipe/separator operators.
            if matches!(c, '?' | '*' | '+' | '@' | '!')
                && chars.get(i + 1).map(|(_, c)| *c) == Some('(')
            {
                if let Some((group, next_i, group_end)) = read_extglob_group(&chars, i) {
                    text.push_str(&group);
                    segments.push(WordSegment::new(group, QuoteKind::None));
                    end = group_end;
                    i = next_i;
                    continue;
                }
            }

            if c.is_whitespace() || lex_operator(&chars, i).is_some() {
                break;
            }

            if c == '\'' {
                // Single quotes are fully literal: no escape or substitution.
                let mut segment = String::new();
                i += 1;
                end = idx + c.len_utf8();
                let mut closed = false;
                while i < chars.len() {
                    let (quoted_idx, quoted_char) = chars[i];
                    end = quoted_idx + quoted_char.len_utf8();
                    if quoted_char == '\'' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    segment.push(quoted_char);
                    i += 1;
                }
                if !closed {
                    return Err(ShellError::parse("unterminated quoted string")
                        .with_span(SourceSpan::new(idx, input.len())));
                }
                text.push_str(&segment);
                segments.push(WordSegment::new(segment, QuoteKind::Single));
                continue;
            }

            if c == '"' {
                // Double quotes allow expansion, but a backslash before $ ` " \
                // produces a literal character (and \<newline> is line
                // continuation). Escaped specials are emitted as Single
                // segments so later expansion treats them literally.
                let mut segment = String::new();
                let mut emitted_any = false;
                i += 1;
                end = idx + c.len_utf8();
                let mut closed = false;

                while i < chars.len() {
                    let (quoted_idx, quoted_char) = chars[i];
                    end = quoted_idx + quoted_char.len_utf8();
                    if quoted_char == '"' {
                        closed = true;
                        i += 1;
                        break;
                    }
                    if quoted_char == '\\' {
                        match chars.get(i + 1).map(|(_, c)| *c) {
                            Some('\n') => {
                                i += 2;
                                end = chars.get(i).map_or(end, |(ci, _)| *ci);
                                continue;
                            }
                            Some(next @ ('$' | '`' | '"' | '\\')) => {
                                if !segment.is_empty() {
                                    text.push_str(&segment);
                                    segments.push(WordSegment::new(
                                        std::mem::take(&mut segment),
                                        QuoteKind::Double,
                                    ));
                                }
                                text.push(next);
                                segments
                                    .push(WordSegment::new(next.to_string(), QuoteKind::Single));
                                emitted_any = true;
                                i += 2;
                                end = quoted_idx + '\\'.len_utf8() + next.len_utf8();
                                continue;
                            }
                            _ => {
                                segment.push('\\');
                                i += 1;
                                continue;
                            }
                        }
                    }
                    // A command substitution inside double quotes is consumed
                    // whole so its own quotes/parens do not close the string.
                    if quoted_char == '$' && chars.get(i + 1).is_some_and(|(_, c)| *c == '(') {
                        if let Some((substitution, next_i, substitution_end)) =
                            read_dollar_paren(&chars, i)
                        {
                            segment.push_str(&substitution);
                            end = substitution_end;
                            i = next_i;
                            continue;
                        }
                    }
                    if quoted_char == '$' && chars.get(i + 1).is_some_and(|(_, c)| *c == '{') {
                        if let Some((substitution, next_i, substitution_end)) =
                            read_dollar_brace(&chars, i)
                        {
                            segment.push_str(&substitution);
                            end = substitution_end;
                            i = next_i;
                            continue;
                        }
                    }
                    if quoted_char == '`' {
                        if let Some((substitution, next_i, substitution_end)) =
                            read_backtick_substitution(&chars, i)
                        {
                            segment.push_str(&substitution);
                            end = substitution_end;
                            i = next_i;
                            continue;
                        }
                    }
                    segment.push(quoted_char);
                    i += 1;
                }

                if !closed {
                    return Err(ShellError::parse("unterminated quoted string")
                        .with_span(SourceSpan::new(idx, input.len())));
                }

                if !segment.is_empty() || !emitted_any {
                    text.push_str(&segment);
                    segments.push(WordSegment::new(segment, QuoteKind::Double));
                }
                continue;
            }

            let mut segment = String::new();
            let mut end_word_after_segment = false;
            while i < chars.len() {
                let (plain_idx, plain_char) = chars[i];
                if plain_char.is_whitespace()
                    || plain_char == '\''
                    || plain_char == '"'
                    || lex_operator(&chars, i).is_some()
                {
                    break;
                }
                if plain_char == '$' && chars.get(i + 1).is_some_and(|(_, c)| *c == '(') {
                    if let Some((substitution, next_i, substitution_end)) =
                        read_dollar_paren(&chars, i)
                    {
                        segment.push_str(&substitution);
                        end = substitution_end;
                        i = next_i;
                        continue;
                    }
                }
                // Keep `${...}` (with internal spaces and nested braces) as one
                // segment so `${VAR:-a b}` is not split on the space.
                if plain_char == '$' && chars.get(i + 1).is_some_and(|(_, c)| *c == '{') {
                    if let Some((substitution, next_i, substitution_end)) =
                        read_dollar_brace(&chars, i)
                    {
                        segment.push_str(&substitution);
                        end = substitution_end;
                        i = next_i;
                        continue;
                    }
                }
                if plain_char == '`' {
                    let Some((substitution, next_i, substitution_end)) =
                        read_backtick_substitution(&chars, i)
                    else {
                        return Err(ShellError::parse("unterminated backtick substitution")
                            .with_span(SourceSpan::new(plain_idx, input.len())));
                    };
                    segment.push_str(&substitution);
                    end = substitution_end;
                    i = next_i;
                    continue;
                }
                if plain_char == '\\' {
                    // Backslash-newline is line continuation: drop both.
                    if chars.get(i + 1).is_some_and(|(_, c)| *c == '\n') {
                        i += 2;
                        continue;
                    }
                    if !segment.is_empty() {
                        text.push_str(&segment);
                        segments.push(WordSegment::new(
                            std::mem::take(&mut segment),
                            QuoteKind::None,
                        ));
                    }
                    i += 1;
                    if i < chars.len() {
                        let (_, escaped) = chars[i];
                        text.push(escaped);
                        segments.push(WordSegment::new(escaped.to_string(), QuoteKind::Single));
                        end = chars[i].0 + escaped.len_utf8();
                        i += 1;
                    }
                    continue;
                }
                // A function definition opener `name()` (with `(` immediately
                // followed by `)`) ends the word at `name()` so a following `{`
                // is a separate token, supporting `f(){ ... }` without a space.
                if plain_char == '('
                    && chars.get(i + 1).is_some_and(|(_, c)| *c == ')')
                    && text.is_empty()
                    && is_lex_identifier(&segment)
                {
                    segment.push('(');
                    segment.push(')');
                    end = plain_idx + 2;
                    i += 2;
                    end_word_after_segment = true;
                    break;
                }
                segment.push(plain_char);
                end = plain_idx + plain_char.len_utf8();
                i += 1;
            }

            if !segment.is_empty() {
                text.push_str(&segment);
                segments.push(WordSegment::new(segment, QuoteKind::None));
            }

            // A `name()` function opener ends the whole word so a following `{`
            // is a separate token.
            if end_word_after_segment {
                break;
            }
        }

        let quote = if segments.len() == 1 {
            segments[0].quote
        } else {
            QuoteKind::None
        };

        tokens.push(Token {
            text,
            quote,
            segments,
            span: SourceSpan::new(token_start, end),
        });
    }

    Ok(tokens)
}

/// Read an extended-glob group `X( ... )` (X in `?*+@!`) starting at `start`,
/// balancing nested parens and honoring quotes. Returns `(text, next_index,
/// end_offset)`.
fn read_extglob_group(chars: &[(usize, char)], start: usize) -> Option<(String, usize, usize)> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = start + 1; // skip the operator char; next is '('
    while i < chars.len() {
        let (idx, c) = chars[i];
        i += 1;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let text: String = chars[start..i].iter().map(|(_, c)| *c).collect();
                    return Some((text, i, idx + c.len_utf8()));
                }
            }
            _ => {}
        }
    }
    None
}

/// Read a process substitution `<( ... )` or `>( ... )` as one token (balanced
/// parens, honoring quotes), if `start` is at `<(` or `>(`.
fn lex_process_substitution(
    chars: &[(usize, char)],
    start: usize,
) -> Option<(String, usize, usize)> {
    let c0 = chars.get(start).map(|(_, c)| *c)?;
    if (c0 != '<' && c0 != '>') || chars.get(start + 1).map(|(_, c)| *c) != Some('(') {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = start + 1;
    while i < chars.len() {
        let (idx, c) = chars[i];
        i += 1;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let text: String = chars[start..i].iter().map(|(_, c)| *c).collect();
                    return Some((text, i, idx + c.len_utf8()));
                }
            }
            _ => {}
        }
    }
    None
}

/// Read a `name=( ... )` (or `name[i]=(…)`, `name+=(…)`) array assignment as a
/// single token, consuming the balanced parenthesized list including spaces.
/// Returns None unless the word is exactly an array-assignment prefix at `start`.
fn lex_array_assignment(chars: &[(usize, char)], start: usize) -> Option<(String, usize, usize)> {
    let mut i = start;
    let c0 = chars.get(i).map(|(_, c)| *c)?;
    if !(c0 == '_' || c0.is_ascii_alphabetic()) {
        return None;
    }
    i += 1;
    while chars
        .get(i)
        .is_some_and(|(_, c)| *c == '_' || c.is_ascii_alphanumeric())
    {
        i += 1;
    }
    // Optional `[subscript]`.
    if chars.get(i).map(|(_, c)| *c) == Some('[') {
        let mut depth = 0;
        while i < chars.len() {
            match chars[i].1 {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }
    if chars.get(i).map(|(_, c)| *c) == Some('+') {
        i += 1;
    }
    if chars.get(i).map(|(_, c)| *c) != Some('=') {
        return None;
    }
    i += 1;
    if chars.get(i).map(|(_, c)| *c) != Some('(') {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let (idx, c) = chars[i];
        i += 1;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let text: String = chars[start..i].iter().map(|(_, c)| *c).collect();
                    return Some((text, i, idx + c.len_utf8()));
                }
            }
            _ => {}
        }
    }
    None
}

/// Read a `(( ... ))` arithmetic group as one token if `start` is at `((`.
/// Returns `(text, next_index, end_offset)`, honoring quotes and nesting.
fn lex_double_paren(chars: &[(usize, char)], start: usize) -> Option<(String, usize, usize)> {
    if chars.get(start).map(|(_, c)| *c) != Some('(')
        || chars.get(start + 1).map(|(_, c)| *c) != Some('(')
    {
        return None;
    }
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut i = start;
    while i < chars.len() {
        let (idx, c) = chars[i];
        i += 1;
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let text: String = chars[start..i].iter().map(|(_, c)| *c).collect();
                    return Some((text, i, idx + c.len_utf8()));
                }
            }
            _ => {}
        }
    }
    None
}

fn lex_operator(chars: &[(usize, char)], i: usize) -> Option<(String, usize, usize)> {
    let (start, ch) = *chars.get(i)?;
    match ch {
        ';' => Some((";".to_string(), i + 1, start + ch.len_utf8())),
        '|' if chars.get(i + 1).is_some_and(|(_, c)| *c == '|') => {
            let end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
            Some(("||".to_string(), i + 2, end))
        }
        '|' => Some(("|".to_string(), i + 1, start + ch.len_utf8())),
        '&' if chars.get(i + 1).is_some_and(|(_, c)| *c == '&') => {
            let end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
            Some(("&&".to_string(), i + 2, end))
        }
        '<' => {
            if chars.get(i + 1).is_some_and(|(_, c)| *c == '<') {
                if chars.get(i + 2).is_some_and(|(_, c)| *c == '<') {
                    let end = chars[i + 2].0 + chars[i + 2].1.len_utf8();
                    Some(("<<<".to_string(), i + 3, end))
                } else if chars.get(i + 2).is_some_and(|(_, c)| *c == '-') {
                    let end = chars[i + 2].0 + chars[i + 2].1.len_utf8();
                    Some(("<<-".to_string(), i + 3, end))
                } else {
                    let end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
                    Some(("<<".to_string(), i + 2, end))
                }
            } else if chars.get(i + 1).is_some_and(|(_, c)| *c == '&') {
                lex_fd_dup(chars, i)
            } else if chars.get(i + 1).is_some_and(|(_, c)| *c == '>') {
                let end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
                Some(("<>".to_string(), i + 2, end))
            } else {
                Some(("<".to_string(), i + 1, start + ch.len_utf8()))
            }
        }
        '>' => {
            if chars.get(i + 1).is_some_and(|(_, c)| *c == '|') {
                let end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
                Some((">|".to_string(), i + 2, end))
            } else if chars.get(i + 1).is_some_and(|(_, c)| *c == '>') {
                let end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
                Some((">>".to_string(), i + 2, end))
            } else if chars.get(i + 1).is_some_and(|(_, c)| *c == '&') {
                lex_fd_dup(chars, i)
            } else {
                Some((">".to_string(), i + 1, start + ch.len_utf8()))
            }
        }
        '&' if chars.get(i + 1).is_some_and(|(_, c)| *c == '>') => {
            let end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
            Some(("&>".to_string(), i + 2, end))
        }
        '&' => Some(("&".to_string(), i + 1, start + ch.len_utf8())),
        c if c.is_ascii_digit() => {
            let mut j = i;
            while chars.get(j).is_some_and(|(_, c)| c.is_ascii_digit()) {
                j += 1;
            }
            if chars.get(j).is_some_and(|(_, c)| *c == '>' || *c == '<') {
                let mut end = chars[j].0 + chars[j].1.len_utf8();
                j += 1;
                if chars.get(j).is_some_and(|(_, c)| *c == '|') {
                    end = chars[j].0 + chars[j].1.len_utf8();
                    j += 1;
                }
                if chars.get(j).is_some_and(|(_, c)| *c == '>') {
                    end = chars[j].0 + chars[j].1.len_utf8();
                    j += 1;
                }
                if chars.get(j).is_some_and(|(_, c)| *c == '&') {
                    end = chars[j].0 + chars[j].1.len_utf8();
                    j += 1;
                    if chars.get(j).is_some_and(|(_, c)| *c == '-') {
                        end = chars[j].0 + chars[j].1.len_utf8();
                        j += 1;
                    } else {
                        while chars.get(j).is_some_and(|(_, c)| c.is_ascii_digit()) {
                            end = chars[j].0 + chars[j].1.len_utf8();
                            j += 1;
                        }
                    }
                }
                let operator = chars[i].0..end;
                let text = chars
                    .iter()
                    .filter(|(idx, _)| operator.contains(idx))
                    .map(|(_, c)| *c)
                    .collect::<String>();
                Some((text, j, end))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Lex a `>&`/`<&` file-descriptor duplication operator starting at `i` (the
/// `>`/`<`), consuming an inline `&N`/`&-` target if present. With no inline
/// target it yields `>&`/`<&`, which redirect to a following word.
fn lex_fd_dup(chars: &[(usize, char)], i: usize) -> Option<(String, usize, usize)> {
    let mut j = i + 2; // past the `>`/`<` and the `&`
    let mut end = chars[i + 1].0 + chars[i + 1].1.len_utf8();
    if chars.get(j).is_some_and(|(_, c)| *c == '-') {
        end = chars[j].0 + chars[j].1.len_utf8();
        j += 1;
    } else {
        while chars.get(j).is_some_and(|(_, c)| c.is_ascii_digit()) {
            end = chars[j].0 + chars[j].1.len_utf8();
            j += 1;
        }
    }
    let text = chars[i..j].iter().map(|(_, c)| *c).collect::<String>();
    Some((text, j, end))
}

fn operator_token(text: String, start: usize, end: usize) -> Token {
    Token {
        text,
        quote: QuoteKind::None,
        segments: Vec::new(),
        span: SourceSpan::new(start, end),
    }
}

fn is_lex_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn read_dollar_paren(chars: &[(usize, char)], start_i: usize) -> Option<(String, usize, usize)> {
    let mut depth = 0usize;
    // Track single/double quotes so metacharacters inside them don't end the
    // substitution early — e.g. `$(echo ')')`, `grep ')'`, `$(echo "a)b")`.
    let mut quote: Option<char> = None;
    let mut i = start_i + 1;
    while i < chars.len() {
        let (_, ch) = chars[i];
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => depth += 1,
            ')' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let end = chars[i].0 + ch.len_utf8();
                    let text = chars[start_i..=i].iter().map(|(_, c)| *c).collect();
                    return Some((text, i + 1, end));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Read a `${...}` parameter expansion, balancing nested braces so the whole
/// expansion (including internal spaces) stays a single word segment.
fn read_dollar_brace(chars: &[(usize, char)], start_i: usize) -> Option<(String, usize, usize)> {
    let mut depth = 0usize;
    // Track quotes so a `}` inside a quoted default value doesn't close the
    // expansion early — e.g. `${x:-'a}b'}`, `${x:-"a}b"}`.
    let mut quote: Option<char> = None;
    let mut i = start_i + 1;
    while i < chars.len() {
        let (_, ch) = chars[i];
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
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let end = chars[i].0 + ch.len_utf8();
                    let text = chars[start_i..=i].iter().map(|(_, c)| *c).collect();
                    return Some((text, i + 1, end));
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn read_backtick_substitution(
    chars: &[(usize, char)],
    start_i: usize,
) -> Option<(String, usize, usize)> {
    let mut i = start_i + 1;
    let mut escaped = false;
    while i < chars.len() {
        let (_, ch) = chars[i];
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '`' {
            let end = chars[i].0 + ch.len_utf8();
            let text = chars[start_i..=i].iter().map(|(_, c)| *c).collect();
            return Some((text, i + 1, end));
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_simple_command() {
        let tokens = lex("echo hello").unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].text, "echo");
        assert_eq!(tokens[1].text, "hello");
    }

    #[test]
    fn lex_dollar_paren_honors_quotes() {
        // SHIP_READINESS_PLAN P1-3: a quote inside $(…) must not end it early.
        for src in [
            "echo $(echo ')')",
            "echo $(echo \"a)b\")",
            "echo $(echo $(echo hi))", // nested still balances
        ] {
            let tokens = lex(src).unwrap();
            assert_eq!(tokens.len(), 2, "{src}: {tokens:?}");
            assert_eq!(tokens[1].text, &src["echo ".len()..]);
        }
    }

    #[test]
    fn lex_dollar_brace_honors_quotes() {
        for src in ["echo ${x:-'a}b'}", "echo ${x:-\"a}b\"}"] {
            let tokens = lex(src).unwrap();
            assert_eq!(tokens.len(), 2, "{src}: {tokens:?}");
            assert_eq!(tokens[1].text, &src["echo ".len()..]);
        }
    }

    #[test]
    fn lex_pipeline() {
        let tokens = lex("echo hello | cat").unwrap();
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["echo", "hello", "|", "cat"]
        );
    }

    #[test]
    fn lex_command_list_operators() {
        let tokens = lex("true && echo ok || echo fallback; echo done").unwrap();
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["true", "&&", "echo", "ok", "||", "echo", "fallback", ";", "echo", "done"]
        );
    }

    #[test]
    fn lex_quotes() {
        let tokens = lex("echo 'hello world'").unwrap();
        assert_eq!(tokens[1].text, "hello world");
        assert_eq!(tokens[1].quote, QuoteKind::Single);
    }

    #[test]
    fn lex_mixed_quotes_as_one_word() {
        let tokens = lex("echo a\" b\"'$C'").unwrap();
        assert_eq!(tokens[1].text, "a b$C");
        assert_eq!(
            tokens[1]
                .segments
                .iter()
                .map(|segment| segment.quote)
                .collect::<Vec<_>>(),
            vec![QuoteKind::None, QuoteKind::Double, QuoteKind::Single]
        );
    }

    #[test]
    fn lex_unquoted_escapes_as_literal_segments() {
        let tokens = lex(r#"echo a\*b\$C\ b"#).unwrap();
        assert_eq!(tokens[1].text, "a*b$C b");
        assert_eq!(
            tokens[1]
                .segments
                .iter()
                .map(|segment| (segment.text.as_str(), segment.quote))
                .collect::<Vec<_>>(),
            vec![
                ("a", QuoteKind::None),
                ("*", QuoteKind::Single),
                ("b", QuoteKind::None),
                ("$", QuoteKind::Single),
                ("C", QuoteKind::None),
                (" ", QuoteKind::Single),
                ("b", QuoteKind::None),
            ]
        );
    }

    #[test]
    fn lex_redirections() {
        let tokens = lex("printf hi > out 2>&1 2>&- 0<&-").unwrap();
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["printf", "hi", ">", "out", "2>&1", "2>&-", "0<&-"]
        );
    }

    #[test]
    fn lex_forced_clobber_redirections() {
        let tokens = lex("printf hi >| out 2>| err").unwrap();
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["printf", "hi", ">|", "out", "2>|", "err"]
        );
    }

    #[test]
    fn lex_unquoted_command_substitution_as_one_word() {
        let tokens = lex("echo $(printf %s hi)").unwrap();
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["echo", "$(printf %s hi)"]
        );
    }

    #[test]
    fn lex_unquoted_backtick_substitution_as_one_word() {
        let tokens = lex("echo `printf %s hi`").unwrap();
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["echo", "`printf %s hi`"]
        );
        assert_eq!(
            tokens[1].segments,
            vec![WordSegment::new("`printf %s hi`", QuoteKind::None)]
        );
    }

    #[test]
    fn rejects_unterminated_backtick_substitution() {
        assert!(lex("echo `printf %s hi").is_err());
    }

    #[test]
    fn lex_double_quote_escapes_specials_as_literal_segments() {
        let tokens = lex(r#"echo "\$HOME\`x\"\\""#).unwrap();
        assert_eq!(tokens[1].text, r#"$HOME`x"\"#);
        // The escaped $ must be a literal (Single) segment so it does not expand.
        assert!(tokens[1]
            .segments
            .iter()
            .any(|seg| seg.text == "$" && seg.quote == QuoteKind::Single));
    }

    #[test]
    fn lex_double_quote_keeps_backslash_before_plain_char() {
        let tokens = lex(r#"echo "a\tb""#).unwrap();
        assert_eq!(tokens[1].text, r"a\tb");
        assert_eq!(tokens[1].quote, QuoteKind::Double);
    }

    #[test]
    fn lex_line_continuation_joins_words() {
        let tokens = lex("echo a\\\nb").unwrap();
        assert_eq!(
            tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
            vec!["echo", "ab"]
        );
    }

    #[test]
    fn lex_line_continuation_inside_double_quotes() {
        let tokens = lex("echo \"a\\\nb\"").unwrap();
        assert_eq!(tokens[1].text, "ab");
    }

    #[test]
    fn lex_empty_double_quote_is_preserved() {
        let tokens = lex(r#"echo """#).unwrap();
        assert_eq!(tokens[1].text, "");
        assert_eq!(tokens[1].segments.len(), 1);
        assert_eq!(tokens[1].segments[0].quote, QuoteKind::Double);
    }
}
