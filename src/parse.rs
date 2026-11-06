//! Parser: tokens -> command list.
//!
//! Grammar:
//!
//!   list       := and_or ( (';' | '&' | '\n') and_or )*
//!   and_or     := pipeline ( ('&&' | '||') pipeline )*
//!   pipeline   := command ( '|' command )*
//!   command    := compound | simple
//!   compound   := if | for | while | until | case | '{' list '}' | '(' list ')'
//!   simple     := ( assignment | word | redirection )+
//!
//! Reserved words only count in command position, so `echo if` passes `if`
//! through as an ordinary argument.

use crate::lex::{Token, Word};

#[derive(Debug, Clone, PartialEq)]
pub enum Sep {
    Always,
    OnSuccess,
    OnFailure,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RedirKind {
    Out,
    Append,
    In,
    /// `<<` / `<<-`. `target` holds the body, already lexed into pieces.
    HereDoc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Redir {
    pub kind: RedirKind,
    pub target: Word,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Simple {
    pub assigns: Vec<(String, Word)>,
    pub args: Vec<Word>,
    pub redirs: Vec<Redir>,
}

impl Simple {
    pub fn is_assignment_only(&self) -> bool {
        self.args.is_empty() && !self.assigns.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Simple(Simple),
    /// `if cond; then body; elif cond; then body; else body; fi`
    If {
        branches: Vec<(List, List)>,
        else_body: Option<List>,
    },
    For {
        var: String,
        /// `None` means `for x; do` — iterate over the positional parameters.
        words: Option<Vec<Word>>,
        body: List,
    },
    While {
        cond: List,
        body: List,
        /// `until` runs the body while the condition FAILS.
        until: bool,
    },
    Case {
        word: Word,
        arms: Vec<(Vec<Word>, List)>,
    },
    /// `{ list; }` — runs in the current shell.
    Group(List),
    /// `( list )` — would need a subshell; parsed so the error is clear.
    Subshell(List),
    /// `name() { ... }` — definition; stored in the shell, not executed.
    Function { name: String, body: Box<Command> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pipeline {
    pub cmds: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct List {
    pub items: Vec<(Pipeline, Sep)>,
}

pub fn parse(tokens: &[Token]) -> Result<List, String> {
    let mut p = Parser { toks: tokens, pos: 0 };
    let list = p.list()?;
    if p.pos != tokens.len() {
        return Err(format!("unexpected token at end: {:?}", tokens[p.pos]));
    }
    Ok(list)
}

struct Parser<'a> {
    toks: &'a [Token],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.toks.get(self.pos)
    }

    /// True when the next token is the reserved word `kw` in command position.
    fn at_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if w.is_literal(kw))
    }

    fn eat_keyword(&mut self, kw: &str) -> bool {
        if self.at_keyword(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), String> {
        if self.eat_keyword(kw) {
            Ok(())
        } else {
            Err(format!("expected '{kw}'"))
        }
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Token, what: &str) -> Result<(), String> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(format!("expected {what}"))
        }
    }

    /// Eats separators that stand between a keyword and the list it opens.
    ///
    /// `do`, `then` and friends are almost always followed by a newline, and a
    /// newline lexes as `;`. Without this a multi-line script fails at its
    /// first block, because the list would start on a separator.
    fn skip_separators(&mut self) {
        while self.eat(&Token::Semi) {}
    }

    fn list(&mut self) -> Result<List, String> {
        let mut items = Vec::new();
        loop {
            if self.at_list_end() {
                break;
            }
            let pipeline = self.and_or()?;
            let (sep, had_sep) = match self.peek() {
                Some(Token::Semi) => {
                    self.pos += 1;
                    (Sep::Always, true)
                }
                Some(Token::And) => {
                    self.pos += 1;
                    (Sep::OnSuccess, true)
                }
                Some(Token::Or) => {
                    self.pos += 1;
                    (Sep::OnFailure, true)
                }
                _ => (Sep::Always, false),
            };
            items.push((pipeline, sep));
            if !had_sep {
                // Without a separator the list is over — anything left that is
                // not a terminator means the input is malformed.
                if !self.at_list_end() && self.peek().is_some() {
                    return Err(format!("unexpected token: {:?}", self.peek()));
                }
                break;
            }
        }
        Ok(List { items })
    }

    fn at_list_end(&self) -> bool {
        match self.peek() {
            None => true,
            Some(Token::RParen) | Some(Token::DSemi) => true,
            Some(Token::Word(w)) => {
                for kw in ["then", "else", "elif", "fi", "do", "done", "esac", "in", "}"] {
                    if w.is_literal(kw) {
                        return true;
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn and_or(&mut self) -> Result<Pipeline, String> {
        // `&&`/`||` chain pipelines; the AST keeps them flat in the list, so a
        // single pipeline is returned and the separators are handled by list().
        self.pipeline()
    }

    fn pipeline(&mut self) -> Result<Pipeline, String> {
        let mut cmds = vec![self.command()?];
        while self.eat(&Token::Pipe) {
            cmds.push(self.command()?);
        }
        Ok(Pipeline { cmds })
    }

    /// `name() compound-command` — POSIX function definition.
    fn at_function_def(&self) -> Option<String> {
        let Some(Token::Word(w)) = self.toks.get(self.pos) else {
            return None;
        };
        if self.toks.get(self.pos + 1) != Some(&Token::LParen) {
            return None;
        }
        if self.toks.get(self.pos + 2) != Some(&Token::RParen) {
            return None;
        }
        let name = w.literal()?;
        if is_name(&name) {
            Some(name)
        } else {
            None
        }
    }

    fn function_def(&mut self, name: String) -> Result<Command, String> {
        self.pos += 3; // name ( )
        let body = self.command()?;
        Ok(Command::Function { name, body: Box::new(body) })
    }

    fn command(&mut self) -> Result<Command, String> {
        if let Some(name) = self.at_function_def() {
            return self.function_def(name);
        }
        if self.at_keyword("if") {
            return self.if_command();
        }
        if self.at_keyword("for") {
            return self.for_command();
        }
        if self.at_keyword("while") {
            return self.while_command(false);
        }
        if self.at_keyword("until") {
            return self.while_command(true);
        }
        if self.at_keyword("case") {
            return self.case_command();
        }
        if self.at_keyword("{") {
            self.pos += 1;
            self.skip_separators();
            let body = self.list()?;
            self.expect_keyword("}")?;
            return Ok(Command::Group(body));
        }
        if self.eat(&Token::LParen) {
            self.skip_separators();
            let body = self.list()?;
            self.expect(&Token::RParen, "')'")?;
            return Ok(Command::Subshell(body));
        }
        Ok(Command::Simple(self.simple()?))
    }

    fn if_command(&mut self) -> Result<Command, String> {
        self.expect_keyword("if")?;
        let mut branches = Vec::new();
        let cond = self.list()?;
        self.expect_keyword("then")?;
        self.skip_separators();
        let body = self.list()?;
        branches.push((cond, body));
        let mut else_body = None;
        loop {
            if self.eat_keyword("elif") {
                let c = self.list()?;
                self.expect_keyword("then")?;
                self.skip_separators();
                let b = self.list()?;
                branches.push((c, b));
                continue;
            }
            if self.eat_keyword("else") {
                self.skip_separators();
                else_body = Some(self.list()?);
            }
            break;
        }
        self.expect_keyword("fi")?;
        Ok(Command::If {
            branches,
            else_body,
        })
    }

    fn for_command(&mut self) -> Result<Command, String> {
        self.expect_keyword("for")?;
        let var = match self.peek() {
            Some(Token::Word(w)) => match w.literal() {
                Some(name) if is_name(&name) => {
                    self.pos += 1;
                    name
                }
                _ => return Err("for: expected a variable name".into()),
            },
            _ => return Err("for: expected a variable name".into()),
        };
        let words = if self.eat_keyword("in") {
            let mut ws = Vec::new();
            while let Some(Token::Word(w)) = self.peek() {
                ws.push(w.clone());
                self.pos += 1;
            }
            Some(ws)
        } else {
            None
        };
        // An optional ';' or newline may precede `do`.
        self.eat(&Token::Semi);
        self.expect_keyword("do")?;
        self.skip_separators();
        let body = self.list()?;
        self.expect_keyword("done")?;
        Ok(Command::For { var, words, body })
    }

    fn while_command(&mut self, until: bool) -> Result<Command, String> {
        self.pos += 1; // while | until
        let cond = self.list()?;
        self.expect_keyword("do")?;
        self.skip_separators();
        let body = self.list()?;
        self.expect_keyword("done")?;
        Ok(Command::While { cond, body, until })
    }

    fn case_command(&mut self) -> Result<Command, String> {
        self.expect_keyword("case")?;
        let word = match self.peek() {
            Some(Token::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                w
            }
            _ => return Err("case: expected a word".into()),
        };
        self.expect_keyword("in")?;
        let mut arms = Vec::new();
        while !self.at_keyword("esac") {
            if self.peek().is_none() {
                return Err("case: missing 'esac'".into());
            }
            self.eat(&Token::Semi);
            if self.at_keyword("esac") {
                break;
            }
            // `(` is optional before a pattern list.
            self.eat(&Token::LParen);
            let mut patterns = Vec::new();
            loop {
                match self.peek() {
                    Some(Token::Word(w)) => {
                        patterns.push(w.clone());
                        self.pos += 1;
                    }
                    _ => return Err("case: expected a pattern".into()),
                }
                if !self.eat(&Token::Pipe) {
                    break;
                }
            }
            self.expect(&Token::RParen, "')' after a case pattern")?;
            self.skip_separators();
            let body = self.list()?;
            self.expect(&Token::DSemi, "';;'")?;
            arms.push((patterns, body));
        }
        self.expect_keyword("esac")?;
        Ok(Command::Case { word, arms })
    }

    fn simple(&mut self) -> Result<Simple, String> {
        let mut cmd = Simple::default();
        loop {
            match self.peek() {
                Some(Token::Word(w)) => {
                    if cmd.args.is_empty() {
                        if let Some((name, value)) = as_assignment(w) {
                            cmd.assigns.push((name, value));
                            self.pos += 1;
                            continue;
                        }
                    }
                    cmd.args.push(w.clone());
                    self.pos += 1;
                }
                Some(Token::RedirOut) | Some(Token::RedirAppend) | Some(Token::RedirIn) => {
                    let kind = match self.peek() {
                        Some(Token::RedirOut) => RedirKind::Out,
                        Some(Token::RedirAppend) => RedirKind::Append,
                        _ => RedirKind::In,
                    };
                    self.pos += 1;
                    match self.peek() {
                        Some(Token::Word(target)) => {
                            cmd.redirs.push(Redir {
                                kind,
                                target: target.clone(),
                            });
                            self.pos += 1;
                        }
                        _ => return Err("redirection without a target".into()),
                    }
                }
                Some(Token::RedirHereDoc(body)) => {
                    // The body arrives already attached to the token; there is
                    // no separate target word to read.
                    cmd.redirs.push(Redir {
                        kind: RedirKind::HereDoc,
                        target: body.clone(),
                    });
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if cmd.args.is_empty() && cmd.redirs.is_empty() && cmd.assigns.is_empty() {
            return Err("expected a command".into());
        }
        Ok(cmd)
    }
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Recognises `NAME=value` when the name part is literal.
fn as_assignment(word: &Word) -> Option<(String, Word)> {
    let first = word.pieces.first()?;
    let crate::lex::Piece::Lit { text, quoted: false } = first else {
        return None;
    };
    let (name, rest_of_first) = text.split_once('=')?;
    if !is_name(name) {
        return None;
    }
    let mut value = Word::default();
    if !rest_of_first.is_empty() {
        value.pieces.push(crate::lex::Piece::Lit {
            text: rest_of_first.to_string(),
            quoted: false,
        });
    }
    value.pieces.extend(word.pieces[1..].iter().cloned());
    Some((name.to_string(), value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::lex;

    fn p(s: &str) -> List {
        parse(&lex(s).unwrap()).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn simple_command() {
        let l = p("echo hi");
        assert!(matches!(&l.items[0].0.cmds[0], Command::Simple(c) if c.args.len() == 2));
    }

    #[test]
    fn pipeline() {
        assert_eq!(p("a | b | c").items[0].0.cmds.len(), 3);
    }

    #[test]
    fn if_then_fi() {
        let l = p("if true; then echo y; fi");
        match &l.items[0].0.cmds[0] {
            Command::If { branches, else_body } => {
                assert_eq!(branches.len(), 1);
                assert!(else_body.is_none());
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    #[test]
    fn if_elif_else() {
        let l = p("if a; then b; elif c; then d; else e; fi");
        match &l.items[0].0.cmds[0] {
            Command::If { branches, else_body } => {
                assert_eq!(branches.len(), 2);
                assert!(else_body.is_some());
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    #[test]
    fn for_with_words() {
        let l = p("for i in a b c; do echo $i; done");
        match &l.items[0].0.cmds[0] {
            Command::For { var, words, .. } => {
                assert_eq!(var, "i");
                assert_eq!(words.as_ref().unwrap().len(), 3);
            }
            other => panic!("expected for, got {other:?}"),
        }
    }

    #[test]
    fn for_without_in() {
        let l = p("for x; do echo $x; done");
        assert!(matches!(&l.items[0].0.cmds[0], Command::For { words: None, .. }));
    }

    #[test]
    fn while_and_until() {
        assert!(matches!(&p("while true; do :; done").items[0].0.cmds[0],
            Command::While { until: false, .. }));
        assert!(matches!(&p("until false; do :; done").items[0].0.cmds[0],
            Command::While { until: true, .. }));
    }

    #[test]
    fn case_arms() {
        let l = p("case $x in a) echo A ;; b|c) echo BC ;; esac");
        match &l.items[0].0.cmds[0] {
            Command::Case { arms, .. } => {
                assert_eq!(arms.len(), 2);
                assert_eq!(arms[0].0.len(), 1);
                assert_eq!(arms[1].0.len(), 2);
            }
            other => panic!("expected case, got {other:?}"),
        }
    }

    #[test]
    fn group_and_subshell() {
        assert!(matches!(&p("{ echo a; }").items[0].0.cmds[0], Command::Group(_)));
        assert!(matches!(&p("( echo a )").items[0].0.cmds[0], Command::Subshell(_)));
    }

    #[test]
    fn keyword_as_argument_is_not_a_keyword() {
        let l = p("echo if then fi");
        match &l.items[0].0.cmds[0] {
            Command::Simple(c) => assert_eq!(c.args.len(), 4),
            other => panic!("expected simple, got {other:?}"),
        }
    }
}
