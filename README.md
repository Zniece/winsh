# winsh

A small POSIX-sh-level shell core for Windows. Target scale is a small core,
roughly 25-40k lines.

**Status: first light.** Lexer, parser and executor handle the basics; most of
POSIX is still missing. Nothing here is stable.

## Design contract

The governing design document is DESIGN.md, one level above this
repository — development documentation, not part of the repo. Its rule:
"UNIX style, Windows semantics". Read it before adding anything. The short
version:

- Style (CLI shape, options, output, exit codes) is copied from UNIX.
- Semantics (mechanisms, capabilities, limits) are Windows-native. No emulation.
- Delegate what a child process can do; keep in-process only what must be
  in-process, and own natively only what needs the process relationship.

winsh owns the language. `winuxcmd` owns files and text. `winadm` owns
processes and system. No command is known to the shell by name.

## Scope

Implemented: quoting, parameter expansion (`$x`, `${x}`, `$?`, `$$`, `$#`,
`$0`..`$9`), field splitting, pathname expansion, arithmetic `$(( ))`, command
substitution `$( )`, assignment and `export`/`unset`, pipes, redirections
(including here-documents `<<` / `<<-`), `&&`/`||`/`;`, `if`/`for`/`while`/
`until`/`case`, functions with `return`/`break`/`continue`, `trap EXIT`,
scripts, and the orphan guarantee (Job Object).

Out: arrays, `[[ ]]`, process substitution, coprocesses, brace expansion,
extglob, `declare`, completion/key bindings (the editor's job), and every
system builtin that belongs to winadm.

### `trap`: signals are refused, EXIT is honoured

`trap` accepts `EXIT` (or `0`) only. Naming a real signal is an **error**, not
a silent no-op — a script that believes it installed a SIGINT handler and did
not would fail in the least debuggable way possible. Windows has no POSIX
signals; when winadm grows console-event handling, it will be a native
mechanism, not a `trap` emulation.

### The one expansion rule worth stating

An **unquoted** expansion splits on whitespace, a quoted one does not:

```
x="a b"; printf '<%s>' $x    ->  <a><b>
x="a b"; printf '<%s>' "$x"   ->  <a b>
```

## Backslash: the one deliberate POSIX deviation

A backslash escapes only characters the shell gives meaning to — whitespace,
`| & ; < > ( ) ' " \` $ \ * ? [ ] ! #`. Before anything else it is literal, so
Windows paths work unquoted:

```
winsh -c 'ls C:\Users\me\src | wc -l'
winsh -c 'echo ..\..\foo'
```

POSIX quoting would turn those into `C:Usersme...`. The cost is that
`echo a\b` prints `a\b` instead of `ab`.

Caveat: `\\` still collapses to one backslash, so an unquoted UNC path loses a
separator — write `'\\server\share'` or `//server/share`.

## Build

```
cargo build --release
winsh -c 'ls -la | grep txt'
```

No dependencies, no network, no build script.

## Testing

Two kinds, because they make different claims:

- `cargo test` — unit tests. These verify "matches my understanding of POSIX".
- `bash difftest/run.sh` — differential tests (`difftest/cases/*.sh`, each run
  through both bash and winsh). These verify "matches bash", which is the
  claim real scripts depend on. stderr is not compared; message wording is not
  a goal.
