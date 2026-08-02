//! The half of the colour decision a pipe cannot test.
//!
//! `headless.rs` captures stderr, so it only ever sees the uncoloured branch —
//! and a test suite that never runs the coloured one would report a healthy
//! green while colour was broken for every real user. So: a pty, where stderr
//! genuinely is a terminal, and the two ways a user turns colour back off.

// Its own test binary, so it links the sandbox ctor itself — see `tui_pty.rs`.
extern crate hrdr_test_support;

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Chat, MockServer, drain_pty, pty_text, text_chunk};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Generous: a cold runner is slow, and a flaky timeout is worse than a slow test.
const DONE: Duration = Duration::from_secs(60);

fn pty_available() -> bool {
    native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 90,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

/// Skip for want of a pty — never in CI, where a missing pty is a broken
/// environment rather than a local sandbox blocking `/dev/ptmx`.
fn skip_for_want_of_a_pty() -> bool {
    if pty_available() || std::env::var_os("CI").is_some() {
        return false;
    }
    eprintln!("skipping: no pty available (a Landlock sandbox blocks /dev/ptmx)");
    true
}

/// Run one headless turn with stdout+stderr on a pty, and return everything the
/// terminal received. `env` adds to the child's environment.
///
/// stdout and stderr share the pty here, which is exactly the situation the
/// colour decision is about: a person watching a terminal.
fn run_on_a_pty(env: &[(&str, &str)]) -> String {
    let server = MockServer::start(vec![Chat::Sse(vec![
        text_chunk("c1", "Hello."),
        common::stop_chunk("c1"),
        "[DONE]".to_string(),
    ])]);
    let home = tempfile::tempdir().expect("temp home");
    let project = tempfile::tempdir().expect("temp project");
    let config_dir = home.path().join("hrdr");
    std::fs::create_dir_all(&config_dir).expect("config dir");
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "model = \"mock://m\"\n\n[providers.mock]\nbase_url = \"{}\"\napi_key = \"k\"\n",
            server.base_url()
        ),
    )
    .expect("write config.toml");

    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("open pty");

    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_hrdr"));
    cmd.args(["run", "say hello"]);
    cmd.cwd(project.path());
    for (key, value) in [
        ("HOME", home.path()),
        ("USERPROFILE", home.path()),
        ("APPDATA", home.path()),
        ("LOCALAPPDATA", home.path()),
        ("XDG_CONFIG_HOME", home.path()),
        ("XDG_DATA_HOME", home.path()),
        ("XDG_STATE_HOME", home.path()),
        ("XDG_CACHE_HOME", home.path()),
    ] {
        cmd.env(key, value);
    }
    cmd.env("TERM", "xterm-256color");
    for key in ["HRDR_MODEL", "HRDR_API_KEY", "RUST_LOG", "NO_COLOR"] {
        cmd.env_remove(key);
    }
    for (key, value) in env {
        cmd.env(key, value);
    }

    let mut child = pty.slave.spawn_command(cmd).expect("spawn hrdr");
    drop(pty.slave);
    let reader = pty.master.try_clone_reader().expect("pty reader");
    // The shared drainer, not a bare read loop: on Windows the child blocks on a
    // cursor-position query until the harness answers it.
    let writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(pty.master.take_writer().expect("pty writer")));
    let seen = drain_pty(reader, writer);

    let deadline = Instant::now() + DONE;
    while Instant::now() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    // Let the reader drain what the child wrote before it exited.
    std::thread::sleep(Duration::from_millis(200));
    pty_text(&seen)
}

/// On a terminal, the chrome is coloured. The counterpart to
/// `captured_stderr_carries_no_escape_codes`: between them, both branches of the
/// decision are exercised, and neither can rot unnoticed.
#[test]
fn chrome_on_a_terminal_is_coloured() {
    if skip_for_want_of_a_pty() {
        return;
    }
    let seen = run_on_a_pty(&[]);
    assert!(
        seen.contains("[usage]"),
        "the chrome this test is about must have run: {seen:?}"
    );
    assert!(seen.contains('\x1b'), "a terminal gets colour: {seen:?}");
}

/// `NO_COLOR` turns it off even on a terminal — the convention hrdr already
/// imposes on every subprocess it spawns, now honoured for its own output.
#[test]
fn no_color_turns_it_off_on_a_terminal() {
    if skip_for_want_of_a_pty() {
        return;
    }
    let seen = run_on_a_pty(&[("NO_COLOR", "1")]);
    assert!(
        seen.contains("[usage]"),
        "the chrome this test is about must have run: {seen:?}"
    );
    assert!(
        !seen.contains('\x1b'),
        "NO_COLOR is honoured on a terminal: {seen:?}"
    );
}

/// `TERM=dumb` likewise — an editor's shell-mode buffer is a terminal that
/// cannot render any of it.
#[test]
fn term_dumb_turns_it_off_on_a_terminal() {
    if skip_for_want_of_a_pty() {
        return;
    }
    let seen = run_on_a_pty(&[("TERM", "dumb")]);
    assert!(
        seen.contains("[usage]"),
        "the chrome this test is about must have run: {seen:?}"
    );
    assert!(!seen.contains('\x1b'), "TERM=dumb is honoured: {seen:?}");
}
