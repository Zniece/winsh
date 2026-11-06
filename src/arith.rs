//! POSIX arithmetic evaluation — `$(( ... ))`.
//!
//! Three rules are easy to get wrong and are stated here explicitly: every
//! operation is `wrapping_*` — arithmetic overflow must never kill the shell,
//! so it wraps; division and modulo by zero are errors; `**` with a negative
//! exponent is an error.
//!
//! Supported: integers, variables (with recursive dereference), the full
//! precedence table, assignment operators, `++`/`--`, and `?:`.

use crate::var::Vars;

const MAX_DEREF_DEPTH: u32 = 64;

pub fn eval(expr: &str, vars: &mut Vars) -> Result<i64, String> {
    let toks = tokenize(expr)?;
    let mut p = Parser {
        toks,
        pos: 0,
        vars,
    };
    let v = p.assignment()?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected trailing input in arithmetic: {expr}"));
    }
    Ok(v)
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(i64),
    Ident(String),
    Op(String),
}

fn tokenize(src: &str) -> Result<Vec<Tok>, String> {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            // 0x hex, 0 octal, else decimal.
            if c == '0' && i + 1 < b.len() && (b[i + 1] == 'x' || b[i + 1] == 'X') {
                i += 2;
                let hs = i;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
                if i == hs {
                    return Err("malformed hex literal".into());
                }
                let s: String = b[hs..i].iter().collect();
                out.push(Tok::Num(
                    i64::from_str_radix(&s, 16).map_err(|e| e.to_string())?,
                ));
            } else {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                let s: String = b[start..i].iter().collect();
                let v = if s.len() > 1 && s.starts_with('0') {
                    i64::from_str_radix(&s[1..], 8).unwrap_or(0)
                } else {
                    s.parse::<i64>().unwrap_or(0)
                };
                out.push(Tok::Num(v));
            }
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
            continue;
        }
        // Longest-match operators first.
        const OPS: &[&str] = &[
            "<<=", ">>=", "**", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "++", "--", "+=",
            "-=", "*=", "/=", "%=", "&=", "^=", "|=", "+", "-", "*", "/", "%", "<", ">", "=", "!",
            "~", "&", "^", "|", "?", ":", "(", ")",
        ];
        let rest: String = b[i..].iter().collect();
        let mut matched = false;
        for op in OPS {
            if rest.starts_with(op) {
                out.push(Tok::Op((*op).to_string()));
                i += op.chars().count();
                matched = true;
                break;
            }
        }
        if !matched {
            return Err(format!("unexpected character in arithmetic: {c}"));
        }
    }
    Ok(out)
}

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    vars: &'a mut Vars,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn eat_op(&mut self, op: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Op(o)) if o == op) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_op(&mut self, op: &str) -> Result<(), String> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(format!("expected '{op}'"))
        }
    }

    /// Reads a variable's value as an integer, dereferencing chains like
    /// `a=b; b=1` up to a fixed depth.
    fn var_value(&mut self, name: &str) -> i64 {
        let mut cur = name.to_string();
        for _ in 0..MAX_DEREF_DEPTH {
            let Some(raw) = self.vars.get(&cur) else {
                return 0;
            };
            let raw = raw.trim().to_string();
            if raw.is_empty() {
                return 0;
            }
            if let Ok(n) = raw.parse::<i64>() {
                return n;
            }
            if raw.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
                && raw.chars().all(|c| c.is_alphanumeric() || c == '_')
            {
                cur = raw;
                continue;
            }
            // An expression-valued variable: evaluate it.
            if let Ok(toks) = tokenize(&raw) {
                let mut p = Parser {
                    toks,
                    pos: 0,
                    vars: self.vars,
                };
                if let Ok(v) = p.assignment() {
                    return v;
                }
            }
            return 0;
        }
        0
    }

    fn assignment(&mut self) -> Result<i64, String> {
        // Look ahead for `NAME op= rhs`.
        if let (Some(Tok::Ident(name)), Some(Tok::Op(op))) = (self.peek().cloned(), self.toks.get(self.pos + 1).cloned()) {
            let assign_op = match op.as_str() {
                "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "^=" | "|=" => {
                    Some(op)
                }
                _ => None,
            };
            if let Some(aop) = assign_op {
                self.pos += 2;
                let rhs = self.assignment()?;
                let value = if aop == "=" {
                    rhs
                } else {
                    let cur = self.var_value(&name);
                    let l = cur;
                    let r = rhs;
                    match aop.as_str() {
                        "+=" => l.wrapping_add(r),
                        "-=" => l.wrapping_sub(r),
                        "*=" => l.wrapping_mul(r),
                        "/=" => {
                            if r == 0 {
                                return Err("division by zero".into());
                            }
                            l.wrapping_div(r)
                        }
                        "%=" => {
                            if r == 0 {
                                return Err("division by zero".into());
                            }
                            l.wrapping_rem(r)
                        }
                        "<<=" => l.wrapping_shl(r as u32),
                        ">>=" => l.wrapping_shr(r as u32),
                        "&=" => l & r,
                        "^=" => l ^ r,
                        "|=" => l | r,
                        _ => unreachable!(),
                    }
                };
                self.vars.set(&name, value.to_string());
                return Ok(value);
            }
        }
        self.ternary()
    }

    fn ternary(&mut self) -> Result<i64, String> {
        let cond = self.logical_or()?;
        if self.eat_op("?") {
            let a = self.assignment()?;
            self.expect_op(":")?;
            let b = self.ternary()?;
            return Ok(if cond != 0 { a } else { b });
        }
        Ok(cond)
    }

    fn logical_or(&mut self) -> Result<i64, String> {
        let mut l = self.logical_and()?;
        while self.eat_op("||") {
            let r = self.logical_and()?;
            l = i64::from(l != 0 || r != 0);
        }
        Ok(l)
    }

    fn logical_and(&mut self) -> Result<i64, String> {
        let mut l = self.bit_or()?;
        while self.eat_op("&&") {
            let r = self.bit_or()?;
            l = i64::from(l != 0 && r != 0);
        }
        Ok(l)
    }

    fn bit_or(&mut self) -> Result<i64, String> {
        let mut l = self.bit_xor()?;
        while self.eat_op("|") {
            l |= self.bit_xor()?;
        }
        Ok(l)
    }

    fn bit_xor(&mut self) -> Result<i64, String> {
        let mut l = self.bit_and()?;
        while self.eat_op("^") {
            l ^= self.bit_and()?;
        }
        Ok(l)
    }

    fn bit_and(&mut self) -> Result<i64, String> {
        let mut l = self.equality()?;
        while self.eat_op("&") {
            l &= self.equality()?;
        }
        Ok(l)
    }

    fn equality(&mut self) -> Result<i64, String> {
        let mut l = self.relational()?;
        loop {
            if self.eat_op("==") {
                l = i64::from(l == self.relational()?);
            } else if self.eat_op("!=") {
                l = i64::from(l != self.relational()?);
            } else {
                return Ok(l);
            }
        }
    }

    fn relational(&mut self) -> Result<i64, String> {
        let mut l = self.shift()?;
        loop {
            if self.eat_op("<=") {
                l = i64::from(l <= self.shift()?);
            } else if self.eat_op(">=") {
                l = i64::from(l >= self.shift()?);
            } else if self.eat_op("<") {
                l = i64::from(l < self.shift()?);
            } else if self.eat_op(">") {
                l = i64::from(l > self.shift()?);
            } else {
                return Ok(l);
            }
        }
    }

    fn shift(&mut self) -> Result<i64, String> {
        let mut l = self.additive()?;
        loop {
            if self.eat_op("<<") {
                l = l.wrapping_shl(self.additive()? as u32);
            } else if self.eat_op(">>") {
                l = l.wrapping_shr(self.additive()? as u32);
            } else {
                return Ok(l);
            }
        }
    }

    fn additive(&mut self) -> Result<i64, String> {
        let mut l = self.multiplicative()?;
        loop {
            if self.eat_op("+") {
                l = l.wrapping_add(self.multiplicative()?);
            } else if self.eat_op("-") {
                l = l.wrapping_sub(self.multiplicative()?);
            } else {
                return Ok(l);
            }
        }
    }

    fn multiplicative(&mut self) -> Result<i64, String> {
        let mut l = self.power()?;
        loop {
            if self.eat_op("*") {
                l = l.wrapping_mul(self.power()?);
            } else if self.eat_op("/") {
                let r = self.power()?;
                if r == 0 {
                    return Err("division by zero".into());
                }
                l = l.wrapping_div(r);
            } else if self.eat_op("%") {
                let r = self.power()?;
                if r == 0 {
                    return Err("division by zero".into());
                }
                l = l.wrapping_rem(r);
            } else {
                return Ok(l);
            }
        }
    }

    /// `**` is right-associative. Unary minus binds tighter on the left, so
    /// `-2**2` is `(-2)**2` = 4.
    fn power(&mut self) -> Result<i64, String> {
        // The base is a unary expression: unary -/+ bind tighter than `**`
        // on the left. The exponent recurses through power, which keeps `**`
        // right-associative and still lets `2**-1` reach the
        // negative-exponent error.
        let base = self.unary()?;
        if self.eat_op("**") {
            let exp = self.power()?;
            if exp < 0 {
                return Err("exponent less than 0".into());
            }
            return Ok(wrapping_pow(base, exp as u32));
        }
        Ok(base)
    }

    fn unary(&mut self) -> Result<i64, String> {
        if self.eat_op("!") {
            return Ok(i64::from(self.unary()? == 0));
        }
        if self.eat_op("~") {
            return Ok(!self.unary()?);
        }
        if self.eat_op("-") {
            return Ok(self.unary()?.wrapping_neg());
        }
        if self.eat_op("+") {
            return self.unary();
        }
        // Prefix increment / decrement.
        if self.eat_op("++") {
            if let Some(Tok::Ident(name)) = self.peek().cloned() {
                self.pos += 1;
                let v = self.var_value(&name).wrapping_add(1);
                self.vars.set(&name, v.to_string());
                return Ok(v);
            }
            return Err("++ requires a variable".into());
        }
        if self.eat_op("--") {
            if let Some(Tok::Ident(name)) = self.peek().cloned() {
                self.pos += 1;
                let v = self.var_value(&name).wrapping_sub(1);
                self.vars.set(&name, v.to_string());
                return Ok(v);
            }
            return Err("-- requires a variable".into());
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<i64, String> {
        match self.peek().cloned() {
            Some(Tok::Num(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(Tok::Ident(name)) => {
                self.pos += 1;
                // Postfix increment / decrement.
                if self.eat_op("++") {
                    let v = self.var_value(&name);
                    self.vars.set(&name, v.wrapping_add(1).to_string());
                    return Ok(v);
                }
                if self.eat_op("--") {
                    let v = self.var_value(&name);
                    self.vars.set(&name, v.wrapping_sub(1).to_string());
                    return Ok(v);
                }
                Ok(self.var_value(&name))
            }
            Some(Tok::Op(o)) if o == "(" => {
                self.pos += 1;
                let v = self.assignment()?;
                self.expect_op(")")?;
                Ok(v)
            }
            other => Err(format!("unexpected token in arithmetic: {other:?}")),
        }
    }
}

fn wrapping_pow(base: i64, exp: u32) -> i64 {
    let mut acc: i64 = 1;
    for _ in 0..exp {
        acc = acc.wrapping_mul(base);
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(expr: &str) -> i64 {
        let mut v = Vars::from_env("t".into(), vec![]);
        eval(expr, &mut v).unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    fn evv(expr: &str, vars: &mut Vars) -> i64 {
        eval(expr, vars).unwrap_or_else(|e| panic!("{expr}: {e}"))
    }

    #[test]
    fn literals_and_bases() {
        assert_eq!(ev("42"), 42);
        assert_eq!(ev("0x10"), 16);
        assert_eq!(ev("010"), 8);
    }

    #[test]
    fn precedence() {
        assert_eq!(ev("1 + 2 * 3"), 7);
        assert_eq!(ev("(1 + 2) * 3"), 9);
        assert_eq!(ev("2 ** 3 ** 2"), 512); // right associative
        assert_eq!(ev("-2 ** 2"), 4); // unary - binds tighter than **
    }

    #[test]
    fn comparison_and_logic_yield_1_or_0() {
        assert_eq!(ev("3 > 2"), 1);
        assert_eq!(ev("3 < 2"), 0);
        assert_eq!(ev("1 && 0"), 0);
        assert_eq!(ev("1 || 0"), 1);
        assert_eq!(ev("!0"), 1);
    }

    #[test]
    fn ternary() {
        assert_eq!(ev("1 ? 10 : 20"), 10);
        assert_eq!(ev("0 ? 10 : 20"), 20);
    }

    #[test]
    fn variables_deref_recursively() {
        let mut v = Vars::from_env("t".into(), vec![]);
        v.set("a", "b".into());
        v.set("b", "7".into());
        assert_eq!(evv("a + 1", &mut v), 8);
    }

    #[test]
    fn unset_variable_is_zero() {
        assert_eq!(ev("nope + 5"), 5);
    }

    #[test]
    fn assignment_and_compound_assignment() {
        let mut v = Vars::from_env("t".into(), vec![]);
        assert_eq!(evv("x = 5", &mut v), 5);
        assert_eq!(v.get("x").as_deref(), Some("5"));
        assert_eq!(evv("x += 3", &mut v), 8);
        assert_eq!(v.get("x").as_deref(), Some("8"));
    }

    #[test]
    fn increments() {
        let mut v = Vars::from_env("t".into(), vec![]);
        v.set("i", "1".into());
        assert_eq!(evv("i++", &mut v), 1); // postfix yields the old value
        assert_eq!(v.get("i").as_deref(), Some("2"));
        assert_eq!(evv("++i", &mut v), 3); // prefix yields the new value
    }

    #[test]
    fn division_by_zero_is_an_error() {
        let mut v = Vars::from_env("t".into(), vec![]);
        assert!(eval("1 / 0", &mut v).is_err());
        assert!(eval("1 % 0", &mut v).is_err());
    }

    #[test]
    fn negative_exponent_is_an_error() {
        let mut v = Vars::from_env("t".into(), vec![]);
        assert!(eval("2 ** -1", &mut v).is_err());
    }

    #[test]
    fn overflow_wraps_instead_of_trapping() {
        assert_eq!(ev("9223372036854775807 + 1"), i64::MIN);
    }
}
