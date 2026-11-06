//! POSIX pattern matching and pathname expansion (globbing).
//!
//! Scope is deliberately POSIX: `*`, `?`, `[...]`, and backslash escaping.
//! No extglob, no brace expansion, no case modifiers — DESIGN.md puts them
//! out of scope.
//!
//! Backslash rule on Windows: a backslash before a glob metacharacter escapes
//! it, anywhere else it is a path separator. That mirrors the lexer's rule
//! (`C:\Users` keeps its separators) and is what makes unquoted Windows paths
//! work in patterns at all.

use std::path::{Path, PathBuf};

const GLOB_META: &[char] = &['*', '?', '['];

/// True when the text contains a metacharacter that is not backslash-escaped.
pub fn has_meta(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && GLOB_META.contains(&chars[i + 1]) {
            i += 2;
            continue;
        }
        if GLOB_META.contains(&chars[i]) {
            return true;
        }
        i += 1;
    }
    false
}

/// Matches `text` against `pattern`.
pub fn matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    match_from(&p, 0, &t, 0)
}

fn match_from(p: &[char], mut pi: usize, t: &[char], mut ti: usize) -> bool {
    // Iterative on `*` with a backtrack point: linear-ish instead of
    // exponential on patterns like `a*a*a*a*b`.
    let mut star: Option<(usize, usize)> = None;

    loop {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    // Collapse runs of stars, then try the shortest match first.
                    while pi < p.len() && p[pi] == '*' {
                        pi += 1;
                    }
                    if pi == p.len() {
                        return true;
                    }
                    star = Some((pi, ti));
                    continue;
                }
                '?' => {
                    if ti < t.len() {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                '[' => {
                    if ti < t.len() {
                        if let Some((ok, next)) = match_class(p, pi, t[ti]) {
                            if ok {
                                pi = next;
                                ti += 1;
                                continue;
                            }
                        }
                    }
                }
                '\\' if pi + 1 < p.len() && GLOB_META.contains(&p[pi + 1]) => {
                    if ti < t.len() && t[ti] == p[pi + 1] {
                        pi += 2;
                        ti += 1;
                        continue;
                    }
                }
                c => {
                    if ti < t.len() && t[ti] == c {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
            }
        } else if ti == t.len() {
            return true;
        }

        // Mismatch: retreat to the last `*` and let it swallow one more char.
        match star {
            Some((sp, st)) if st < t.len() => {
                star = Some((sp, st + 1));
                pi = sp;
                ti = st + 1;
            }
            _ => return false,
        }
    }
}

/// Matches a `[...]` class starting at `p[start] == '['`.
/// Returns (matched, index just past the closing bracket).
fn match_class(p: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let mut negate = false;
    if i < p.len() && (p[i] == '!' || p[i] == '^') {
        negate = true;
        i += 1;
    }
    let mut matched = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((matched != negate, i + 1));
        }
        first = false;
        // Range like a-z.
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            if p[i] <= c && c <= p[i + 2] {
                matched = true;
            }
            i += 3;
        } else {
            if p[i] == c {
                matched = true;
            }
            i += 1;
        }
    }
    // Unterminated class: treat the '[' as a literal.
    None
}

/// Strips backslashes that escape metacharacters, for literal use.
fn unescape(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && GLOB_META.contains(&chars[i + 1]) {
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Expands a pattern into matching paths, sorted.
///
/// Returns an empty vector when nothing matches; the caller decides whether
/// that means "use the word literally" (POSIX default) or an error.
pub fn expand(pattern: &str, cwd: &Path) -> Vec<String> {
    if !has_meta(pattern) {
        return Vec::new();
    }

    // Split into components on separators. A backslash followed by a
    // metacharacter is an escape and stays inside the component.
    let (absolute, prefix, comps, sep) = split_components(pattern);

    let mut current: Vec<PathBuf> = if absolute {
        vec![prefix.clone()]
    } else {
        vec![cwd.to_path_buf()]
    };

    for comp in &comps {
        let mut next: Vec<PathBuf> = Vec::new();
        for base in &current {
            if has_meta(comp) {
                let Ok(entries) = std::fs::read_dir(base) else {
                    continue;
                };
                let mut names: Vec<String> = Vec::new();
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // A leading dot is matched only by a pattern that starts
                    // with one, POSIX rule.
                    if name.starts_with('.') && !comp.starts_with('.') {
                        continue;
                    }
                    if matches(comp, &name) {
                        names.push(name);
                    }
                }
                names.sort();
                for n in names {
                    next.push(base.join(n));
                }
            } else {
                let literal = unescape(comp);
                let candidate = base.join(&literal);
                if candidate.exists() {
                    next.push(candidate);
                }
            }
        }
        current = next;
        if current.is_empty() {
            return Vec::new();
        }
    }

    let mut out: Vec<String> = current
        .iter()
        .map(|p| render(p, absolute, cwd, sep))
        .collect();
    out.sort();
    out
}

fn split_components(pattern: &str) -> (bool, PathBuf, Vec<String>, char) {
    let chars: Vec<char> = pattern.chars().collect();
    let absolute = pattern.starts_with('/')
        || (chars.len() >= 3 && chars[0].is_ascii_alphabetic() && chars[1] == ':' && (chars[2] == '\\' || chars[2] == '/'));

    let prefix = if absolute {
        if chars.len() >= 3 && chars[1] == ':' {
            PathBuf::from(format!("{}:\\", chars[0]))
        } else {
            PathBuf::from("\\")
        }
    } else {
        PathBuf::new()
    };

    let mut comps = Vec::new();
    let mut cur = String::new();
    let mut sep = '\\';
    let mut i = 0;
    // Skip the prefix we already consumed.
    if absolute {
        i = if chars.len() >= 3 && chars[1] == ':' { 3 } else { 1 };
    }
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && GLOB_META.contains(&chars[i + 1]) {
            cur.push(c);
            cur.push(chars[i + 1]);
            i += 2;
            continue;
        }
        if c == '/' || c == '\\' {
            sep = c;
            if !cur.is_empty() {
                comps.push(std::mem::take(&mut cur));
            }
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    if !cur.is_empty() {
        comps.push(cur);
    }
    (absolute, prefix, comps, sep)
}

/// Renders a matched path back the way the user wrote it: a relative pattern
/// yields a relative result, an absolute one keeps its drive or root.
fn render(path: &Path, absolute: bool, cwd: &Path, sep: char) -> String {
    let raw = if absolute {
        path.to_string_lossy().into_owned()
    } else {
        match path.strip_prefix(cwd) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    };
    // Echo back the separator the user wrote, so `sub/*.txt` yields
    // `sub/x.txt` and `sub\*.txt` yields `sub\x.txt`. Neither surprises.
    if sep == '/' {
        raw.replace('\\', "/")
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_matches_itself() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abd"));
        assert!(!matches("abc", "ab"));
    }

    #[test]
    fn star_matches_any_run() {
        assert!(matches("*", ""));
        assert!(matches("*", "anything"));
        assert!(matches("a*c", "abc"));
        assert!(matches("a*c", "ac"));
        assert!(matches("a*c", "axxxc"));
        assert!(!matches("a*c", "abx"));
        assert!(matches("*.txt", "file.txt"));
        assert!(!matches("*.txt", "file.txtx"));
    }

    #[test]
    fn question_matches_exactly_one() {
        assert!(matches("a?c", "abc"));
        assert!(!matches("a?c", "ac"));
        assert!(!matches("a?c", "abbc"));
    }

    #[test]
    fn classes() {
        assert!(matches("[abc]", "b"));
        assert!(!matches("[abc]", "d"));
        assert!(matches("[a-z]", "q"));
        assert!(!matches("[a-z]", "Q"));
        assert!(matches("[!abc]", "d"));
        assert!(!matches("[!abc]", "a"));
        assert!(matches("[a-cx-z]", "y"));
        assert!(matches("[]abc]", "]"));
    }

    #[test]
    fn escapes_are_literal() {
        assert!(matches(r"a\*b", "a*b"));
        assert!(!matches(r"a\*b", "axb"));
        assert!(!has_meta(r"a\*b"));
        assert!(has_meta("a*b"));
    }

    #[test]
    fn star_backtracking_is_not_exponential() {
        // Would hang a naive recursive matcher.
        assert!(matches("a*a*a*a*a*b", &format!("{}b", "a".repeat(200))));
        assert!(!matches("a*a*a*a*a*b", &"a".repeat(500)));
    }
}
