//! Floating-point arithmetic for the `math` builtin and float-aware `$(( ))`.
//!
//! bash has no floating-point arithmetic at all; this gives agsh a real
//! calculator (a zsh `zsh/mathfunc`-style superset). Integer `$(( ))` stays on
//! the existing i64 evaluator — this is used only when an expression looks
//! floating-point (contains `.`, a float exponent, or a math function/constant).

/// A variable lookup for identifiers used in a float expression.
pub trait FloatVars {
    fn get(&self, name: &str) -> Option<f64>;
}

/// Whether `expr` should be evaluated as floating point: it contains a decimal
/// point, a scientific exponent, or a known math function / constant.
pub fn looks_floating(expr: &str) -> bool {
    let bytes = expr.as_bytes();
    // A `.` between digits, or `<digit>e<sign><digit>` exponent.
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'.' {
            let prev_digit = i > 0 && bytes[i - 1].is_ascii_digit();
            let next_digit = i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit();
            if prev_digit || next_digit {
                return true;
            }
        }
    }
    // Any known function/constant name as a whole identifier.
    let mut ident = String::new();
    let flush = |ident: &str| !ident.is_empty() && (is_math_fn(ident) || is_math_const(ident));
    for ch in expr.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            ident.push(ch);
        } else {
            if flush(&ident) {
                return true;
            }
            ident.clear();
        }
    }
    flush(&ident)
}

fn is_math_const(name: &str) -> bool {
    matches!(name, "pi" | "e" | "PI" | "E")
}

fn is_math_fn(name: &str) -> bool {
    matches!(
        name,
        "sqrt"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "exp"
            | "log"
            | "ln"
            | "log2"
            | "log10"
            | "pow"
            | "abs"
            | "floor"
            | "ceil"
            | "round"
            | "trunc"
            | "fmod"
            | "hypot"
            | "min"
            | "max"
            | "int"
    )
}

/// Evaluate a floating-point expression, resolving identifiers via `vars`.
pub fn eval(expr: &str, vars: &dyn FloatVars) -> Result<f64, String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut p = Parser {
        chars: &chars,
        pos: 0,
        vars,
        depth: 0,
    };
    p.skip_ws();
    let v = p.expr()?;
    p.skip_ws();
    if p.pos != p.chars.len() {
        return Err(format!("unexpected `{}`", p.chars[p.pos]));
    }
    Ok(v)
}

/// Format a float for shell output: integral values print without a decimal,
/// others print trimmed to a stable precision.
pub fn format_result(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "nan".to_string()
        } else if x > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if x == x.trunc() && x.abs() < 1e15 {
        return format!("{}", x as i64);
    }
    // Up to 10 significant decimals, trailing zeros trimmed.
    let mut s = format!("{x:.10}");
    while s.contains('.') && (s.ends_with('0') || s.ends_with('.')) {
        s.pop();
    }
    s
}

struct Parser<'a> {
    chars: &'a [char],
    pos: usize,
    vars: &'a dyn FloatVars,
    /// Active recursion depth, to reject pathological nesting before it
    /// overflows the stack (e.g. `((((…))))` or `~~~…`).
    depth: usize,
}

/// Max recursive-descent depth. Each nesting level adds a few frames; this keeps
/// the worst case well under the main-thread stack while allowing any real
/// expression.
const MAX_DEPTH: usize = 512;

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.chars.len() && self.chars[self.pos].is_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, s: &str) -> bool {
        let target: Vec<char> = s.chars().collect();
        if self.chars[self.pos..].starts_with(&target) {
            self.pos += target.len();
            true
        } else {
            false
        }
    }

    /// Run `body` one recursion level deeper, erroring if too deeply nested.
    fn guarded(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<f64, String>,
    ) -> Result<f64, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            self.depth -= 1;
            return Err("expression nested too deeply".to_string());
        }
        let r = body(self);
        self.depth -= 1;
        r
    }

    // expr = term (('+' | '-') term)*
    fn expr(&mut self) -> Result<f64, String> {
        self.guarded(Self::expr_inner)
    }

    fn expr_inner(&mut self) -> Result<f64, String> {
        let mut value = self.term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('+') => {
                    self.pos += 1;
                    value += self.term()?;
                }
                Some('-') => {
                    self.pos += 1;
                    value -= self.term()?;
                }
                _ => return Ok(value),
            }
        }
    }

    // term = power (('*' | '/' | '%') power)*
    fn term(&mut self) -> Result<f64, String> {
        let mut value = self.power()?;
        loop {
            self.skip_ws();
            // `**` is exponentiation, handled in power(); don't consume it here.
            if self.chars[self.pos..].starts_with(&['*', '*']) {
                return Ok(value);
            }
            match self.peek() {
                Some('*') => {
                    self.pos += 1;
                    value *= self.power()?;
                }
                Some('/') => {
                    self.pos += 1;
                    value /= self.power()?;
                }
                Some('%') => {
                    self.pos += 1;
                    value %= self.power()?;
                }
                _ => return Ok(value),
            }
        }
    }

    // power = unary ('**' power)?
    fn power(&mut self) -> Result<f64, String> {
        self.guarded(Self::power_inner)
    }

    fn power_inner(&mut self) -> Result<f64, String> {
        let base = self.unary()?;
        self.skip_ws();
        if self.eat("**") {
            let exp = self.power()?;
            return Ok(base.powf(exp));
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<f64, String> {
        self.guarded(Self::unary_inner)
    }

    fn unary_inner(&mut self) -> Result<f64, String> {
        self.skip_ws();
        match self.peek() {
            Some('-') => {
                self.pos += 1;
                Ok(-self.unary()?)
            }
            Some('+') => {
                self.pos += 1;
                self.unary()
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> Result<f64, String> {
        self.skip_ws();
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let v = self.expr()?;
                self.skip_ws();
                if !self.eat(")") {
                    return Err("expected `)`".to_string());
                }
                Ok(v)
            }
            Some(c) if c.is_ascii_digit() || c == '.' => self.number(),
            Some(c) if c.is_ascii_alphabetic() || c == '_' => self.ident(),
            Some(c) => Err(format!("unexpected `{c}`")),
            None => Err("unexpected end of expression".to_string()),
        }
    }

    fn number(&mut self) -> Result<f64, String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '.' {
                self.pos += 1;
            } else if (c == 'e' || c == 'E')
                && self
                    .chars
                    .get(self.pos + 1)
                    .is_some_and(|n| n.is_ascii_digit() || *n == '+' || *n == '-')
            {
                self.pos += 2;
            } else {
                break;
            }
        }
        let s: String = self.chars[start..self.pos].iter().collect();
        s.parse::<f64>()
            .map_err(|_| format!("invalid number `{s}`"))
    }

    fn ident(&mut self) -> Result<f64, String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        self.skip_ws();
        if self.peek() == Some('(') {
            self.pos += 1;
            let mut args = Vec::new();
            self.skip_ws();
            if self.peek() != Some(')') {
                loop {
                    args.push(self.expr()?);
                    self.skip_ws();
                    if self.eat(",") {
                        continue;
                    }
                    break;
                }
            }
            if !self.eat(")") {
                return Err("expected `)`".to_string());
            }
            return apply_fn(&name, &args);
        }
        match name.as_str() {
            "pi" | "PI" => Ok(std::f64::consts::PI),
            "e" | "E" => Ok(std::f64::consts::E),
            _ => self
                .vars
                .get(&name)
                .ok_or_else(|| format!("unknown name `{name}`")),
        }
    }
}

fn apply_fn(name: &str, args: &[f64]) -> Result<f64, String> {
    let one = |a: &[f64]| -> Result<f64, String> {
        a.first()
            .copied()
            .ok_or_else(|| format!("{name}: expected 1 argument"))
    };
    let two = |a: &[f64]| -> Result<(f64, f64), String> {
        match a {
            [x, y, ..] => Ok((*x, *y)),
            _ => Err(format!("{name}: expected 2 arguments")),
        }
    };
    Ok(match name {
        "sqrt" => one(args)?.sqrt(),
        "sin" => one(args)?.sin(),
        "cos" => one(args)?.cos(),
        "tan" => one(args)?.tan(),
        "asin" => one(args)?.asin(),
        "acos" => one(args)?.acos(),
        "atan" => one(args)?.atan(),
        "atan2" => {
            let (y, x) = two(args)?;
            y.atan2(x)
        }
        "exp" => one(args)?.exp(),
        "log" | "ln" => one(args)?.ln(),
        "log2" => one(args)?.log2(),
        "log10" => one(args)?.log10(),
        "pow" => {
            let (b, x) = two(args)?;
            b.powf(x)
        }
        "abs" => one(args)?.abs(),
        "floor" => one(args)?.floor(),
        "ceil" => one(args)?.ceil(),
        "round" => one(args)?.round(),
        "trunc" | "int" => one(args)?.trunc(),
        "fmod" => {
            let (a, b) = two(args)?;
            a % b
        }
        "hypot" => {
            let (a, b) = two(args)?;
            a.hypot(b)
        }
        "min" => args.iter().copied().fold(f64::INFINITY, f64::min),
        "max" => args.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        _ => return Err(format!("unknown function `{name}`")),
    })
}
