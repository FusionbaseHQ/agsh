//! Dependency-free syntax highlighting for source files in `view`/rich display.
//!
//! A single generic tokenizer (comments, strings, numbers, identifiers,
//! operators) is driven by a small per-language table (comment markers, string
//! delimiters, keyword set). It is not a full parser — it highlights the lexical
//! categories users expect (keywords, strings, comments, numbers, calls) for the
//! common languages, and preserves every byte of the source verbatim, only
//! inserting SGR. Themed via roles so it matches the rest of agsh.

use agsh_style::{Role, Theme};

/// The lexical rules for one language.
pub struct Language {
    pub name: &'static str,
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// String delimiters, longest first (e.g. `"""` before `"`). A delimiter
    /// longer than one byte (or a backtick) may span lines.
    strings: &'static [&'static str],
    keywords: &'static [&'static str],
    /// Match keywords case-insensitively (e.g. SQL).
    case_insensitive: bool,
}

/// Don't highlight beyond this size; above it, return the source unchanged.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Resolve a language from a file name's extension.
pub fn detect_language(name: &str) -> Option<&'static Language> {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let lang = match ext.as_str() {
        "py" | "pyw" | "pyi" => &PYTHON,
        "rs" => &RUST,
        "js" | "mjs" | "cjs" | "jsx" => &JAVASCRIPT,
        "ts" | "tsx" | "mts" | "cts" => &TYPESCRIPT,
        "go" => &GO,
        "c" | "h" => &C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => &CPP,
        "java" => &JAVA,
        "rb" => &RUBY,
        "sh" | "bash" | "zsh" | "ksh" => &SHELL,
        "sql" => &SQL,
        "toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" => &CONFIG,
        "lua" => &LUA,
        _ => return None,
    };
    Some(lang)
}

/// Highlight `source` as `lang`, returning the source with SGR color inserted.
pub fn highlight(source: &str, lang: &Language, theme: &Theme) -> String {
    if !theme.enabled() || source.len() > MAX_BYTES {
        return source.to_string();
    }
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len() + source.len() / 4);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];

        // Line comment.
        if let Some(marker) = lang.line_comments.iter().find(|m| matches_at(&chars, i, m)) {
            let start = i;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            paint_span(&mut out, theme, Role::Comment, &chars[start..i]);
            let _ = marker;
            continue;
        }

        // Block comment.
        if let Some((open, close)) = lang.block_comment {
            if matches_at(&chars, i, open) {
                let start = i;
                i += open.chars().count();
                while i < chars.len() && !matches_at(&chars, i, close) {
                    i += 1;
                }
                if i < chars.len() {
                    i += close.chars().count();
                }
                paint_span(&mut out, theme, Role::Comment, &chars[start..i]);
                continue;
            }
        }

        // String literal.
        if let Some(delim) = lang.strings.iter().find(|d| matches_at(&chars, i, d)) {
            let multiline = delim.chars().count() > 1 || *delim == "`";
            let start = i;
            i += delim.chars().count();
            while i < chars.len() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 2; // escape
                    continue;
                }
                if matches_at(&chars, i, delim) {
                    i += delim.chars().count();
                    break;
                }
                if chars[i] == '\n' && !multiline {
                    break; // unterminated single-line string
                }
                i += 1;
            }
            paint_span(&mut out, theme, Role::Str, &chars[start..i]);
            continue;
        }

        // Number.
        if c.is_ascii_digit() || (c == '.' && next_is_digit(&chars, i)) {
            let start = i;
            i = scan_number(&chars, i);
            paint_span(&mut out, theme, Role::Number, &chars[start..i]);
            continue;
        }

        // Identifier / keyword / call.
        if is_ident_start(c) {
            let start = i;
            while i < chars.len() && is_ident_part(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let role = if is_keyword(lang, &word) {
                Role::Keyword
            } else if next_nonspace_is(&chars, i, '(') {
                Role::Function
            } else {
                paint_plain(&mut out, &chars[start..i]);
                continue;
            };
            paint_span(&mut out, theme, role, &chars[start..i]);
            continue;
        }

        // Operators (a run of operator characters).
        if is_operator(c) {
            let start = i;
            while i < chars.len() && is_operator(chars[i]) {
                i += 1;
            }
            paint_span(&mut out, theme, Role::Operator, &chars[start..i]);
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

fn matches_at(chars: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    i + p.len() <= chars.len() && chars[i..i + p.len()] == p[..]
}

fn next_is_digit(chars: &[char], i: usize) -> bool {
    chars.get(i + 1).is_some_and(|c| c.is_ascii_digit())
}

fn next_nonspace_is(chars: &[char], mut i: usize, target: char) -> bool {
    while i < chars.len() && (chars[i] == ' ' || chars[i] == '\t') {
        i += 1;
    }
    chars.get(i) == Some(&target)
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_ident_part(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn is_operator(c: char) -> bool {
    matches!(
        c,
        '=' | '+' | '-' | '*' | '/' | '%' | '<' | '>' | '&' | '|' | '!' | '^' | '~'
    )
}

fn is_keyword(lang: &Language, word: &str) -> bool {
    if lang.case_insensitive {
        let lower = word.to_ascii_lowercase();
        lang.keywords.iter().any(|k| k.eq_ignore_ascii_case(&lower))
    } else {
        lang.keywords.contains(&word)
    }
}

/// Consume a numeric literal (int/float/hex/bin/oct, separators, exponent, suffix).
fn scan_number(chars: &[char], mut i: usize) -> usize {
    // 0x / 0b / 0o prefix.
    if chars.get(i) == Some(&'0') {
        if let Some(p) = chars.get(i + 1) {
            if matches!(p, 'x' | 'X' | 'b' | 'B' | 'o' | 'O') {
                i += 2;
                while chars
                    .get(i)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
                {
                    i += 1;
                }
                return i;
            }
        }
    }
    while chars
        .get(i)
        .is_some_and(|c| c.is_ascii_digit() || *c == '_' || *c == '.')
    {
        i += 1;
    }
    // Exponent.
    if matches!(chars.get(i), Some('e') | Some('E')) {
        i += 1;
        if matches!(chars.get(i), Some('+') | Some('-')) {
            i += 1;
        }
        while chars.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    }
    // Type suffix (f64, u8, L, ull, …).
    while chars
        .get(i)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
    {
        i += 1;
    }
    i
}

fn paint_span(out: &mut String, theme: &Theme, role: Role, span: &[char]) {
    let text: String = span.iter().collect();
    out.push_str(&theme.paint(role, &text));
}

fn paint_plain(out: &mut String, span: &[char]) {
    out.extend(span.iter());
}

// ---- language tables ------------------------------------------------------

macro_rules! lang {
    ($name:literal, line: $lc:expr, block: $bc:expr, str: $st:expr, kw: $kw:expr $(, ci: $ci:expr)?) => {
        Language {
            name: $name,
            line_comments: $lc,
            block_comment: $bc,
            strings: $st,
            keywords: $kw,
            case_insensitive: false $(|| $ci)?,
        }
    };
}

static PYTHON: Language = lang!("Python",
    line: &["#"], block: None, str: &["\"\"\"", "'''", "\"", "'"],
    kw: &["def","class","if","elif","else","for","while","return","import","from","as",
        "with","try","except","finally","raise","yield","lambda","pass","break","continue",
        "global","nonlocal","in","is","not","and","or","None","True","False","async","await",
        "del","assert","match","case","print"]);

static RUST: Language = lang!("Rust",
    line: &["//"], block: Some(("/*","*/")), str: &["\"","'"],
    kw: &["fn","let","mut","const","static","if","else","match","for","while","loop","return",
        "struct","enum","impl","trait","pub","use","mod","crate","self","Self","super","as","ref",
        "move","where","type","dyn","async","await","unsafe","extern","in","break","continue",
        "true","false","Some","None","Ok","Err","box"]);

static JAVASCRIPT: Language = lang!("JavaScript",
    line: &["//"], block: Some(("/*","*/")), str: &["`","\"","'"],
    kw: &["function","var","let","const","if","else","for","while","return","class","extends",
        "new","this","super","import","export","from","default","async","await","try","catch",
        "finally","throw","typeof","instanceof","in","of","switch","case","break","continue","do",
        "yield","null","undefined","true","false","void","delete"]);

static TYPESCRIPT: Language = lang!("TypeScript",
    line: &["//"], block: Some(("/*","*/")), str: &["`","\"","'"],
    kw: &["function","var","let","const","if","else","for","while","return","class","extends",
        "new","this","super","import","export","from","default","async","await","try","catch",
        "finally","throw","typeof","instanceof","in","of","switch","case","break","continue","do",
        "yield","null","undefined","true","false","void","delete","interface","type","enum",
        "implements","public","private","protected","readonly","namespace","declare","as","abstract",
        "keyof","satisfies"]);

static GO: Language = lang!("Go",
    line: &["//"], block: Some(("/*","*/")), str: &["`","\"","'"],
    kw: &["func","var","const","type","struct","interface","map","chan","package","import","if",
        "else","for","range","return","switch","case","default","go","defer","select","break",
        "continue","fallthrough","nil","true","false","iota"]);

static C: Language = lang!("C",
    line: &["//"], block: Some(("/*","*/")), str: &["\"","'"],
    kw: &["int","char","float","double","void","long","short","unsigned","signed","struct","union",
        "enum","typedef","const","static","extern","volatile","register","if","else","for","while",
        "do","switch","case","default","return","break","continue","sizeof","goto","inline",
        "true","false","NULL"]);

static CPP: Language = lang!("C++",
    line: &["//"], block: Some(("/*","*/")), str: &["\"","'"],
    kw: &["int","char","float","double","void","long","short","unsigned","signed","struct","union",
        "enum","typedef","const","constexpr","static","extern","if","else","for","while","do",
        "switch","case","default","return","break","continue","sizeof","goto","class","public",
        "private","protected","virtual","override","namespace","template","typename","new","delete",
        "this","true","false","nullptr","auto","using","friend","operator","explicit"]);

static JAVA: Language = lang!("Java",
    line: &["//"], block: Some(("/*","*/")), str: &["\"","'"],
    kw: &["class","interface","extends","implements","import","package","public","private",
        "protected","static","final","abstract","void","new","this","super","return","if","else",
        "for","while","do","switch","case","default","try","catch","finally","throw","throws",
        "break","continue","instanceof","enum","null","true","false","int","long","double","float",
        "boolean","char","byte","short","synchronized","volatile"]);

static RUBY: Language = lang!("Ruby",
    line: &["#"], block: None, str: &["\"","'"],
    kw: &["def","class","module","if","elsif","else","unless","while","until","for","do","end",
        "return","yield","begin","rescue","ensure","raise","then","case","when","nil","true","false",
        "self","require","require_relative","attr_accessor","attr_reader","attr_writer","puts","new",
        "and","or","not","in"]);

static SHELL: Language = lang!("Shell",
    line: &["#"], block: None, str: &["\"","'"],
    kw: &["if","then","else","elif","fi","for","while","until","do","done","case","esac","function",
        "return","in","select","local","export","readonly","declare","echo","exit","source"]);

static SQL: Language = lang!("SQL",
    line: &["--"], block: Some(("/*","*/")), str: &["'","\""],
    kw: &["select","from","where","insert","into","values","update","set","delete","create","table",
        "drop","alter","add","column","primary","key","foreign","references","join","inner","left",
        "right","outer","on","group","by","order","having","limit","offset","union","all","distinct",
        "as","and","or","not","null","is","in","like","between","exists","case","when","then","else",
        "end","count","sum","avg","min","max","index","view"],
    ci: true);

static CONFIG: Language = lang!("Config",
    line: &["#",";"], block: None, str: &["\"","'"],
    kw: &["true","false","null","yes","no","on","off"]);

static LUA: Language = lang!("Lua",
    line: &["--"], block: Some(("--[[","]]")), str: &["\"","'"],
    kw: &["function","local","if","then","else","elseif","end","for","while","do","repeat","until",
        "return","break","nil","true","false","and","or","not","in","goto"]);

#[cfg(test)]
mod tests {
    use super::*;
    use agsh_style::{ColorLevel, Icons, Palette};

    fn theme() -> Theme {
        Theme {
            palette: Palette::dark(),
            level: ColorLevel::TrueColor,
            icons: Icons::disabled(),
        }
    }

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(detect_language("main.py").map(|l| l.name), Some("Python"));
        assert_eq!(detect_language("lib.rs").map(|l| l.name), Some("Rust"));
        assert_eq!(
            detect_language("app.tsx").map(|l| l.name),
            Some("TypeScript")
        );
        assert_eq!(detect_language("q.SQL").map(|l| l.name), Some("SQL"));
        assert!(detect_language("notes.txt").is_none());
    }

    #[test]
    fn highlights_python_categories() {
        let t = theme();
        let src = "def greet(name):  # say hi\n    return \"hello \" + name  # 42\n";
        let out = highlight(src, &PYTHON, &t);
        assert!(out.contains(&t.paint(Role::Keyword, "def")), "keyword def");
        assert!(
            out.contains(&t.paint(Role::Function, "greet")),
            "call greet"
        );
        assert!(out.contains(&t.paint(Role::Str, "\"hello \"")), "string");
        assert!(out.contains(&t.paint(Role::Comment, "# say hi")), "comment");
        // Every source byte survives (only SGR inserted).
        let stripped = strip_sgr(&out);
        assert_eq!(stripped, src);
    }

    #[test]
    fn highlights_numbers_and_block_comments() {
        let t = theme();
        let src = "let x = 0xFF + 3.14e2; /* note */\n";
        let out = highlight(src, &RUST, &t);
        assert!(out.contains(&t.paint(Role::Keyword, "let")));
        assert!(out.contains(&t.paint(Role::Number, "0xFF")));
        assert!(out.contains(&t.paint(Role::Number, "3.14e2")));
        assert!(out.contains(&t.paint(Role::Comment, "/* note */")));
        assert_eq!(strip_sgr(&out), src);
    }

    #[test]
    fn plain_theme_leaves_source_unchanged() {
        let t = Theme::plain();
        let src = "def f(): pass\n";
        assert_eq!(highlight(src, &PYTHON, &t), src);
    }

    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c2 in chars.by_ref() {
                    if c2 == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
