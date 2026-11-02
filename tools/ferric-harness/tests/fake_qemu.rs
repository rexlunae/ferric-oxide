//! End-to-end tests against a fake QEMU.
//!
//! The test binary doubles as the fake: the harness is pointed at this executable with
//! wrapper arguments that make libtest run only `fake_qemu_process`, and the QEMU command
//! line follows `--`. The fake serves QMP and serial sockets where the real QEMU would,
//! plays a scripted console (`-fake-script <json>`), renders screenshots of scripted text,
//! logs the input events it receives, and powers off or quits like QEMU.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ferric_harness::bootenv::BootEnvVia;
use ferric_harness::input::{Button, ButtonAction, PointerAction};
use ferric_harness::machine::{Display, LaunchConfig, Mode, OnCrash};
use ferric_harness::markers::Marker;
use ferric_harness::report::Report;
use ferric_harness::serial::Eol;
use ferric_harness::session::{Session, Since};
use ferric_harness::verdict::{CheckStatus, Outcome, StopReason};
use serde_json::{Value, json};

/// The fake QEMU's entry point; a no-op in a normal test run.
#[test]
fn fake_qemu_process() {
    let args: Vec<String> = std::env::args().collect();
    let Some(split) = args.iter().position(|a| a == "--") else {
        return;
    };
    let qemu_args = &args[split + 1..];
    if !qemu_args.iter().any(|a| a == "-qmp") {
        return;
    }
    fake::run(qemu_args);
}

mod fake {
    use super::*;

    fn value_after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    fn option(spec: &str, key: &str) -> Option<String> {
        // QEMU doubles commas inside values.
        let spec = spec.replace(",,", "\u{0}");
        spec.split(',')
            .find_map(|part| part.strip_prefix(&format!("{key}=")))
            .map(|v| v.replace('\u{0}', ","))
    }

    struct Shared {
        qmp: Mutex<Option<UnixStream>>,
        serial: Mutex<Option<UnixStream>>,
        keys: Mutex<VecDeque<String>>,
        script: Value,
        started: Mutex<bool>,
        stats_calls: Mutex<u64>,
    }

    impl Shared {
        fn event(&self, name: &str, data: Value) {
            if let Some(qmp) = self.qmp.lock().expect("lock").as_mut() {
                let text = json!({"event": name, "data": data,
                    "timestamp": {"seconds": 0, "microseconds": 0}});
                writeln!(qmp, "{text}").ok();
            }
        }

        fn serial_write(&self, text: &str) {
            if let Some(serial) = self.serial.lock().expect("lock").as_mut() {
                serial.write_all(text.as_bytes()).ok();
            }
        }

        fn shutdown(&self, reason: &str, code: i32) -> ! {
            self.event(
                "SHUTDOWN",
                json!({"guest": reason.starts_with("guest"), "reason": reason}),
            );
            thread::sleep(Duration::from_millis(50));
            std::process::exit(code);
        }

        fn wait_key(&self, key: &str, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                {
                    let mut keys = self.keys.lock().expect("lock");
                    while let Some(k) = keys.pop_front() {
                        if k == key {
                            return true;
                        }
                    }
                }
                thread::sleep(Duration::from_millis(10));
            }
            false
        }
    }

    pub fn run(args: &[String]) -> ! {
        let qmp_path = value_after(args, "-qmp")
            .first()
            .and_then(|s| s.strip_prefix("unix:"))
            .and_then(|s| option(&format!("path={s}"), "path"))
            .expect("-qmp unix:");
        let serial_path = value_after(args, "-chardev")
            .iter()
            .find_map(|spec| option(spec, "path"))
            .expect("-chardev path");
        let script_path = value_after(args, "-fake-script")[0].to_owned();
        let script: Value =
            serde_json::from_str(&fs::read_to_string(&script_path).expect("script")).expect("json");
        // Like QEMU, create the shared guest-RAM file.
        for spec in value_after(args, "-object") {
            if let Some(path) = option(spec, "mem-path") {
                File::create(path).expect("mem-path");
            }
        }
        if let Some(log) = script.get("args_log").and_then(Value::as_str) {
            fs::write(log, args.join("\n")).expect("args log");
        }

        let qmp_listener = UnixListener::bind(&qmp_path).expect("bind qmp");
        let serial_listener = UnixListener::bind(&serial_path).expect("bind serial");
        let shared = Arc::new(Shared {
            qmp: Mutex::new(None),
            serial: Mutex::new(None),
            keys: Mutex::new(VecDeque::new()),
            script,
            started: Mutex::new(!args.iter().any(|a| a == "-S")),
            stats_calls: Mutex::new(0),
        });

        {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let (stream, _) = serial_listener.accept().expect("serial accept");
                *shared.serial.lock().expect("lock") = Some(stream.try_clone().expect("clone"));
                serial_input(&shared, stream);
            });
        }
        {
            let shared = Arc::clone(&shared);
            thread::spawn(move || {
                let (stream, _) = qmp_listener.accept().expect("qmp accept");
                *shared.qmp.lock().expect("lock") = Some(stream.try_clone().expect("clone"));
                qmp_loop(&shared, stream);
            });
        }

        while !*shared.started.lock().expect("lock") {
            thread::sleep(Duration::from_millis(5));
        }
        let lines = shared.script["serial"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for line in lines {
            let delay = line.get("delay_ms").and_then(Value::as_u64).unwrap_or(5);
            thread::sleep(Duration::from_millis(delay));
            let text = line["text"].as_str().unwrap_or("");
            shared.serial_write(text);
            let keys: Vec<&str> = match line.get("wait_key") {
                Some(Value::String(key)) => vec![key.as_str()],
                Some(Value::Array(keys)) => keys.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            };
            for key in keys {
                assert!(
                    shared.wait_key(key, Duration::from_secs(20)),
                    "no {key} key from the harness"
                );
            }
        }
        if let Some(code) = shared
            .script
            .get("exit_after_serial")
            .and_then(Value::as_i64)
        {
            shared.shutdown("guest-shutdown", i32::try_from(code).expect("code"));
        }
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }

    fn serial_input(shared: &Shared, mut stream: UnixStream) {
        let rules = shared.script["respond"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let mut pending = String::new();
        let mut buf = [0u8; 256];
        loop {
            let n = match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            pending.push_str(&String::from_utf8_lossy(&buf[..n]));
            for rule in &rules {
                let expect = rule["expect"].as_str().unwrap_or("");
                if let Some(at) = pending.find(expect) {
                    pending.drain(..at + expect.len());
                    shared.serial_write(rule["reply"].as_str().unwrap_or(""));
                }
            }
        }
    }

    fn qmp_loop(shared: &Shared, stream: UnixStream) {
        let reply = |value: Value| {
            if let Some(qmp) = shared.qmp.lock().expect("lock").as_mut() {
                writeln!(qmp, "{value}").ok();
            }
        };
        reply(
            json!({"QMP": {"version": {"qemu": {"major": 10, "minor": 2, "micro": 1}, "package": "fake"}, "capabilities": []}}),
        );
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                std::process::exit(0);
            }
            let command: Value = serde_json::from_str(&line).expect("qmp json");
            let id = command.get("id").cloned().unwrap_or(Value::Null);
            let ok = |ret: Value| json!({"return": ret, "id": id});
            let name = command["execute"].as_str().unwrap_or("");
            let arguments = &command["arguments"];
            match name {
                "qmp_capabilities" => reply(json!({"return": {}})),
                "cont" => {
                    reply(ok(json!({})));
                    *shared.started.lock().expect("lock") = true;
                    shared.event("RESUME", json!({}));
                }
                "screendump" => {
                    let lines: Vec<String> = shared.script["screen"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect();
                    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
                    let image = ferric_harness::screen::render_text(
                        &refs,
                        80,
                        25,
                        0xc0c0c0,
                        0,
                        Some((0, 24)),
                    );
                    let path = arguments["filename"].as_str().expect("filename");
                    fs::write(path, image.to_png().expect("png")).expect("write png");
                    reply(ok(json!({})));
                }
                "input-send-event" => {
                    if let Some(log) = shared.script.get("input_log").and_then(Value::as_str) {
                        let mut file = OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(log)
                            .expect("log");
                        for event in arguments["events"].as_array().into_iter().flatten() {
                            writeln!(file, "{event}").expect("write");
                            if event["type"] == "key" && event["data"]["down"] == true {
                                let key = event["data"]["key"]["data"]
                                    .as_str()
                                    .unwrap_or("")
                                    .to_owned();
                                if key == "e" && shared.script.get("editor").is_some() {
                                    shared.serial_write(
                                        "--- Redox Bootloader Environment Editor ---\r\n",
                                    );
                                }
                                shared.keys.lock().expect("lock").push_back(key);
                            }
                        }
                    }
                    reply(ok(json!({})));
                }
                "query-cpus-fast" => reply(ok(json!([
                    {"cpu-index": 0, "thread-id": std::process::id(), "qom-path": "/machine/unattached/device[0]", "target": "x86_64"}
                ]))),
                "query-stats" => {
                    let mut calls = shared.stats_calls.lock().expect("lock");
                    *calls += 1;
                    let n = *calls * 100;
                    if arguments["target"] == "vcpu" {
                        reply(ok(
                            json!([{"provider": "kvm", "qom-path": "/machine/unattached/device[0]",
                            "stats": [{"name": "exits", "value": n}, {"name": "halt_exits", "value": n / 2}]}]),
                        ));
                    } else {
                        reply(ok(
                            json!([{"provider": "kvm", "stats": [{"name": "remote_tlb_flush", "value": n}]}]),
                        ));
                    }
                }
                "system_powerdown" => {
                    reply(ok(json!({})));
                    shared.event("POWERDOWN", json!({}));
                    if shared.script["on_powerdown"] == "exit" {
                        shared.serial_write("ACPI shutdown\r\n");
                        shared.shutdown("guest-shutdown", 0);
                    }
                }
                "quit" => {
                    reply(ok(json!({})));
                    shared.shutdown("host-qmp-quit", 0);
                }
                _ => reply(json!({"error": {"class": "CommandNotFound", "desc": name}, "id": id})),
            }
        }
    }
}

/// A temporary directory with an image, firmware, a run directory and a script.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("disk.img"), b"").expect("image");
        fs::write(dir.path().join("OVMF.fd"), b"").expect("firmware");
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn config(&self, name: &str, script: &Value) -> LaunchConfig {
        let script_path = self.path(&format!("{name}.json"));
        fs::write(&script_path, script.to_string()).expect("script");
        let mut config = LaunchConfig::new(self.path("disk.img"), self.path(name));
        config.firmware = Some(self.path("OVMF.fd"));
        config.qemu = std::env::current_exe().expect("test exe");
        config.qemu_wrapper_args = [
            "fake_qemu_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
            "--",
        ]
        .map(str::to_owned)
        .to_vec();
        config.qemu_args = vec![
            "-fake-script".to_owned(),
            script_path.to_string_lossy().into_owned(),
        ];
        config.stats_interval_s = 0.2;
        config.powerdown_timeout_s = 3.0;
        config.key_delay_ms = 1;
        config
    }
}

/// Stops the guest if a test fails part-way, so no fake QEMU outlives it.
struct Running(Session);

impl Running {
    fn launch(config: LaunchConfig) -> Self {
        Self(Session::launch(config).expect("launch"))
    }
}

impl std::ops::Deref for Running {
    type Target = Session;

    fn deref(&self) -> &Session {
        &self.0
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if !self.0.exited() {
            self.0.interrupt();
            self.0.wait_exit(Some(Duration::from_secs(15)));
        }
    }
}

/// Brings a supervisor started with `up` down if a test fails part-way.
struct Supervised(String);

impl Drop for Supervised {
    fn drop(&mut self) {
        if Path::new(&self.0).join("control.sock").exists() {
            harness(&["down", "--run", &self.0, "--timeout", "0.2"]);
        }
    }
}

fn input_events(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("event"))
        .collect()
}

fn keys_down(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "key" && e["data"]["down"] == true)
        .map(|e| e["data"]["key"]["data"].as_str().unwrap_or("").to_owned())
        .collect()
}

fn check<'a>(report: &'a Report, name: &str) -> &'a ferric_harness::verdict::Check {
    report
        .verdict
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no {name} check"))
}

fn desktop_script(fixture: &Fixture) -> Value {
    json!({
        "input_log": fixture.path("input.jsonl"),
        "on_powerdown": "exit",
        "screen": ["Ferric Oxide boot log", "redox login: _"],
        "serial": [
            {"text": "\u{1b}[2J\u{1b}[001;001HBdsDxe: loading Boot0002\r\n"},
            {"text": "Redox OS Bootloader 1.0.0 on x86_64/UEFI\r\n"},
            {"text": "Arrow keys and enter select mode\r\n", "wait_key": "ret"},
            {"text": "kernel::arch::x86_shared::start:INFO -- Ferric Oxide starting...\r\n"},
            {"text": "init: switchroot to /scheme/initfs /scheme/initfs/etc\r\n"},
            {"text": "FERRIC_BENCH boot-probe stage=early iops=123 lat_ns=4.5\r\n"},
            {"text": "\u{1b}[1mredox login:\u{1b}[0m "}
        ],
        "respond": [
            {"expect": "root\r", "reply": "root\r\nPassword: "},
            {"expect": "password\r", "reply": "\r\nroot:~# "},
            {"expect": "echo hi\r", "reply": "echo hi\r\nhi\r\nroot:~# "}
        ]
    })
}

#[test]
fn desktop_session_end_to_end() {
    let fixture = Fixture::new();
    let config = fixture.config("desktop", &desktop_script(&fixture));
    let session = Running::launch(config);
    let wait = Duration::from_secs(20);

    session
        .expect("login: $", &Since::Cursor, wait)
        .expect("login prompt");
    session.serial_send("root", Eol::Cr).expect("send");
    session
        .expect("assword: $", &Since::Cursor, wait)
        .expect("password prompt");
    session.serial_send("password", Eol::Cr).expect("send");
    session
        .expect("^root:~# $", &Since::Cursor, wait)
        .expect("shell");
    let mark = session.mark("shell");
    session.serial_send("echo hi", Eol::Cr).expect("send");
    let m = session
        .expect("^hi$", &Since::Mark("shell".into()), wait)
        .expect("output");
    assert!(m.matched.start >= mark.serial_offset);
    assert!(matches!(
        session.expect("Bootloader", &Since::Now, Duration::from_millis(100)),
        Err(ferric_harness::Error::ExpectTimeout { .. })
    ));

    let shot = session.shot(None).expect("shot");
    assert_eq!((shot.width, shot.height), (640, 400));
    assert!(shot.path.starts_with(session.run().shots_dir()));
    let text = session.text(None).expect("text");
    assert_eq!(text.screen.text(), "Ferric Oxide boot log\nredox login: _");
    session
        .wait_screen("text:redox login", wait)
        .expect("screen predicate");

    session.type_text("Ab").expect("type");
    session.key(&["meta_l-f1".to_owned()]).expect("key");
    session
        .pointer(PointerAction::Abs {
            x: 320,
            y: 200,
            raw: false,
        })
        .expect("pointer");
    session
        .pointer(PointerAction::Button {
            button: Button::Left,
            action: ButtonAction::Click,
        })
        .expect("click");
    let sample = session.stats().expect("sample");
    assert_eq!(sample.vcpus.len(), 1);
    assert!(sample.vcpus[0].kvm.contains_key("exits"));
    let status = session.status();
    assert!(status.running);
    assert!(status.milestones.contains_key("login"));

    let down = session.down(None);
    let verdict = down.verdict.expect("verdict");
    let report = Report::load(session.run().root()).expect("report");
    assert_eq!(report.verdict, verdict);
    assert_eq!(verdict.result, Outcome::Pass, "{verdict:#?}");
    assert_eq!(check(&report, "shutdown").detail, "the guest powered off");
    assert_eq!(check(&report, "mode").status, CheckStatus::Pass);
    for m in [
        "qemu_start",
        "guest_start",
        "bootloader",
        "bootloader_menu",
        "kernel",
        "init",
        "login",
        "shutdown",
    ] {
        assert!(
            report.milestones.contains_key(m),
            "milestone {m}: {:?}",
            report.milestones
        );
    }
    assert!(report.milestones["kernel"] > report.milestones["bootloader_menu"]);
    assert_eq!(report.bench.len(), 1);
    assert_eq!(report.bench[0].record.series(), "boot-probe[stage=early]");
    assert_eq!(report.environment.qemu.version.as_deref(), Some("10.2.1"));
    assert!(report.stats.samples >= 1);
    assert_eq!(report.stats.vcpus.len(), 1);
    assert_eq!(report.marks[0].name, "shell");
    assert!(report.qmp_events.contains_key("SHUTDOWN"));

    let events = input_events(&fixture.path("input.jsonl"));
    let keys = keys_down(&events);
    // The menu was answered with Enter before anything else was typed.
    assert_eq!(keys[0], "ret");
    assert!(keys.windows(4).any(|w| w == ["shift", "a", "b", "meta_l"]));
    let abs: Vec<&Value> = events.iter().filter(|e| e["type"] == "abs").collect();
    assert_eq!(abs[0]["data"]["value"], 16409);
    assert_eq!(abs[1]["data"]["value"], 16424);
    assert_eq!(events.iter().filter(|e| e["type"] == "btn").count(), 2);

    let jsonl = fs::read_to_string(session.run().serial_jsonl()).expect("jsonl");
    assert!(jsonl.contains("\"text\":\"Arrow keys and enter select mode\""));
    assert!(!session.run().qmp_sock().exists());
}

#[test]
fn smoke_run_powers_off_by_itself() {
    let fixture = Fixture::new();
    let script = json!({
        "serial": [
            {"text": "kernel::arch::x86_shared::start:INFO -- Ferric Oxide starting...\r\n"},
            {"text": "FERRIC_SMOKE_BEGIN\r\n"},
            {"text": "FERRIC_TEST_PASS dd-pipe\r\n"},
            {"text": "FERRIC_TEST_SKIP nvme-grant: no grants\r\n"},
            {"text": "FERRIC_SMOKE_OK\r\n", "delay_ms": 50}
        ],
        "exit_after_serial": 0
    });
    let mut config = fixture.config("smoke", &script);
    config.display = Display::None;
    config.smoke = true;
    config.timeout_s = Some(30.0);
    let session = Running::launch(config);
    let report = session
        .wait_report(Some(Duration::from_secs(30)))
        .expect("report");
    assert_eq!(
        report.verdict.result,
        Outcome::Pass,
        "{:#?}",
        report.verdict
    );
    assert_eq!(report.ending.stop, StopReason::GuestExit);
    assert_eq!(check(&report, "smoke").detail, "FERRIC_SMOKE_OK");
    assert_eq!(
        check(&report, "tests").detail,
        "1 passed, 1 skipped, 0 failed"
    );
    assert!(report.milestones.contains_key("smoke_ok"));
    assert!(report.milestones.contains_key("shutdown"));
    let names: Vec<String> = report
        .markers
        .iter()
        .filter_map(|m| match &m.marker {
            Marker::Test(t) => Some(t.name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["dd-pipe", "nvme-grant"]);
    let args = fs::read_to_string(session.run().qemu_args()).expect("args");
    assert!(args.contains("\n-vga\nnone\n"));
    assert!(args.contains("nvme,drive=drv0,serial=NVME_SERIAL,addr=4.0"));
}

#[test]
fn crash_and_timeout_stop_the_run() {
    let fixture = Fixture::new();
    let crash = json!({
        "serial": [
            {"text": "kernel::arch::x86_shared::start:INFO -- Ferric Oxide starting...\r\n"},
            {"text": "KERNEL PANIC: panicked at src/memory/mod.rs:1540:34:\r\n"}
        ]
    });
    let mut config = fixture.config("crash", &crash);
    config.display = Display::None;
    config.on_crash = OnCrash::Stop;
    let started = Instant::now();
    let report = Running::launch(config)
        .wait_report(Some(Duration::from_secs(30)))
        .expect("report");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "the backtrace gets its grace"
    );
    assert_eq!(report.ending.stop, StopReason::Crash);
    assert_eq!(report.verdict.result, Outcome::Fail);
    assert_eq!(check(&report, "crash").status, CheckStatus::Fail);
    assert_eq!(
        report.ending.shutdown_reason.as_deref(),
        Some("host-qmp-quit")
    );

    let idle = json!({"serial": [{"text": "kernel::a:INFO -- idle\r\n"}]});
    let mut config = fixture.config("timeout", &idle);
    config.display = Display::None;
    config.timeout_s = Some(1.0);
    let report = Running::launch(config)
        .wait_report(Some(Duration::from_secs(30)))
        .expect("report");
    assert_eq!(report.ending.stop, StopReason::Timeout);
    assert_eq!(check(&report, "stop").detail, "timed out after 1s");
}

#[test]
fn powerdown_ignored_then_quit() {
    let fixture = Fixture::new();
    let script = json!({
        "on_powerdown": "ignore",
        "serial": [{"text": "kernel::a:INFO -- trace: sched.switch on from boot\r\n"}]
    });
    let mut config = fixture.config("ignore", &script);
    config.display = Display::None;
    config.powerdown_timeout_s = 0.5;
    let session = Running::launch(config);
    // Let the console line arrive before stopping.
    session
        .expect("idle|trace:", &Since::Start, Duration::from_secs(20))
        .expect("line");
    let down = session.down(None);
    let report = Report::load(session.run().root()).expect("report");
    assert_eq!(down.verdict.as_ref().map(|v| v.result), Some(Outcome::Fail));
    // A normal-mode run fails on a kernel trace: line.
    assert_eq!(check(&report, "mode").status, CheckStatus::Fail);
    assert_eq!(check(&report, "shutdown").status, CheckStatus::Warn);
    assert_eq!(report.ending.powerdown_wait_s, Some(0.5));
}

#[test]
fn trace_mode_starts_follow_and_cleans_up() {
    let fixture = Fixture::new();
    let follow = fixture.path("ferric-trace");
    fs::write(
        &follow,
        "#!/bin/sh\necho \"$@\"\ntrap 'echo flushed; exit 0' INT TERM\nwhile :; do sleep 0.05; done\n",
    )
    .expect("script");
    fs::set_permissions(&follow, fs::Permissions::from_mode(0o755)).expect("chmod");
    let script = json!({
        "args_log": fixture.path("args.txt"),
        "serial": [
            {"text": "kernel::trace::imp:INFO -- trace: trace-mode kernel\r\n"},
            {"text": "kernel::trace::imp:INFO -- trace: host rings at phys 0x7a000000, 4 CPUs x 4096 KiB\r\n"}
        ]
    });
    let name = format!("trace-{}", std::process::id());
    let mut config = fixture.config(&name, &script);
    config.display = Display::None;
    config.mode = Mode::Trace;
    config.ferric_trace = Some(follow);
    config.trace_ring_kib = 1024;
    config.boot_env = vec![("FERRIC_TRACE".into(), "sched.switch".into())];
    let shm = config.shm_path().expect("shm");
    let session = Running::launch(config);
    let log = session.run().trace_follow_log();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("follow")
    {
        assert!(Instant::now() < deadline, "follow never started");
        thread::sleep(Duration::from_millis(20));
    }
    assert!(shm.exists());
    assert!(matches!(
        session.trace_start(),
        Err(ferric_harness::Error::TraceRunning)
    ));
    let stopped = session.trace_stop().expect("stop");
    assert_eq!(stopped.exit.as_ref().map(|e| e.code), Some(Some(0)));
    let restarted = session.trace_start().expect("restart");
    assert!(restarted.dir.ends_with("trace.2"));
    session.down(Some(Duration::from_millis(300)));
    let report = Report::load(session.run().root()).expect("report");
    assert_eq!(check(&report, "mode").status, CheckStatus::Pass);
    let trace = report.trace.expect("trace info");
    assert_eq!(trace.host_rings.map(|r| r.phys), Some(0x7a00_0000));
    assert_eq!(trace.captures.len(), 2);
    assert!(
        trace
            .captures
            .iter()
            .all(|c| c.exit.as_ref().is_some_and(|e| e.code == Some(0)))
    );
    assert!(!shm.exists(), "the RAM file is removed");
    let follow_log = fs::read_to_string(&log).expect("log");
    assert!(follow_log.contains(&format!(
        "follow --guest-ram {} --phys 0x7a000000 --ram-mib 2048 --out {}",
        shm.display(),
        session.run().trace_dir(1).display()
    )));
    assert!(follow_log.contains("flushed"));
    let env = fs::read_to_string(session.run().boot_env()).expect("boot env");
    assert_eq!(
        env,
        "FERRIC_KERNEL=trace\nFERRIC_TRACE_HOST=1024\nFERRIC_TRACE=sched.switch\n"
    );
    let args = fs::read_to_string(fixture.path("args.txt")).expect("args");
    assert!(args.contains("name=opt/org.ferric-oxide/boot-env,file="));
    assert!(args.contains(&format!("mem-path={},size=2048M,share=on", shm.display())));
}

#[test]
fn boot_env_typed_into_the_editor() {
    let fixture = Fixture::new();
    let script = json!({
        "input_log": fixture.path("keys.jsonl"),
        "editor": true,
        "serial": [
            // One Enter ends the typed line, the second boots.
            {"text": "Arrow keys and enter select mode\r\n", "wait_key": ["ret", "ret"]},
            {"text": "kernel::a:INFO -- booted\r\n"}
        ],
        "on_powerdown": "exit"
    });
    let mut config = fixture.config("keys", &script);
    config.boot_env_via = BootEnvVia::Keys;
    config.boot_env = vec![("FOO_1".into(), "a=B".into())];
    let session = Running::launch(config);
    session
        .expect("booted", &Since::Start, Duration::from_secs(20))
        .expect("boot continues after typing");
    session.down(None);
    let keys = keys_down(&input_events(&fixture.path("keys.jsonl")));
    assert_eq!(
        keys,
        [
            "e", "shift", "f", "shift", "o", "shift", "o", "shift", "minus", "1", "equal", "a",
            "equal", "shift", "b", "ret", "ret"
        ]
    );
    assert!(
        !session.run().boot_env().exists(),
        "no fw_cfg file when typing"
    );
}

fn harness(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ferric-harness"))
        .args(args)
        .output()
        .expect("run ferric-harness")
}

#[test]
fn cli_up_commands_down_report() {
    let fixture = Fixture::new();
    let script_path = fixture.path("cli.json");
    fs::write(&script_path, desktop_script(&fixture).to_string()).expect("script");
    let run = fixture.path("cli-run");
    let run_s = run.to_string_lossy().into_owned();
    let exe = std::env::current_exe().expect("exe");
    let mut up: Vec<String> = vec![
        "up".into(),
        fixture.path("disk.img").to_string_lossy().into_owned(),
        "--run".into(),
        run_s.clone(),
        "--firmware".into(),
        fixture.path("OVMF.fd").to_string_lossy().into_owned(),
        "--qemu".into(),
        exe.to_string_lossy().into_owned(),
        "--stats-interval".into(),
        "0.5".into(),
    ];
    for arg in [
        "fake_qemu_process",
        "--exact",
        "--nocapture",
        "--test-threads=1",
        "--",
    ] {
        up.push("--qemu-wrapper-arg".into());
        up.push(arg.into());
    }
    up.extend([
        "--qemu-arg".into(),
        "-fake-script".into(),
        "--qemu-arg".into(),
        script_path.to_string_lossy().into_owned(),
    ]);
    let refs: Vec<&str> = up.iter().map(String::as_str).collect();
    let out = harness(&refs);
    let _guard = Supervised(run_s.clone());
    assert!(
        out.status.success(),
        "up: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("run: "));

    let run_args = |cmd: &[&str]| {
        let mut v = cmd.to_vec();
        v.extend(["--run", run_s.as_str()]);
        harness(&v)
    };
    let out = run_args(&["expect", "login: $", "--timeout", "20"]);
    assert!(
        out.status.success(),
        "expect: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("redox login:"));
    let out = run_args(&["serial", "send", "root"]);
    assert!(out.status.success());
    let out = run_args(&["expect", "assword", "--timeout", "20"]);
    assert!(out.status.success());
    let out = run_args(&["expect", "never-printed", "--timeout", "0.2"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no match for"));
    let out = run_args(&["text"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("redox login: _"));
    let out = run_args(&["key", "nonsense-key"]);
    assert_eq!(out.status.code(), Some(2));
    let out = run_args(&["status", "--json"]);
    let status: Value = serde_json::from_slice(&out.stdout).expect("status json");
    assert_eq!(status["running"], true);
    let out = run_args(&["down"]);
    assert!(
        out.status.success(),
        "down: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("verdict: Pass"));
    // The supervisor removes its socket once the report is written.
    let deadline = Instant::now() + Duration::from_secs(10);
    while run.join("control.sock").exists() {
        assert!(Instant::now() < deadline, "control socket left behind");
        thread::sleep(Duration::from_millis(20));
    }
    let out = run_args(&["status"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("not running"));
    let out = harness(&["report", &run_s]);
    assert!(out.status.success());
    let table = String::from_utf8_lossy(&out.stdout);
    assert!(table.contains("cli-run"));
    assert!(table.contains("bench.boot-probe[stage=early].iops"));
    let out = harness(&["report", "--a", &run_s, "--b", &run_s, "--json"]);
    let value: Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(value["paired"]["pairs"], 1);
}

#[test]
fn cli_run_scenario() {
    let fixture = Fixture::new();
    let script_path = fixture.path("scen.json");
    fs::write(&script_path, desktop_script(&fixture).to_string()).expect("script");
    let scenario = fixture.path("login.toml");
    fs::write(
        &scenario,
        r#"
[[step]]
expect = "login: $"
timeout = 20
[[step]]
send = "root"
[[step]]
expect = "assword"
[[step]]
send = "password"
[[step]]
expect = "^root:~# $"
[[step]]
mark = "shell"
[[step]]
text = "screen.txt"
contains = "redox login"
[[step]]
shot = "end.png"
[[step]]
key = "ret"
"#,
    )
    .expect("scenario");
    let run = fixture.path("scen-run");
    let exe = std::env::current_exe().expect("exe");
    let mut args: Vec<String> = vec![
        "run".into(),
        fixture.path("disk.img").to_string_lossy().into_owned(),
        "--scenario".into(),
        scenario.to_string_lossy().into_owned(),
        "--run".into(),
        run.to_string_lossy().into_owned(),
        "--firmware".into(),
        fixture.path("OVMF.fd").to_string_lossy().into_owned(),
        "--qemu".into(),
        exe.to_string_lossy().into_owned(),
    ];
    for arg in [
        "fake_qemu_process",
        "--exact",
        "--nocapture",
        "--test-threads=1",
        "--",
    ] {
        args.push("--qemu-wrapper-arg".into());
        args.push(arg.into());
    }
    args.extend([
        "--qemu-arg".into(),
        "-fake-script".into(),
        "--qemu-arg".into(),
        script_path.to_string_lossy().into_owned(),
    ]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = harness(&refs);
    assert!(
        out.status.success(),
        "run: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let report = Report::load(&run).expect("report");
    let scenario = report.scenario.clone().expect("scenario");
    assert_eq!(scenario.summary.ran, 9);
    assert!(scenario.steps.iter().all(|s| s.ok));
    assert_eq!(check(&report, "scenario").status, CheckStatus::Pass);
    assert!(
        fs::read_to_string(run.join("screen.txt"))
            .expect("text")
            .contains("redox login")
    );
    assert!(run.join("shots/end.png").exists());

    // A failing step fails the run.
    let bad = fixture.path("bad.toml");
    fs::write(&bad, "[[step]]\nexpect = \"never\"\ntimeout = 0.3\n").expect("scenario");
    let run2 = fixture.path("bad-run");
    let mut args2 = args.clone();
    args2[3] = bad.to_string_lossy().into_owned();
    args2[5] = run2.to_string_lossy().into_owned();
    let refs: Vec<&str> = args2.iter().map(String::as_str).collect();
    let out = harness(&refs);
    assert_eq!(out.status.code(), Some(1));
    let report = Report::load(&run2).expect("report");
    assert_eq!(check(&report, "scenario").status, CheckStatus::Fail);
}
