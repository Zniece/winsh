//! Executor.
//!
//! Expansion happens here, not in the parser: a word becomes zero or more
//! fields only when the command is about to run.

use crate::expand::{expand_word, expand_words, expand_words_globbed};
use crate::parse::{Command as Cmd, List, Pipeline, Redir, RedirKind, Sep, Simple};
use crate::var::Vars;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};

pub enum Flow {
    Continue(i32),
    /// `exit` — leave the whole shell.
    Exit(i32),
    /// `return` — leave the innermost function.
    Return(i32),
    /// `break [n]` — leave n enclosing loops.
    Break(u32),
    /// `continue [n]` — skip to the next iteration of the nth enclosing loop.
    ContinueLoop(u32),
}

impl Flow {
    fn clone_status(&self) -> i32 {
        match self {
            Flow::Continue(c) | Flow::Exit(c) | Flow::Return(c) => *c,
            Flow::Break(_) | Flow::ContinueLoop(_) => 0,
        }
    }

    /// Collapses a non-local flow into a status for contexts that cannot act
    /// on it (the top level, a pipeline stage).
    fn into_status(self) -> i32 {
        match self {
            Flow::Continue(c) | Flow::Exit(c) | Flow::Return(c) => c,
            Flow::Break(_) | Flow::ContinueLoop(_) => 0,
        }
    }
}

thread_local! {
    /// Active stdout capture for `$( ... )`. The shell is single-threaded, so
    /// a thread-local is enough and avoids threading a sink through every
    /// builtin.
    static CAPTURE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn capturing() -> bool {
    CAPTURE.with(|c| c.borrow().is_some())
}

fn capture_write(text: &str) {
    CAPTURE.with(|c| {
        if let Some(buf) = c.borrow_mut().as_mut() {
            buf.push_str(text);
        }
    });
}

/// Runs `text` in a subshell and returns its standard output.
///
/// The subshell gets a copy of the variables, so assignments inside it do not
/// leak out — that is the observable part of POSIX subshell semantics that
/// matters for command substitution.
pub fn command_substitute(text: &str, vars: &Vars) -> String {
    let tokens = match crate::lex::lex(text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("winsh: command substitution: {e}");
            return String::new();
        }
    };
    let list = match crate::parse::parse(&tokens) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("winsh: command substitution: {e}");
            return String::new();
        }
    };
    let mut sub = vars.clone();
    CAPTURE.with(|c| *c.borrow_mut() = Some(String::new()));
    let _ = run_flow(&list, &mut sub);
    let out = CAPTURE.with(|c| c.borrow_mut().take()).unwrap_or_default();
    out
}

pub fn run(list: &List, vars: &mut Vars) -> i32 {
    run_flow(list, vars).into_status()
}

/// The internal form: distinguishes "the shell must exit" from "this list
/// finished with status N". Compound commands need that distinction.
fn run_flow(list: &List, vars: &mut Vars) -> Flow {
    let mut last = vars.status;
    for (pipeline, sep) in &list.items {
        match run_pipeline(pipeline, vars) {
            f @ (Flow::Exit(_) | Flow::Return(_) | Flow::Break(_) | Flow::ContinueLoop(_)) => {
                vars.status = f.clone_status();
                return f;
            }
            Flow::Continue(code) => {
                vars.status = code;
                last = code;
                match sep {
                    Sep::OnSuccess if code != 0 => break,
                    Sep::OnFailure if code == 0 => break,
                    _ => {}
                }
            }
        }
    }
    Flow::Continue(last)
}

fn run_pipeline(p: &Pipeline, vars: &mut Vars) -> Flow {
    if p.cmds.len() == 1 {
        return match &p.cmds[0] {
            Cmd::Simple(c) => run_single(c, vars),
            other => run_compound(other, vars),
        };
    }

    // A builtin in a pipeline would need a fork-like mechanism the shell does
    // not have; say so rather than silently misbehaving.
    for cmd in &p.cmds {
        let Cmd::Simple(simple) = cmd else {
            eprintln!("winsh: compound command in a pipeline is not supported yet");
            return Flow::Continue(2);
        };
        if let Some(first) = simple.args.first() {
            let name = expand_word(first, vars);
            if let Some(n) = name.first() {
                if is_builtin(n) {
                    eprintln!("winsh: '{n}': builtin in a pipeline is not supported yet");
                    return Flow::Continue(2);
                }
            }
        }
    }

    match spawn_pipeline(p, vars, &[]) {
        Ok(code) => Flow::Continue(code),
        Err(e) => {
            eprintln!("winsh: {e}");
            Flow::Continue(1)
        }
    }
}

/// Runs a compound command. Everything here shares the shell's own state
/// (`cd`, variables, `$?`), which is why none of it can be delegated to a
/// child process.
fn run_compound(cmd: &Cmd, vars: &mut Vars) -> Flow {
    match cmd {
        Cmd::If {
            branches,
            else_body,
        } => {
            for (cond, body) in branches {
                match run_flow(cond, vars) {
                    Flow::Continue(0) => return run_flow(body, vars),
                    Flow::Continue(_) => continue,
                    other => return other,
                }
            }
            match else_body {
                Some(b) => run_flow(b, vars),
                None => Flow::Continue(0),
            }
        }
        Cmd::For { var, words, body } => {
            // `for x; do` iterates the positional parameters.
            let items: Vec<String> = match words {
                Some(ws) => expand_words_globbed(ws, vars, &cwd()),
                None => vars.positional.clone(),
            };
            for item in items {
                vars.set(var, item);
                match run_flow(body, vars) {
                    Flow::Break(n) => {
                        return if n <= 1 { Flow::Continue(0) } else { Flow::Break(n - 1) };
                    }
                    Flow::ContinueLoop(n) => {
                        if n <= 1 {
                            continue;
                        }
                        return Flow::ContinueLoop(n - 1);
                    }
                    Flow::Continue(_) => {}
                    other => return other,
                }
            }
            Flow::Continue(0)
        }
        Cmd::While { cond, body, until } => {
            loop {
                let code = match run_flow(cond, vars) {
                    Flow::Continue(c) => c,
                    other => return other,
                };
                let keep_going = if *until { code != 0 } else { code == 0 };
                if !keep_going {
                    break;
                }
                match run_flow(body, vars) {
                    Flow::Break(n) => {
                        return if n <= 1 { Flow::Continue(0) } else { Flow::Break(n - 1) };
                    }
                    Flow::ContinueLoop(n) => {
                        if n <= 1 {
                            continue;
                        }
                        return Flow::ContinueLoop(n - 1);
                    }
                    Flow::Continue(_) => {}
                    other => return other,
                }
            }
            Flow::Continue(0)
        }
        Cmd::Case { word, arms } => {
            let value = expand_word(word, vars).join(" ");
            for (patterns, body) in arms {
                for pat in patterns {
                    let p = expand_word(pat, vars).join(" ");
                    if crate::glob::matches(&p, &value) {
                        return run_flow(body, vars);
                    }
                }
            }
            Flow::Continue(0)
        }
        Cmd::Function { name, body } => {
            vars.functions.insert(name.clone(), (**body).clone());
            Flow::Continue(0)
        }
        Cmd::Group(body) => run_flow(body, vars),
        Cmd::Subshell(_) => {
            // A real subshell needs fork-like isolation the shell does not
            // have on Windows. Refuse loudly rather than run in-place and
            // silently leak state changes.
            eprintln!("winsh: '( ... )' subshells are not supported yet");
            Flow::Continue(2)
        }
        Cmd::Simple(_) => unreachable!("simple commands are dispatched by run_single"),
    }
}

fn cwd() -> std::path::PathBuf {
    std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
}

fn run_single(cmd: &Simple, vars: &mut Vars) -> Flow {
    // 1. Assignments are expanded first, left to right.
    let mut assigns: Vec<(String, String)> = Vec::new();
    for (name, value_word) in &cmd.assigns {
        let fields = expand_word(value_word, vars);
        assigns.push((name.clone(), fields.join(" ")));
    }

    // 2. A command consisting only of assignments mutates the shell.
    if cmd.is_assignment_only() {
        for (name, value) in assigns {
            vars.set(&name, value);
        }
        return Flow::Continue(0);
    }

    // 3. Expand the argument list into fields, then glob the unquoted ones.
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let args = expand_words_globbed(&cmd.args, vars, &cwd);
    let Some(name) = args.first().cloned() else {
        eprintln!("winsh: empty command");
        return Flow::Continue(2);
    };

    if is_builtin(&name) {
        return run_builtin(&name, &args[1..], cmd, vars);
    }

    // A function shadows an external command of the same name, as in POSIX.
    if let Some(body) = vars.functions.get(&name).cloned() {
        let saved = std::mem::replace(&mut vars.positional, args[1..].to_vec());
        let flow = run_compound(&body, vars);
        vars.positional = saved;
        return match flow {
            Flow::Return(code) => Flow::Continue(code),
            other => other,
        };
    }

    match spawn_pipeline(
        &Pipeline {
            cmds: vec![Cmd::Simple(Simple {
                assigns: Vec::new(),
                args: cmd.args.clone(),
                redirs: cmd.redirs.clone(),
            })],
        },
        vars,
        &assigns,
    ) {
        Ok(code) => Flow::Continue(code),
        Err(e) => {
            eprintln!("winsh: {e}");
            Flow::Continue(127)
        }
    }
}

fn spawn_pipeline(p: &Pipeline, vars: &mut Vars, extra_env: &[(String, String)]) -> Result<i32, String> {
    let mut children: Vec<Child> = Vec::new();
    let mut prev_stdout: Option<Stdio> = None;
    // When capturing, the final stage's stdout is piped to us instead of the
    // console; hold the handle so the output can be collected.
    let mut captured: Option<std::process::ChildStdout> = None;

    for (i, cmd) in p.cmds.iter().enumerate() {
        let Cmd::Simple(cmd) = cmd else {
            return Err("compound command in a pipeline is not supported yet".into());
        };
        let is_last = i + 1 == p.cmds.len();
        let (mut command, heredoc) = build_command(cmd, vars, extra_env, prev_stdout.take())?;
        if !is_last || capturing() {
            command.stdout(Stdio::piped());
        }
        let display_name = cmd
            .args
            .first()
            .and_then(|w| expand_word(w, vars).into_iter().next())
            .unwrap_or_default();
        let mut child = command
            .spawn()
            .map_err(|e| format!("{display_name}: {e}"))?;
        // Best-effort: a failure here only weakens the orphan guarantee.
        crate::job::assign(&child);
        // Feed the here-document, then close the pipe: the child sees EOF and
        // proceeds. Dropping the handle before waiting is what signals it.
        if let Some(body) = heredoc {
            if let Some(mut stdin) = child.stdin.take() {
                if let Err(e) = stdin.write_all(body.as_bytes()) {
                    eprintln!("winsh: here-document: {e}");
                }
            }
        }
        if !is_last {
            let out = child.stdout.take().ok_or("failed to take child stdout")?;
            prev_stdout = Some(Stdio::from(out));
        } else if capturing() {
            captured = child.stdout.take();
        }
        children.push(child);
    }

    // Drain the captured pipe before waiting: the child closes stdout when it
    // exits, so this both collects the output and avoids a full-pipe deadlock.
    if let Some(mut out) = captured {
        let mut buf = String::new();
        let _ = out.read_to_string(&mut buf);
        capture_write(&buf);
    }

    let mut status = 0;
    for mut child in children {
        let s = child.wait().map_err(|e| format!("wait: {e}"))?;
        status = s.code().unwrap_or(1);
    }
    Ok(status)
}

/// Builds a child process description. Returns the here-document body too,
/// because it has to be written to the child after it starts.
fn build_command(
    cmd: &Simple,
    vars: &mut Vars,
    extra_env: &[(String, String)],
    stdin: Option<Stdio>,
) -> Result<(Command, Option<String>), String> {
    let args = expand_words(&cmd.args, vars);
    let (name, rest) = args.split_first().ok_or("empty command in pipeline")?;
    let mut c = Command::new(name);
    c.args(rest);

    // The upstream pipe is applied first so that an explicit redirection can
    // still override it, which is the POSIX order.
    if let Some(s) = stdin {
        c.stdin(s);
    }

    // The child inherits our environment; exported variables and any
    // `VAR=value` prefix override entries in it.
    for (k, v) in vars.exported_pairs() {
        c.env(k, v);
    }
    for (k, v) in extra_env {
        c.env(k, v);
    }

    let mut heredoc: Option<String> = None;
    for r in &cmd.redirs {
        match r.kind {
            RedirKind::HereDoc => {
                // The body is a byte stream, not a list of words: expand the
                // pieces but never field-split them.
                let text = expand_word(&r.target, vars).join("");
                c.stdin(Stdio::piped());
                heredoc = Some(text);
            }
            RedirKind::Out => {
                let target = expand_word(&r.target, vars).join(" ");
                let f = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&target)
                    .map_err(|e| format!("{target}: {e}"))?;
                c.stdout(Stdio::from(f));
            }
            RedirKind::Append => {
                let target = expand_word(&r.target, vars).join(" ");
                let f = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&target)
                    .map_err(|e| format!("{target}: {e}"))?;
                c.stdout(Stdio::from(f));
            }
            RedirKind::In => {
                let target = expand_word(&r.target, vars).join(" ");
                let f = std::fs::File::open(&target).map_err(|e| format!("{target}: {e}"))?;
                c.stdin(Stdio::from(f));
            }
        }
    }
    Ok((c, heredoc))
}

fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        "echo"
            | "cd"
            | "pwd"
            | "exit"
            | "true"
            | "false"
            | ":"
            | "export"
            | "unset"
            | "set"
            | "test"
            | "["
            | "return"
            | "break"
            | "continue"
            | "trap"
    )
}

fn run_builtin(name: &str, args: &[String], cmd: &Simple, vars: &mut Vars) -> Flow {
    match name {
        ":" | "true" => Flow::Continue(0),
        "false" => Flow::Continue(1),
        "exit" => Flow::Exit(args.first().and_then(|a| a.parse().ok()).unwrap_or(0)),
        "pwd" => {
            let dir = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            emit(&format!("{dir}\n"), &cmd.redirs, vars)
        }
        "cd" => {
            let target = args.first().cloned().unwrap_or_else(home_dir);
            match std::env::set_current_dir(&target) {
                Ok(()) => Flow::Continue(0),
                Err(e) => {
                    eprintln!("winsh: cd: {target}: {e}");
                    Flow::Continue(1)
                }
            }
        }
        "export" => {
            if args.is_empty() {
                let mut out = String::new();
                for n in vars.names() {
                    if vars.is_exported(n) {
                        out.push_str(&format!("export {n}={}\n", vars.get(n).unwrap_or_default()));
                    }
                }
                return emit(&out, &cmd.redirs, vars);
            }
            for a in args {
                match a.split_once('=') {
                    Some((k, v)) => {
                        vars.set(k, v.to_string());
                        vars.export(k);
                    }
                    None => vars.export(a),
                }
            }
            Flow::Continue(0)
        }
        "unset" => {
            for a in args {
                vars.unset(a);
            }
            Flow::Continue(0)
        }
        "set" => {
            if args.is_empty() {
                let mut out = String::new();
                for n in vars.names() {
                    out.push_str(&format!("{n}={}\n", vars.get(n).unwrap_or_default()));
                }
                return emit(&out, &cmd.redirs, vars);
            }
            eprintln!("winsh: set: only the listing form is implemented");
            Flow::Continue(2)
        }
        "return" => {
            // Outside a function, `return` is an error in POSIX; we treat it
            // as a plain status so a stray one cannot kill the script.
            let code = args.first().and_then(|a| a.parse().ok()).unwrap_or(vars.status);
            Flow::Return(code)
        }
        "break" => Flow::Break(args.first().and_then(|a| a.parse().ok()).unwrap_or(1)),
        "continue" => Flow::ContinueLoop(args.first().and_then(|a| a.parse().ok()).unwrap_or(1)),
        "trap" => trap_builtin(args, cmd, vars),
        "test" | "[" => {
            // `[` requires the closing `]`; strip it before evaluating.
            let mut a = args.to_vec();
            if name == "[" {
                if a.last().map(|s| s.as_str()) != Some("]") {
                    eprintln!("winsh: [: missing ']'");
                    return Flow::Continue(2);
                }
                a.pop();
            }
            match crate::test_cmd::evaluate(&a) {
                Ok(true) => Flow::Continue(0),
                Ok(false) => Flow::Continue(1),
                Err(e) => {
                    eprintln!("winsh: {e}");
                    Flow::Continue(2)
                }
            }
        }
        "echo" => {
            let mut newline = true;
            let mut start = 0;
            if args.first().map(|s| s.as_str()) == Some("-n") {
                newline = false;
                start = 1;
            }
            let mut out = args[start..].join(" ");
            if newline {
                out.push('\n');
            }
            emit(&out, &cmd.redirs, vars)
        }
        other => {
            eprintln!("winsh: {other}: unknown builtin");
            Flow::Continue(2)
        }
    }
}

/// `trap [action] [condition...]`.
///
/// Only `EXIT` (or `0`) is accepted. Naming a real signal is an error rather
/// than a silent no-op: a script that thinks it installed a SIGINT handler and
/// did not would fail in the least debuggable way possible.
fn trap_builtin(args: &[String], cmd: &Simple, vars: &mut Vars) -> Flow {
    if args.is_empty() {
        let mut out = String::new();
        if let Some(t) = vars.traps.get("EXIT") {
            out.push_str(&format!("trap -- '{}' EXIT\n", t.source));
        }
        return emit(&out, &cmd.redirs, vars);
    }
    let action = &args[0];
    let conditions = &args[1..];
    if conditions.is_empty() {
        eprintln!("winsh: trap: usage: trap [action] EXIT");
        return Flow::Continue(2);
    }
    let mut status = 0;
    for cond in conditions {
        if !is_exit_condition(cond) {
            eprintln!(
                "winsh: trap: '{cond}': only EXIT is supported — Windows has no POSIX signals"
            );
            status = 1;
            continue;
        }
        // `trap - EXIT` resets, and `trap '' EXIT` (ignore) is equivalent for
        // EXIT because there is nothing to ignore: it simply does not run.
        if action == "-" || action.is_empty() {
            vars.traps.remove("EXIT");
            continue;
        }
        match crate::lex::lex(action).and_then(|t| crate::parse::parse(&t)) {
            Ok(body) => {
                vars.traps.insert(
                    "EXIT".to_string(),
                    crate::var::Trap {
                        source: action.clone(),
                        body,
                    },
                );
            }
            Err(e) => {
                eprintln!("winsh: trap: {e}");
                status = 2;
            }
        }
    }
    Flow::Continue(status)
}

fn is_exit_condition(cond: &str) -> bool {
    matches!(cond, "0" | "EXIT" | "exit")
}

/// Runs the `EXIT` trap, if one is set, and returns the status the process
/// should exit with.
///
/// `$?` inside the action is the status being exited with. An `exit` inside the
/// action overrides that status; any other outcome leaves it alone — a cleanup
/// action must not be able to silently change the script's result.
pub fn run_exit_trap(vars: &mut Vars, code: i32) -> i32 {
    // Removing it first makes an `exit` inside the action terminate instead of
    // re-entering this function.
    let Some(trap) = vars.traps.remove("EXIT") else {
        return code;
    };
    vars.status = code;
    match run_flow(&trap.body, vars) {
        Flow::Exit(c) => c,
        _ => code,
    }
}

fn emit(text: &str, redirs: &[Redir], vars: &mut Vars) -> Flow {
    for r in redirs {
        if !matches!(r.kind, RedirKind::Out | RedirKind::Append) {
            continue;
        }
        let target = expand_word(&r.target, vars).join(" ");
        let res = if matches!(r.kind, RedirKind::Append) {
            OpenOptions::new().append(true).create(true).open(&target)
        } else {
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&target)
        };
        match res {
            Ok(mut f) => {
                if let Err(e) = f.write_all(text.as_bytes()) {
                    eprintln!("winsh: {target}: {e}");
                    return Flow::Continue(1);
                }
                return Flow::Continue(0);
            }
            Err(e) => {
                eprintln!("winsh: {target}: {e}");
                return Flow::Continue(1);
            }
        }
    }
    if capturing() {
        capture_write(text);
    } else {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
    Flow::Continue(0)
}

fn home_dir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::lex;
    use crate::parse::parse;

    fn list(src: &str) -> List {
        parse(&lex(src).unwrap_or_else(|e| panic!("{src}: {e}"))).unwrap_or_else(|e| panic!("{src}: {e}"))
    }

    /// Runs a whole script the way `main` does, and returns the status and
    /// everything written to standard output.
    fn run_script(src: &str) -> (i32, String) {
        let mut vars = Vars::from_env("winsh".into(), vec![]);
        CAPTURE.with(|c| *c.borrow_mut() = Some(String::new()));
        let code = run(&list(src), &mut vars);
        let code = run_exit_trap(&mut vars, code);
        let out = CAPTURE.with(|c| c.borrow_mut().take()).unwrap_or_default();
        (code, out)
    }

    #[test]
    fn exit_trap_runs_when_the_script_ends() {
        assert_eq!(run_script("trap 'echo bye' EXIT; echo main"), (0, "main\nbye\n".into()));
    }

    #[test]
    fn exit_trap_sees_the_status_being_exited_with() {
        assert_eq!(run_script("trap 'echo s=$?' EXIT; exit 7"), (7, "s=7\n".into()));
    }

    #[test]
    fn exit_trap_can_override_the_status() {
        assert_eq!(run_script("trap 'exit 3' EXIT; exit 7"), (3, String::new()));
    }

    #[test]
    fn a_failing_trap_does_not_change_the_status() {
        assert_eq!(run_script("trap 'false' EXIT; exit 7"), (7, String::new()));
    }

    #[test]
    fn trap_can_be_reset() {
        assert_eq!(run_script("trap 'echo bye' EXIT; trap - EXIT; echo after"), (0, "after\n".into()));
    }

    #[test]
    fn trap_zero_is_the_exit_trap() {
        assert_eq!(run_script("trap 'echo zero' 0; echo body"), (0, "body\nzero\n".into()));
    }

    #[test]
    fn the_last_trap_wins() {
        assert_eq!(run_script("trap 'echo A' EXIT; trap 'echo B' EXIT"), (0, "B\n".into()));
    }

    #[test]
    fn a_real_signal_is_refused() {
        // Better a loud error than a handler that silently never runs.
        assert_eq!(run_script("trap 'echo x' INT"), (1, String::new()));
    }

    #[test]
    fn exit_trap_runs_after_the_last_command_fails() {
        assert_eq!(run_script("trap 'echo s=$?' EXIT; false"), (1, "s=1\n".into()));
    }
}
