//! Binary smoke tests: the CLI launches and its arg surface is wired.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_hrdr")
}

#[test]
fn prints_version() {
    let out = Command::new(bin()).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("hrdr"));
}

#[test]
fn prints_help() {
    let out = Command::new(bin()).arg("--help").output().unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("harness"));
    assert!(s.contains("run"));
}

#[test]
fn run_requires_a_prompt() {
    // `run` with no prompt is a usage error (clap: required trailing arg).
    let out = Command::new(bin()).arg("run").output().unwrap();
    assert!(!out.status.success());
}
