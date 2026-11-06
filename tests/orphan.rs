//! Regression test for the orphan guarantee (DESIGN.md §5.3).
//!
//! Killing the shell must not leave children running. Before the session job
//! existed this test failed: a force-killed winsh left its `ping` child alive.
//!
//! The probe is a renamed copy of ping.exe so the process search cannot match
//! an unrelated ping started by the machine or by another test.

#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const PROBE: &str = "winsh-orphan-probe.exe";

fn probe_path() -> PathBuf {
    std::env::temp_dir().join(PROBE)
}

/// Counts live probe processes. `tasklist` exits non-zero when nothing
/// matches, so the count comes from the output text, not the status.
fn probe_count() -> usize {
    let out = Command::new("tasklist")
        .args(["/fi", &format!("imagename eq {PROBE}"), "/nh"])
        .output()
        .expect("run tasklist");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.to_ascii_uppercase().contains("WINSH-ORPHAN-PROBE"))
        .count()
}

fn install_probe() {
    let system_ping = PathBuf::from(r"C:\Windows\System32\PING.EXE");
    std::fs::copy(&system_ping, probe_path()).expect("copy ping.exe to probe name");
}

#[test]
fn killing_the_shell_kills_its_children() {
    install_probe();

    let mut shell = Command::new(env!("CARGO_BIN_EXE_winsh"))
        // Absolute path: the probe lives in TEMP, which is not on PATH.
        .args(["-c", &format!("\"{}\" -n 20 127.0.0.1", probe_path().display())])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn winsh");

    // Give the shell time to spawn its child and assign it to the job.
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        probe_count() >= 1,
        "the child should be running before we kill the shell"
    );

    shell.kill().expect("kill winsh");
    let _ = shell.wait();

    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        probe_count(),
        0,
        "the child must not survive the shell — orphan guarantee broken"
    );

    let _ = std::fs::remove_file(probe_path());
}
