//! `ferric-unlock`'s exit status and messages for the tables that start no daemon (Cargo
//! feature `unlock`).

#![cfg(feature = "unlock")]

use std::process::{Command, Output};

fn run(table: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ferric-unlock"))
        .arg(table)
        .output()
        .expect("ferric-unlock runs")
}

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("ferric-unlock-cli-{}-{name}", std::process::id()))
}

#[test]
fn a_missing_table_unlocks_nothing_and_succeeds() {
    let out = run(temp("missing").to_str().expect("UTF-8"));
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing to unlock"));
}

#[test]
fn an_empty_table_succeeds() {
    let path = temp("empty");
    std::fs::write(&path, "# no volumes yet\n").expect("written");
    let out = run(path.to_str().expect("UTF-8"));
    std::fs::remove_file(&path).expect("removed");
    assert!(out.status.success());
    assert!(out.stdout.is_empty() && out.stderr.is_empty());
}

#[test]
fn a_bad_table_starts_nothing_and_fails_with_the_whole_reason() {
    let path = temp("bad");
    std::fs::write(&path, "ok dev:/x\nbad dev:/y redoxfs:root\n").expect("written");
    let out = run(path.to_str().expect("UTF-8"));
    std::fs::remove_file(&path).expect("removed");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("nothing unlocked: line 2: \"redoxfs:root\": RedoxFS needs"),
        "{err}"
    );
}
