//! `serial send --secret` on a terminal: echo is off while the secret is read, and back on
//! afterwards, also when Ctrl+C ends the command. Runs the binary with a pseudo-terminal as its
//! standard input; no harness is running, so the command fails after reading, which is fine.

use std::fs::File;
use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fd::OwnedFd;
use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
use rustix::termios::{LocalModes, tcgetattr};

/// A pseudo-terminal: the master this test holds, and the slave the command reads from.
fn terminal() -> (OwnedFd, File) {
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("openpt");
    grantpt(&master).expect("grantpt");
    unlockpt(&master).expect("unlockpt");
    let name = ptsname(&master, Vec::new()).expect("ptsname");
    let slave = File::options()
        .read(true)
        .write(true)
        .open(name.to_str().expect("a UTF-8 path"))
        .expect("open the slave");
    (master, slave)
}

fn echo(master: &OwnedFd) -> bool {
    tcgetattr(master)
        .expect("tcgetattr")
        .local_modes
        .contains(LocalModes::ECHO)
}

/// Starts `serial send --secret` reading from `slave`, and waits until it has turned echo off.
fn start(master: &OwnedFd, slave: &File) -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_ferric-harness"))
        .args([
            "serial",
            "send",
            "--secret",
            "--run",
            "/nonexistent-ferric-run",
        ])
        .stdin(slave.try_clone().expect("dup the slave"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("run ferric-harness");
    let deadline = Instant::now() + Duration::from_secs(10);
    while echo(master) {
        assert!(Instant::now() < deadline, "echo was never turned off");
        thread::sleep(Duration::from_millis(10));
    }
    child
}

#[test]
fn echo_is_off_while_the_secret_is_read_and_back_after() {
    let (master, slave) = terminal();
    assert!(echo(&master), "a new terminal echoes");
    let mut child = start(&master, &slave);
    File::from(master.try_clone().expect("dup the master"))
        .write_all(b"hunter2\n")
        .expect("type the secret");
    let status = child.wait().expect("wait");
    // No harness is running, so sending fails, after the read.
    assert!(!status.success());
    assert!(echo(&master), "echo is back after the read");
}

#[test]
fn ctrl_c_during_the_read_restores_echo_and_still_ends_by_sigint() {
    let (master, slave) = terminal();
    let mut child = start(&master, &slave);
    // Ctrl+C on a terminal in its default modes: the line discipline sends SIGINT. The child
    // is not the terminal's foreground process, so send the signal the key would.
    rustix::process::kill_process(
        rustix::process::Pid::from_child(&child),
        rustix::process::Signal::INT,
    )
    .expect("SIGINT");
    let status = child.wait().expect("wait");
    assert_eq!(status.signal(), Some(rustix::process::Signal::INT.as_raw()));
    assert!(echo(&master), "echo is back after Ctrl+C");
}
