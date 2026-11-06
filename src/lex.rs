//! Tokenizer.
//!
//! A word is kept as a sequence of pieces rather than a flat string, because
//! expansion has to know where quoting ended: `"$x"` yields one field even if
//! `$x` contains spaces, while `$x` splits. Collapsing to a string here would
//! destroy that distinction irrecoverably.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Word(Word),
    Pipe,
    Semi,
    And,
    Or,
    RedirOut,
    RedirAppend,
    RedirIn,
    /// `<<` / `<<-`. The body is not on the command line: it starts on the
    /// next line, so the lexer attaches it once that line ends.
    RedirHereDoc(Word),
    LParen,
    RParen,
    /// `;;` — the case-arm terminator.
    DSemi,
}

/// One segment of a word.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// Literal text. `quoted` records whether it came from inside quotes (or
    /// was backslash-escaped), which suppresses field splitting later.
    Lit { text: String, quoted: bool },
    /// `$name`, `${name}`, `$?`, `$$`, `$#`, `$0`..`$9`.
    Param { name: String, quoted: bool },
    /// `$(( expression ))`, evaluated at expansion time.
    Arith { expr: String, quoted: bool },
    /// `$( command )`, run in a subshell; its stdout becomes the value.
    CommandSub { text: String, quoted: bool },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Word {
    pub pieces: Vec<Piece>,
}

impl Word {
    /// The word as plain text, with expansions left unsubstituted. Used by the
    /// parser to recognise assignment prefixes and redirection targets.
    pub fn literal(&self) -> Option<String> {
        let mut out = String::new();
        for p in &self.pieces {
            match p {
                Piece::Lit { text, .. } => out.push_str(text),
                Piece::Param { .. } | Piece::Arith { .. } | Piece::CommandSub { .. } => {
                    return None
                }
            }
        }
        Some(out)
    }

    /// True when the word is a single unquoted literal equal to `s`.
    pub fn is_literal(&self, s: &str) -> bool {
        matches!(self.pieces.as_slice(),
            [Piece::Lit { text, quoted: false }] if text == s)
    }
}

/// A here-document whose delimiter has been read but whose body has not.
///
/// The body begins on the line *after* the command, so it cannot be read where
/// the `<<` is found: the rest of the command line has to be lexed first.
struct PendingHeredoc {
    /// Index into the token vector of the `RedirHereDoc` to fill in.
    token: usize,
    delim: String,
    /// False when the delimiter was quoted (`<<'EOF'`), which suppresses every
    /// expansion in the body.
    expand: bool,
    /// `<<-` strips leading tabs from the body and the delimiter line.
    strip_tabs: bool,
}

pub fn lex(src: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let mut pending: Vec<PendingHeredoc> = Vec::new();
    let mut chars = src.chars().peekable();

    while let Some(&c) = chars.peek() {
        if c == '\n' {
            // A newline separates commands exactly like ';'. Inside quotes it
            // is part of the word and never reaches this branch.
            chars.next();
            out.push(Token::Semi);
            // Here-document bodies live after this newline, in the order their
            // redirections appeared.
            if !pending.is_empty() {
                read_heredocs(&mut chars, &mut out, &mut pending)?;
            }
            continue;
        }
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        match c {
            '|' => {
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    out.push(Token::Or);
                } else {
                    out.push(Token::Pipe);
                }
            }
            '&' => {
                chars.next();
                if chars.peek() == Some(&'&') {
                    chars.next();
                    out.push(Token::And);
                } else {
                    return Err("background jobs are not supported yet".into());
                }
            }
            ';' => {
                chars.next();
                if chars.peek() == Some(&';') {
                    chars.next();
                    out.push(Token::DSemi);
                } else {
                    out.push(Token::Semi);
                }
            }
            '(' => {
                chars.next();
                out.push(Token::LParen);
            }
            ')' => {
                chars.next();
                out.push(Token::RParen);
            }
            '>' => {
                chars.next();
                if chars.peek() == Some(&'>') {
                    chars.next();
                    out.push(Token::RedirAppend);
                } else {
                    out.push(Token::RedirOut);
                }
            }
            '<' => {
                chars.next();
                if chars.peek() == Some(&'<') {
                    chars.next();
                    let strip_tabs = if chars.peek() == Some(&'-') {
                        chars.next();
                        true
                    } else {
                        false
                    };
                    // `<< EOF` is legal: whitespace may separate the operator
                    // from its delimiter.
                    while matches!(chars.peek(), Some(&c) if c == ' ' || c == '\t') {
                        chars.next();
                    }
                    let (delim, delim_quoted) = read_heredoc_delim(&mut chars)?;
                    if delim.is_empty() {
                        return Err("here-document requires a delimiter".into());
                    }
                    // Quoting any part of the delimiter disables expansion in
                    // the body: `<<'EOF'`, `<<"EOF"` and `<<\EOF` all mean the
                    // body is taken literally.
                    let expand = !delim_quoted;
                    pending.push(PendingHeredoc {
                        token: out.len(),
                        delim,
                        expand,
                        strip_tabs,
                    });
                    out.push(Token::RedirHereDoc(Word::default()));
                } else {
                    out.push(Token::RedirIn);
                }
            }
            _ => out.push(Token::Word(read_word(&mut chars)?)),
        }
    }
    Ok(out)
}

/// Characters a backslash may escape.
///
/// This is the deliberate Windows deviation from POSIX: a backslash only
/// escapes something that would otherwise be special, so `C:\Users` and
/// `..\foo` survive unquoted instead of losing their separators.
///
/// Note `\\` still collapses to a single `\`, so an unquoted UNC path loses
/// one separator; write `'\\server\share'` or `//server/share` for those.
fn is_escapable(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '\'' | '"' | '`' | '$' | '\\' | '*' | '?'
                | '[' | ']' | '!' | '#'
        )
}

/// Reads a here-document delimiter: quote removal only, no expansion.
///
/// This is the one place where a backslash quotes the next character
/// unconditionally. Everywhere else in winsh a backslash only escapes a shell
/// metacharacter, so that `C:\Users` survives unquoted — but a delimiter is
/// never a path, and `<<\EOF` is the established POSIX spelling for "take the
/// body literally". Honouring it costs nothing and keeps ported scripts
/// working.
fn read_heredoc_delim(
    chars: &mut std::iter::Peekable<std::str::Chars>,
) -> Result<(String, bool), String> {
    let mut text = String::new();
    let mut quoted = false;
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() || matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')') {
            break;
        }
        match c {
            '\'' => {
                chars.next();
                quoted = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => text.push(ch),
                        None => return Err("unterminated quote in here-document delimiter".into()),
                    }
                }
            }
            '"' => {
                chars.next();
                quoted = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.peek() {
                            Some(&e @ ('"' | '\\' | '$' | '`')) => {
                                text.push(e);
                                chars.next();
                            }
                            _ => text.push('\\'),
                        },
                        Some(ch) => text.push(ch),
                        None => return Err("unterminated quote in here-document delimiter".into()),
                    }
                }
            }
            '\\' => {
                chars.next();
                match chars.next() {
                    Some(ch) => {
                        text.push(ch);
                        quoted = true;
                    }
                    None => text.push('\\'),
                }
            }
            _ => {
                text.push(c);
                chars.next();
            }
        }
    }
    Ok((text, quoted))
}

/// Characters that may follow a `$` and start an expansion.
///
/// Anything else leaves the `$` as a literal: `cost $ 5` and `a$` are
/// ordinary text, not errors.
fn starts_param(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || matches!(c, '(' | '{' | '?' | '$' | '#' | '!' | '@' | '*')
}

/// Reads one line, without its newline. `None` only at end of input.
fn read_line(chars: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
    let mut line = String::new();
    while let Some(&c) = chars.peek() {
        chars.next();
        if c == '\n' {
            return Some(line);
        }
        line.push(c);
    }
    if line.is_empty() {
        None
    } else {
        Some(line)
    }
}

/// Consumes the bodies of every pending here-document, in order, and attaches
/// each to its redirection token.
fn read_heredocs(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    out: &mut [Token],
    pending: &mut Vec<PendingHeredoc>,
) -> Result<(), String> {
    for p in pending.drain(..) {
        let mut body = String::new();
        loop {
            let Some(line) = read_line(chars) else {
                return Err(format!(
                    "unterminated here-document: expected a line with only '{}'",
                    p.delim
                ));
            };
            // `<<-` strips leading tabs from body lines and from the
            // delimiter line itself.
            let content = if p.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                line.as_str()
            };
            if content == p.delim {
                break;
            }
            body.push_str(content);
            body.push('\n');
        }
        let word = if p.expand {
            heredoc_body_word(&body)?
        } else {
            Word {
                pieces: vec![Piece::Lit {
                    text: body,
                    quoted: true,
                }],
            }
        };
        out[p.token] = Token::RedirHereDoc(word);
    }
    Ok(())
}

/// Lexes a here-document body.
///
/// The body is almost raw text: quotes and wildcards are literal, and only `\`,
/// `$` and a backtick are special. Every piece is marked quoted so that
/// expansion cannot field-split the body — a here-document is one stream of
/// bytes, not a list of words.
fn heredoc_body_word(text: &str) -> Result<Word, String> {
    let mut word = Word::default();
    let mut lit = String::new();
    let mut chars = text.chars().peekable();

    macro_rules! flush {
        () => {
            if !lit.is_empty() {
                word.pieces.push(Piece::Lit {
                    text: std::mem::take(&mut lit),
                    quoted: true,
                });
            }
        };
    }

    while let Some(&c) = chars.peek() {
        match c {
            '\\' => {
                chars.next();
                match chars.peek() {
                    // Backslash-newline joins the lines, as in POSIX.
                    Some('\n') => {
                        chars.next();
                    }
                    Some(&e @ ('$' | '`' | '\\')) => {
                        lit.push(e);
                        chars.next();
                    }
                    // Every other backslash is literal — Windows paths inside
                    // here-documents survive untouched.
                    _ => lit.push('\\'),
                }
            }
            '$' => {
                chars.next();
                match chars.peek() {
                    Some(&n) if starts_param(n) => {
                        flush!();
                        word.pieces.push(read_param_after_dollar(&mut chars, true)?);
                    }
                    _ => lit.push('$'),
                }
            }
            _ => {
                lit.push(c);
                chars.next();
            }
        }
    }
    flush!();
    if word.pieces.is_empty() {
        // An empty body must still be one (empty) field, not zero.
        word.pieces.push(Piece::Lit {
            text: String::new(),
            quoted: true,
        });
    }
    Ok(word)
}

fn read_word(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<Word, String> {
    let mut word = Word::default();
    let mut lit = String::new();
    let mut lit_quoted = false;
    // An empty quoted string (`""` or `''`) must still produce one empty
    // field; without this the word would vanish and `[ -z "" ]` would lose its
    // operand.
    let mut saw_quote = false;

    // Flushes the pending literal into a piece, merging with a neighbouring
    // literal that carries the same quoting state.
    macro_rules! flush {
        () => {
            if !lit.is_empty() {
                match word.pieces.last_mut() {
                    Some(Piece::Lit { text, quoted }) if *quoted == lit_quoted => {
                        text.push_str(&lit);
                    }
                    _ => word.pieces.push(Piece::Lit {
                        text: std::mem::take(&mut lit),
                        quoted: lit_quoted,
                    }),
                }
                lit.clear();
            }
        };
    }

    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() => break,
            '|' | ';' | '&' | '>' | '<' | '(' | ')' => break,
            '\'' => {
                chars.next();
                // Start a new segment: the text before the quote is unquoted.
                flush!();
                lit_quoted = true;
                saw_quote = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => lit.push(ch),
                        None => return Err("unterminated single quote".into()),
                    }
                }
            }
            '"' => {
                chars.next();
                flush!();
                lit_quoted = true;
                saw_quote = true;
                loop {
                    match chars.peek() {
                        Some('"') => {
                            chars.next();
                            break;
                        }
                        Some('\\') => {
                            chars.next();
                            match chars.peek() {
                                Some(&e @ ('"' | '\\' | '$' | '`')) => {
                                    lit.push(e);
                                    chars.next();
                                }
                                _ => lit.push('\\'),
                            }
                        }
                        Some('$') => {
                            chars.next();
                            match chars.peek() {
                                Some(&n) if starts_param(n) => {
                                    flush!();
                                    lit_quoted = true;
                                    word.pieces.push(read_param_after_dollar(chars, true)?);
                                }
                                _ => {
                                    lit_quoted = true;
                                    lit.push('$');
                                }
                            }
                        }
                        Some(&ch) => {
                            lit_quoted = true;
                            lit.push(ch);
                            chars.next();
                        }
                        None => return Err("unterminated double quote".into()),
                    }
                }
            }
            '\\' => {
                chars.next();
                match chars.peek() {
                    // Backslash-newline is a line continuation: both go away.
                    Some(&'\n') => {
                        chars.next();
                    }
                    Some(&next) if is_escapable(next) => {
                        lit_quoted = true;
                        lit.push(next);
                        chars.next();
                    }
                    _ => lit.push('\\'),
                }
            }
            '$' => {
                chars.next();
                match chars.peek() {
                    Some(&n) if starts_param(n) => {
                        flush!();
                        lit_quoted = false;
                        word.pieces.push(read_param_after_dollar(chars, false)?);
                    }
                    _ => lit.push('$'),
                }
            }
            _ => {
                lit.push(c);
                chars.next();
            }
        }
    }
    flush!();
    if word.pieces.is_empty() && saw_quote {
        word.pieces.push(Piece::Lit {
            text: String::new(),
            quoted: true,
        });
    }
    Ok(word)
}

/// Reads the part after `$`, which the caller has already consumed.
fn read_param_after_dollar(
    chars: &mut std::iter::Peekable<std::str::Chars>,
    quoted: bool,
) -> Result<Piece, String> {
    let name = match chars.peek() {
        Some('(') => {
            // `$((` opens an arithmetic expansion; `$(` alone is command
            // substitution, which is not implemented yet.
            chars.next();
            if chars.peek() != Some(&'(') {
                return Ok(Piece::CommandSub {
                    text: read_command_sub(chars)?,
                    quoted,
                });
            }
            chars.next();
            let mut depth = 0usize;
            let mut expr = String::new();
            loop {
                match chars.next() {
                    Some('(') => {
                        depth += 1;
                        expr.push('(');
                    }
                    Some(')') => {
                        if depth == 0 {
                            // Expect the closing paren of `))`.
                            match chars.next() {
                                Some(')') => break,
                                _ => return Err("unterminated $(( ... ))".into()),
                            }
                        }
                        depth -= 1;
                        expr.push(')');
                    }
                    Some(ch) => expr.push(ch),
                    None => return Err("unterminated $(( ... ))".into()),
                }
            }
            return Ok(Piece::Arith { expr, quoted });
        }
        Some('{') => {
            chars.next();
            let mut name = String::new();
            loop {
                match chars.next() {
                    Some('}') => break,
                    Some(ch) => name.push(ch),
                    None => return Err("unterminated ${...}".into()),
                }
            }
            if name.is_empty() {
                return Err("${} with an empty name".into());
            }
            name
        }
        Some(&c) if c.is_ascii_alphabetic() || c == '_' => {
            let mut name = String::new();
            while let Some(&ch) = chars.peek() {
                if ch.is_ascii_alphanumeric() || ch == '_' {
                    name.push(ch);
                    chars.next();
                } else {
                    break;
                }
            }
            name
        }
        Some(&c) if c.is_ascii_digit() || matches!(c, '?' | '$' | '#' | '!' | '@' | '*') => {
            chars.next();
            c.to_string()
        }
        _ => return Err("lone '$'".into()),
    };
    Ok(Piece::Param { name, quoted })
}

/// Reads the body of `$( ... )` up to the matching close paren. The opening
/// `$(` has already been consumed. Quotes inside are honoured so a `)` in a
/// string does not end the substitution early.
fn read_command_sub(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<String, String> {
    let mut out = String::new();
    let mut depth = 0usize;
    loop {
        match chars.next() {
            Some('(') => {
                depth += 1;
                out.push('(');
            }
            Some(')') => {
                if depth == 0 {
                    return Ok(out);
                }
                depth -= 1;
                out.push(')');
            }
            Some('\\') => {
                out.push('\\');
                if let Some(c) = chars.next() {
                    out.push(c);
                }
            }
            Some('\'') => {
                out.push('\'');
                loop {
                    match chars.next() {
                        Some('\'') => {
                            out.push('\'');
                            break;
                        }
                        Some(c) => out.push(c),
                        None => return Err("unterminated quote in $( )".into()),
                    }
                }
            }
            Some('"') => {
                out.push('"');
                loop {
                    match chars.next() {
                        Some('"') => {
                            out.push('"');
                            break;
                        }
                        Some('\\') => {
                            out.push('\\');
                            if let Some(c) = chars.next() {
                                out.push(c);
                            }
                        }
                        Some(c) => out.push(c),
                        None => return Err("unterminated quote in $( )".into()),
                    }
                }
            }
            Some(c) => out.push(c),
            None => return Err("unterminated $( ... )".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(src: &str) -> Vec<Word> {
        lex(src)
            .unwrap()
            .into_iter()
            .filter_map(|t| match t {
                Token::Word(w) => Some(w),
                _ => None,
            })
            .collect()
    }

    fn texts(src: &str) -> Vec<String> {
        words(src).iter().filter_map(|w| w.literal()).collect()
    }

    #[test]
    fn splits_words_and_operators() {
        assert_eq!(
            lex("ls -la | grep x").unwrap().len(),
            5,
            "ls -la | grep x is five tokens"
        );
    }

    #[test]
    fn quotes_group_spaces() {
        assert_eq!(texts("echo 'a b' \"c d\""), vec!["echo", "a b", "c d"]);
    }

    #[test]
    fn backslash_escapes_only_specials() {
        assert_eq!(texts(r"echo a\ b")[1], "a b");
        assert_eq!(texts(r"echo a\$b")[1], "a$b");
        assert_eq!(texts(r"echo a\\b")[1], r"a\b");
        assert_eq!(texts(r"echo a\|b")[1], "a|b");
    }

    #[test]
    fn windows_paths_survive_unquoted() {
        assert_eq!(texts(r"echo C:\Users\znie")[1], r"C:\Users\znie");
        assert_eq!(texts(r"echo ..\..\foo")[1], r"..\..\foo");
    }

    #[test]
    fn backslash_newline_is_a_continuation() {
        assert_eq!(texts("echo a\\\nb"), vec!["echo", "ab"]);
    }

    #[test]
    fn newline_separates_commands() {
        assert!(lex("echo a\necho b").unwrap().contains(&Token::Semi));
    }

    #[test]
    fn parameter_pieces_carry_quoting() {
        let w = &words("$x \"$y\" '${z}'")[0];
        assert!(matches!(&w.pieces[0], Piece::Param { name, quoted: false } if name == "x"));
        let w = &words("\"$y\"")[0];
        assert!(matches!(&w.pieces[0], Piece::Param { name, quoted: true } if name == "y"));
        // Single quotes suppress expansion entirely: it stays literal text.
        assert_eq!(words("'${z}'")[0].literal().as_deref(), Some("${z}"));
    }

    #[test]
    fn special_parameters() {
        for (src, want) in [
            ("$?", "?"),
            ("$$", "$"),
            ("$#", "#"),
            ("$1", "1"),
            ("${name}", "name"),
        ] {
            match &words(src)[0].pieces[0] {
                Piece::Param { name, .. } => assert_eq!(name, want),
                other => panic!("{src}: expected a param, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_unterminated_quote() {
        assert!(lex("echo 'oops").is_err());
    }

    /// The body of the first here-document in `src`.
    fn heredoc_body(src: &str) -> Word {
        lex(src)
            .unwrap_or_else(|e| panic!("{src}: {e}"))
            .into_iter()
            .find_map(|t| match t {
                Token::RedirHereDoc(w) => Some(w),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{src}: no here-document"))
    }

    #[test]
    fn heredoc_body_comes_from_the_following_lines() {
        let w = heredoc_body("cat <<END\nline1\nline2\nEND\necho after");
        assert_eq!(
            w.pieces,
            vec![Piece::Lit {
                text: "line1\nline2\n".into(),
                quoted: true
            }]
        );
    }

    #[test]
    fn heredoc_body_stops_at_the_delimiter_line_only() {
        // `ENDX` and `END ` are body text, not the terminator.
        let w = heredoc_body("cat <<END\nENDX\nEND \nEND\n");
        assert_eq!(
            w.pieces,
            vec![Piece::Lit {
                text: "ENDX\nEND \n".into(),
                quoted: true
            }]
        );
    }

    #[test]
    fn heredoc_quoted_delimiter_takes_the_body_literally() {
        for src in [
            "cat <<'END'\nraw $x\nEND\n",
            "cat <<\"END\"\nraw $x\nEND\n",
            "cat <<\\END\nraw $x\nEND\n",
        ] {
            let w = heredoc_body(src);
            assert_eq!(
                w.pieces,
                vec![Piece::Lit {
                    text: "raw $x\n".into(),
                    quoted: true
                }],
                "{src}"
            );
        }
    }

    #[test]
    fn heredoc_unquoted_delimiter_expands() {
        let w = heredoc_body("cat <<END\nvalue=$x\nEND\n");
        assert!(
            matches!(&w.pieces[1], Piece::Param { name, quoted: true } if name == "x"),
            "{:?}",
            w.pieces
        );
    }

    #[test]
    fn heredoc_dash_strips_leading_tabs() {
        let w = heredoc_body("cat <<-END\n\tone\n\t\ttwo\n\tEND\n");
        assert_eq!(
            w.pieces,
            vec![Piece::Lit {
                text: "one\ntwo\n".into(),
                quoted: true
            }]
        );
    }

    #[test]
    fn heredoc_body_keeps_backslashes_that_are_not_special() {
        // `\a` and Windows paths survive; only `\$`, `\\` and `\`+newline are
        // special inside a body.
        let w = heredoc_body("cat <<END\nC:\\Users \\a \\\\ \\$\nEND\n");
        assert_eq!(
            w.pieces,
            vec![Piece::Lit {
                text: "C:\\Users \\a \\ $\n".into(),
                quoted: true
            }]
        );
    }

    #[test]
    fn heredoc_body_joins_backslash_newline() {
        let w = heredoc_body("cat <<END\na\\\nb\nEND\n");
        assert_eq!(
            w.pieces,
            vec![Piece::Lit {
                text: "ab\n".into(),
                quoted: true
            }]
        );
    }

    #[test]
    fn two_heredocs_are_read_in_order() {
        let toks = lex("cat <<A <<B\nfirst\nA\nsecond\nB\n").unwrap();
        let bodies: Vec<_> = toks
            .into_iter()
            .filter_map(|t| match t {
                Token::RedirHereDoc(w) => w.literal(),
                _ => None,
            })
            .collect();
        assert_eq!(bodies, vec!["first\n", "second\n"]);
    }

    #[test]
    fn empty_heredoc_body_is_one_empty_piece() {
        let w = heredoc_body("cat <<END\nEND\n");
        assert_eq!(
            w.pieces,
            vec![Piece::Lit {
                text: String::new(),
                quoted: true
            }]
        );
    }

    #[test]
    fn unterminated_heredoc_is_an_error() {
        assert!(lex("cat <<END\nno terminator\n").is_err());
    }

    #[test]
    fn a_dollar_without_a_name_is_literal() {
        // Erroring here would reject ordinary prose and awk one-liners.
        assert_eq!(texts("echo a$"), vec!["echo", "a$"]);
        assert_eq!(texts("echo cost $ 5"), vec!["echo", "cost", "$", "5"]);
        assert_eq!(texts("echo \"$\""), vec!["echo", "$"]);
    }
}
