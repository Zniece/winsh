//! Shell variables and the parameters that expansion can read.
//!
//! Special parameters are computed, not stored: `$?` is the last status, `$$`
//! this process, `$#` the positional count, `$0`..`$9` the positional list.

use std::collections::{HashMap, HashSet};

use crate::parse::{Command, List};

/// A `trap` action: the text as written (for `trap` with no arguments) and the
/// parsed form (so a syntax error is reported when the trap is set, not when
/// it finally runs).
#[derive(Clone)]
pub struct Trap {
    pub source: String,
    pub body: List,
}

#[derive(Clone)]
pub struct Vars {
    map: HashMap<String, String>,
    exported: HashSet<String>,
    pub status: i32,
    pub script_name: String,
    pub positional: Vec<String>,
    /// Shell functions. Stored here (not in a separate table) so a command
    /// substitution's variable copy carries them, which is what a subshell
    /// sees.
    pub functions: HashMap<String, Command>,
    /// Only `EXIT` is honoured. Windows has no POSIX signals, and pretending
    /// to catch them would be a lie (DESIGN.md, the honesty rule).
    pub traps: HashMap<String, Trap>,
}

impl Vars {
    /// Seeds the table from the process environment, all of it exported, as a
    /// shell does at startup.
    pub fn from_env(script_name: String, positional: Vec<String>) -> Self {
        let map: HashMap<String, String> = std::env::vars().collect();
        let exported = map.keys().cloned().collect();
        Vars {
            map,
            exported,
            status: 0,
            script_name,
            positional,
            functions: HashMap::new(),
            traps: HashMap::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<String> {
        match name {
            "?" => Some(self.status.to_string()),
            "$" => Some(std::process::id().to_string()),
            "#" => Some(self.positional.len().to_string()),
            "0" => Some(self.script_name.clone()),
            "@" | "*" => Some(self.positional.join(" ")),
            n if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() => {
                let idx: usize = n.parse().ok()?;
                Some(self.positional.get(idx.wrapping_sub(1)).cloned().unwrap_or_default())
            }
            _ => self.map.get(name).cloned(),
        }
    }

    pub fn set(&mut self, name: &str, value: String) {
        self.map.insert(name.to_string(), value);
    }

    pub fn unset(&mut self, name: &str) {
        self.map.remove(name);
        self.exported.remove(name);
    }

    pub fn export(&mut self, name: &str) {
        if self.map.contains_key(name) {
            self.exported.insert(name.to_string());
        }
    }

    pub fn is_exported(&self, name: &str) -> bool {
        self.exported.contains(name)
    }

    /// Variables a child process must see, on top of the inherited
    /// environment.
    pub fn exported_pairs(&self) -> Vec<(String, String)> {
        self.exported
            .iter()
            .filter_map(|k| self.map.get(k).map(|v| (k.clone(), v.clone())))
            .collect()
    }

    /// All names, sorted, for `set` with no arguments.
    pub fn names(&self) -> Vec<&String> {
        let mut v: Vec<&String> = self.map.keys().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        let mut v = Vars::from_env("winsh".into(), vec!["a".into(), "b".into()]);
        v.set("name", "world".into());
        v
    }

    #[test]
    fn reads_plain_variables() {
        assert_eq!(vars().get("name").as_deref(), Some("world"));
        assert_eq!(vars().get("nope"), None);
    }

    #[test]
    fn reads_special_parameters() {
        let mut v = vars();
        v.status = 7;
        assert_eq!(v.get("?").as_deref(), Some("7"));
        assert_eq!(v.get("#").as_deref(), Some("2"));
        assert_eq!(v.get("0").as_deref(), Some("winsh"));
        assert_eq!(v.get("1").as_deref(), Some("a"));
        assert_eq!(v.get("2").as_deref(), Some("b"));
        assert_eq!(v.get("9").as_deref(), Some(""));
        assert_eq!(v.get("$").as_deref(), Some(std::process::id().to_string().as_str()));
    }

    #[test]
    fn export_is_opt_in() {
        let mut v = vars();
        assert!(!v.is_exported("name"));
        v.export("name");
        assert!(v.is_exported("name"));
        assert!(v
            .exported_pairs()
            .iter()
            .any(|(k, val)| k == "name" && val == "world"));
    }

    #[test]
    fn unset_clears_export_too() {
        let mut v = vars();
        v.export("name");
        v.unset("name");
        assert_eq!(v.get("name"), None);
        assert!(!v.is_exported("name"));
    }
}
