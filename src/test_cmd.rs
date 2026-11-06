//! The `test` / `[` builtin.
//!
//! Control flow is useless without it: `if [ -f x ]`, `while [ $i -lt 3 ]`.
//! Implemented as a small recursive-descent evaluator so `!`, `-a`, `-o` and
//! parentheses compose properly instead of needing special-case arity rules.

use std::path::Path;

pub fn evaluate(args: &[String]) -> Result<bool, String> {
    if args.is_empty() {
        return Ok(false);
    }
    let mut p = Parser { args, pos: 0 };
    let v = p.or_expr()?;
    if p.pos != args.len() {
        return Err(format!("test: unexpected argument '{}'", args[p.pos]));
    }
    Ok(v)
}

struct Parser<'a> {
    args: &'a [String],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&str> {
        self.args.get(self.pos).map(|s| s.as_str())
    }

    fn or_expr(&mut self) -> Result<bool, String> {
        let mut v = self.and_expr()?;
        while self.peek() == Some("-o") {
            self.pos += 1;
            let r = self.and_expr()?;
            v = v || r;
        }
        Ok(v)
    }

    fn and_expr(&mut self) -> Result<bool, String> {
        let mut v = self.not_expr()?;
        while self.peek() == Some("-a") {
            self.pos += 1;
            let r = self.not_expr()?;
            v = v && r;
        }
        Ok(v)
    }

    fn not_expr(&mut self) -> Result<bool, String> {
        if self.peek() == Some("!") {
            self.pos += 1;
            return Ok(!self.not_expr()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<bool, String> {
        // Parenthesised sub-expression.
        if self.peek() == Some("(") {
            self.pos += 1;
            let v = self.or_expr()?;
            if self.peek() != Some(")") {
                return Err("test: missing ')'".into());
            }
            self.pos += 1;
            return Ok(v);
        }

        let Some(first) = self.peek() else {
            return Err("test: expected an expression".into());
        };

        // Unary operator.
        if is_unary(first) {
            let op = first.to_string();
            self.pos += 1;
            let Some(operand) = self.peek() else {
                return Err(format!("test: '{op}' needs an operand"));
            };
            let operand = operand.to_string();
            self.pos += 1;
            return Ok(unary(&op, &operand));
        }

        // Binary operator.
        if let Some(op) = self.args.get(self.pos + 1).map(|s| s.as_str()) {
            if is_binary(op) {
                let left = first.to_string();
                let op = op.to_string();
                self.pos += 2;
                let Some(right) = self.peek() else {
                    return Err(format!("test: '{op}' needs a right operand"));
                };
                let right = right.to_string();
                self.pos += 1;
                return Ok(binary(&left, &op, &right));
            }
        }

        // A lone string is true when non-empty.
        let v = !first.is_empty();
        self.pos += 1;
        Ok(v)
    }
}

fn is_unary(s: &str) -> bool {
    matches!(
        s,
        "-e" | "-f" | "-d" | "-r" | "-w" | "-x" | "-s" | "-L" | "-h" | "-z" | "-n"
    )
}

fn is_binary(s: &str) -> bool {
    matches!(
        s,
        "=" | "==" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
    )
}

fn unary(op: &str, operand: &str) -> bool {
    let path = Path::new(operand);
    match op {
        "-e" => path.exists(),
        "-f" => path.is_file(),
        "-d" => path.is_dir(),
        "-s" => std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false),
        "-L" | "-h" => std::fs::symlink_metadata(path)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        // Windows has no permission bits; existence is the honest answer, and
        // DESIGN.md says to be honest rather than fake a POSIX model.
        "-r" | "-w" | "-x" => path.exists(),
        "-z" => operand.is_empty(),
        "-n" => !operand.is_empty(),
        _ => false,
    }
}

fn binary(left: &str, op: &str, right: &str) -> bool {
    match op {
        "=" | "==" => left == right,
        "!=" => left != right,
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            let l = parse_int(left);
            let r = parse_int(right);
            match (l, r) {
                (Some(l), Some(r)) => match op {
                    "-eq" => l == r,
                    "-ne" => l != r,
                    "-lt" => l < r,
                    "-le" => l <= r,
                    "-gt" => l > r,
                    "-ge" => l >= r,
                    _ => false,
                },
                // A non-numeric operand makes the comparison false, with a
                // message saying why — guessing a value would hide the bug in
                // the script that caused it.
                _ => {
                    eprintln!("test: integer expression expected: '{left}' '{right}'");
                    false
                }
            }
        }
        _ => false,
    }
}

fn parse_int(s: &str) -> Option<i64> {
    s.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(args: &[&str]) -> bool {
        evaluate(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn empty_and_single() {
        assert!(!t(&[]));
        assert!(t(&["x"]));
        assert!(!t(&[""]));
    }

    #[test]
    fn string_comparison() {
        assert!(t(&["a", "=", "a"]));
        assert!(!t(&["a", "=", "b"]));
        assert!(t(&["a", "!=", "b"]));
        assert!(t(&["-z", ""]));
        assert!(t(&["-n", "x"]));
    }

    #[test]
    fn numeric_comparison() {
        assert!(t(&["1", "-eq", "1"]));
        assert!(t(&["1", "-lt", "2"]));
        assert!(t(&["2", "-ge", "2"]));
        assert!(!t(&["3", "-lt", "2"]));
        assert!(t(&["-5", "-lt", "0"]));
    }

    #[test]
    fn negation_and_logic() {
        assert!(t(&["!", "a", "=", "b"]));
        assert!(t(&["1", "-eq", "1", "-a", "2", "-eq", "2"]));
        assert!(!t(&["1", "-eq", "2", "-a", "2", "-eq", "2"]));
        assert!(t(&["1", "-eq", "2", "-o", "2", "-eq", "2"]));
    }

    #[test]
    fn parentheses() {
        assert!(t(&["(", "1", "-eq", "2", "-o", "3", "-eq", "3", ")"]));
        assert!(!t(&["!", "(", "1", "-eq", "1", ")"]));
    }

    #[test]
    fn file_tests() {
        assert!(t(&["-d", "."]));
        assert!(!t(&["-f", "."]));
        assert!(!t(&["-e", "definitely-not-here-xyz"]));
    }
}
