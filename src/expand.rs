//! Parameter expansion and field splitting.
//!
//! The rule that matters: an **unquoted** expansion is split on IFS, a quoted
//! one is not. `x="a b"; cmd $x` passes two arguments, `cmd "$x"` passes one.
//! Literal text is never split, however it looks.

use crate::lex::{Piece, Word};
use crate::var::Vars;

const DEFAULT_IFS: &[char] = &[' ', '\t', '\n'];

/// One expanded field. `pattern` is the same text with glob metacharacters
/// that came from quoted segments backslash-escaped, so `"*"` stays literal
/// while an unquoted `*` still globs.
pub struct Field {
    pub text: String,
    pub pattern: String,
}

fn escape_meta_into(text: &str, out: &mut String) {
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[') {
            out.push('\\');
        }
        out.push(c);
    }
}

/// Expands a word into zero or more fields.
/// Expands a word into fields, tracking for each field which metacharacters
/// were quoted (and so must not glob).
pub fn expand_word_fields(word: &Word, vars: &mut Vars) -> Vec<Field> {
    let mut fields: Vec<Field> = Vec::new();
    let mut cur = String::new();
    let mut cur_pat = String::new();
    let mut has = false;

    macro_rules! push_field {
        () => {{
            fields.push(Field {
                text: std::mem::take(&mut cur),
                pattern: std::mem::take(&mut cur_pat),
            });
            has = false;
        }};
    }

    for piece in &word.pieces {
        match piece {
            Piece::Lit { text, quoted } => {
                cur.push_str(text);
                if *quoted {
                    escape_meta_into(text, &mut cur_pat);
                } else {
                    cur_pat.push_str(text);
                }
                has = true;
            }
            Piece::Arith { expr, .. } => {
                let value = match crate::arith::eval(expr, vars) {
                    Ok(n) => n.to_string(),
                    Err(e) => {
                        eprintln!("winsh: arithmetic: {e}");
                        String::new()
                    }
                };
                cur.push_str(&value);
                cur_pat.push_str(&value);
                has = true;
            }
            Piece::CommandSub { text, quoted } => {
                let out = crate::exec::command_substitute(text, vars);
                // POSIX strips trailing newlines from the captured output.
                let value = out.trim_end_matches('\n').to_string();
                if *quoted {
                    cur.push_str(&value);
                    escape_meta_into(&value, &mut cur_pat);
                    has = true;
                    continue;
                }
                let parts = split_fields(&value);
                if parts.is_empty() {
                    continue;
                }
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        push_field!();
                    }
                    cur.push_str(part);
                    cur_pat.push_str(part);
                    has = true;
                }
            }
            Piece::Param { name, quoted } => {
                let value = vars.get(name).unwrap_or_default();
                if *quoted {
                    cur.push_str(&value);
                    escape_meta_into(&value, &mut cur_pat);
                    has = true;
                    continue;
                }
                let parts = split_fields(&value);
                if parts.is_empty() {
                    // An unquoted empty expansion contributes nothing at all —
                    // it does not create an empty field.
                    continue;
                }
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        push_field!();
                    }
                    cur.push_str(part);
                    cur_pat.push_str(part);
                    has = true;
                }
            }
        }
    }

    if has || !cur.is_empty() {
        push_field!();
    }
    fields
}

pub fn expand_word(word: &Word, vars: &mut Vars) -> Vec<String> {
    expand_word_fields(word, vars)
        .into_iter()
        .map(|f| f.text)
        .collect()
}

/// Expands a whole argument list, flattening each word's fields in order.
pub fn expand_words(words: &[Word], vars: &mut Vars) -> Vec<String> {
    let mut out = Vec::new();
    for w in words {
        out.extend(expand_word(w, vars));
    }
    out
}

/// Expands command arguments and applies pathname expansion to the fields
/// whose metacharacters were unquoted.
///
/// POSIX leaves a pattern unchanged when nothing matches; that is what makes
/// `rm *.tmp` harmless in an empty directory (the word reaches `rm` as-is).
pub fn expand_words_globbed(words: &[Word], vars: &mut Vars, cwd: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    for w in words {
        for f in expand_word_fields(w, vars) {
            if crate::glob::has_meta(&f.pattern) {
                let matched = crate::glob::expand(&f.pattern, cwd);
                if matched.is_empty() {
                    out.push(f.text);
                } else {
                    out.extend(matched);
                }
            } else {
                out.push(f.text);
            }
        }
    }
    out
}

fn split_fields(value: &str) -> Vec<String> {
    value
        .split(|c| DEFAULT_IFS.contains(&c))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::lex;

    fn first_word(src: &str) -> Word {
        lex(src)
            .unwrap()
            .into_iter()
            .find_map(|t| match t {
                crate::lex::Token::Word(w) => Some(w),
                _ => None,
            })
            .unwrap()
    }

    fn vars() -> Vars {
        let mut v = Vars::from_env("winsh".into(), vec!["p1".into(), "p2".into()]);
        v.set("x", "a b".into());
        v.set("empty", "".into());
        v.set("one", "solo".into());
        v
    }

    #[test]
    fn unquoted_expansion_splits() {
        assert_eq!(expand_word(&first_word("$x"), &mut vars()), vec!["a", "b"]);
    }

    #[test]
    fn quoted_expansion_does_not_split() {
        assert_eq!(expand_word(&first_word("\"$x\""), &mut vars()), vec!["a b"]);
    }

    #[test]
    fn literal_text_is_never_split() {
        // A quoted literal keeps its space; the lexer would otherwise have
        // split the input into two words before expansion ever ran.
        assert_eq!(expand_word(&first_word("'a b'"), &mut vars()), vec!["a b"]);
        assert_eq!(expand_word(&first_word(r"a\ b"), &mut vars()), vec!["a b"]);
    }

    #[test]
    fn empty_unquoted_expansion_yields_no_field() {
        assert!(expand_word(&first_word("$empty"), &mut vars()).is_empty());
    }

    #[test]
    fn empty_quoted_expansion_yields_one_empty_field() {
        assert_eq!(expand_word(&first_word("\"$empty\""), &mut vars()), vec![""]);
    }

    #[test]
    fn adjacent_literal_joins_first_field() {
        assert_eq!(expand_word(&first_word("pre$x"), &mut vars()), vec!["prea", "b"]);
    }

    #[test]
    fn special_parameters_expand() {
        let mut v = vars();
        v.status = 3;
        assert_eq!(expand_word(&first_word("$?"), &mut v), vec!["3"]);
        assert_eq!(expand_word(&first_word("$#"), &mut v), vec!["2"]);
        assert_eq!(expand_word(&first_word("${one}"), &mut v), vec!["solo"]);
    }

    #[test]
    fn multiple_words_flatten() {
        let words: Vec<Word> = lex("$x $one").unwrap().into_iter().filter_map(|t| match t {
            crate::lex::Token::Word(w) => Some(w),
            _ => None,
        }).collect();
        assert_eq!(expand_words(&words, &mut vars()), vec!["a", "b", "solo"]);
    }
}
