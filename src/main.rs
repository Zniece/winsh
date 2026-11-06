//! winsh — a small POSIX-sh-level shell core for Windows.
//!
//! Entry points for now:
//!   winsh -c <command>     run a command string
//!   winsh <script> [args]  run a script file
//!
//! Interactive mode is not implemented yet.

mod arith;
mod exec;
mod expand;
mod glob;
mod job;
mod lex;
mod parse;
mod test_cmd;
mod var;

use std::process::ExitCode;

fn main() -> ExitCode {
    // Take ownership of every child this shell will ever spawn, so an abnormal
    // death cannot leave orphans behind (DESIGN.md §5.3).
    job::init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    let (source, script_name, positional) = match args.as_slice() {
        [] => {
            eprintln!("usage: winsh -c <command> | winsh <script> [args]");
            return ExitCode::from(2);
        }
        [flag, cmd, rest @ ..] if flag == "-c" => {
            (cmd.clone(), "-c".to_string(), rest.to_vec())
        }
        [path, rest @ ..] => match std::fs::read_to_string(path) {
            Ok(s) => (s, path.clone(), rest.to_vec()),
            Err(e) => {
                eprintln!("winsh: {path}: {e}");
                return ExitCode::from(1);
            }
        },
    };

    let tokens = match lex::lex(&source) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("winsh: syntax error: {e}");
            return ExitCode::from(2);
        }
    };
    let list = match parse::parse(&tokens) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("winsh: parse error: {e}");
            return ExitCode::from(2);
        }
    };

    let mut vars = var::Vars::from_env(script_name, positional);
    let code = exec::run(&list, &mut vars);
    // The EXIT trap runs on every way out of the script: falling off the end,
    // `exit N`, and a failed command that the script does not check.
    let code = exec::run_exit_trap(&mut vars, code);
    ExitCode::from(code as u8)
}
