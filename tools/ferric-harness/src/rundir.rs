//! The run directory: where one run keeps everything it writes.
//!
//! A run directory holds the console logs, QMP events, screenshots, stats samples, trace
//! capture, sockets and the final `report.json`. It defaults to `build/harness/<name>/` in
//! the repository, never `/tmp` (a small tmpfs on some hosts, and a 10 GB image's snapshot
//! overlay or a trace capture does not fit there). `build/harness/latest` points at the most
//! recent run started there, so commands can omit `--run`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Name of the link to the newest run under `build/harness/`.
pub const LATEST: &str = "latest";

/// The files of one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunDir {
    root: PathBuf,
}

impl RunDir {
    /// A run directory at `root` (not created).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Creates the directory and its subdirectories.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when a directory cannot be created.
    pub fn create(&self) -> Result<(), Error> {
        for dir in [
            self.root.clone(),
            self.shots_dir(),
            self.tmp_dir(),
            self.tftp_dir(),
        ] {
            fs::create_dir_all(&dir).map_err(|source| Error::Io { path: dir, source })?;
        }
        Ok(())
    }

    /// The directory itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The run's name: the directory's last component.
    pub fn name(&self) -> String {
        self.root
            .file_name()
            .map_or_else(|| "run".to_owned(), |n| n.to_string_lossy().into_owned())
    }

    /// Raw console bytes as QEMU sent them.
    pub fn serial_log(&self) -> PathBuf {
        self.root.join("serial.log")
    }
    /// Console lines, one JSON object per line, stamped with host times.
    pub fn serial_jsonl(&self) -> PathBuf {
        self.root.join("serial.jsonl")
    }
    /// Text sent to the console, stamped (what the harness typed on serial).
    pub fn serial_input_jsonl(&self) -> PathBuf {
        self.root.join("serial-input.jsonl")
    }
    /// QMP events, one JSON object per line, stamped with host times.
    pub fn qmp_events(&self) -> PathBuf {
        self.root.join("qmp-events.jsonl")
    }
    /// The supervisor's control socket.
    pub fn control_sock(&self) -> PathBuf {
        self.root.join("control.sock")
    }
    /// QEMU's QMP socket.
    pub fn qmp_sock(&self) -> PathBuf {
        self.root.join("qmp.sock")
    }
    /// QEMU's serial socket.
    pub fn serial_sock(&self) -> PathBuf {
        self.root.join("serial.sock")
    }
    /// The boot environment passed through fw_cfg.
    pub fn boot_env(&self) -> PathBuf {
        self.root.join("boot-env")
    }
    /// The launch configuration the supervisor reads.
    pub fn launch_json(&self) -> PathBuf {
        self.root.join("launch.json")
    }
    /// The QEMU command line, one argument per line.
    pub fn qemu_args(&self) -> PathBuf {
        self.root.join("qemu-args.txt")
    }
    /// The final report.
    pub fn report_json(&self) -> PathBuf {
        self.root.join("report.json")
    }
    /// Screenshots.
    pub fn shots_dir(&self) -> PathBuf {
        self.root.join("shots")
    }
    /// Periodic stats samples, one JSON object per line.
    pub fn stats_jsonl(&self) -> PathBuf {
        self.root.join("stats.jsonl")
    }
    /// The first host trace capture; later ones get a number (`trace.2`, ...).
    pub fn trace_dir(&self, index: usize) -> PathBuf {
        match index {
            0 | 1 => self.root.join("trace"),
            n => self.root.join(format!("trace.{n}")),
        }
    }
    /// Output of `ferric-trace follow`.
    pub fn trace_follow_log(&self) -> PathBuf {
        self.root.join("trace-follow.log")
    }
    /// The supervisor's own log (stdout and stderr).
    pub fn supervisor_log(&self) -> PathBuf {
        self.root.join("supervisor.log")
    }
    /// QEMU's stdout and stderr.
    pub fn qemu_log(&self) -> PathBuf {
        self.root.join("qemu.log")
    }
    /// The private TFTP root the user network serves (the netq test's fixture).
    pub fn tftp_dir(&self) -> PathBuf {
        self.root.join("tftp")
    }
    /// QEMU's `TMPDIR`, where `snapshot=on` keeps its overlay (deleted as soon as it is
    /// opened, but it still takes space while QEMU runs).
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }
}

/// The repository root: the nearest ancestor of `start` holding both `Cargo.toml` and
/// `features.toml`.
pub fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join("features.toml").is_file())
        .map(Path::to_path_buf)
}

/// `build/harness` under the repository root (or under `start` outside a checkout).
pub fn harness_root(start: &Path) -> PathBuf {
    find_repo_root(start)
        .unwrap_or_else(|| start.to_path_buf())
        .join("build")
        .join("harness")
}

/// A run name from the image's file stem and the time: `wayland-20261101-184200`.
pub fn default_name(image: &Path, wall: time::OffsetDateTime) -> String {
    let stem = image
        .parent()
        .and_then(Path::file_name)
        .filter(|_| image.file_stem().is_some_and(|s| s == "harddrive"))
        .or_else(|| image.file_stem())
        .map_or_else(|| "run".to_owned(), |s| s.to_string_lossy().into_owned());
    format!(
        "{}-{:04}{:02}{:02}-{:02}{:02}{:02}",
        sanitize(&stem),
        wall.year(),
        u8::from(wall.month()),
        wall.day(),
        wall.hour(),
        wall.minute(),
        wall.second()
    )
}

/// Keeps letters, digits, `-`, `_` and `.`; anything else becomes `-`.
pub fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Points `<parent>/latest` at the run directory (relative), replacing an older link.
///
/// # Errors
///
/// [`Error::Io`] when the link cannot be replaced.
pub fn update_latest(run: &RunDir) -> Result<(), Error> {
    let Some(parent) = run.root().parent() else {
        return Ok(());
    };
    let link = parent.join(LATEST);
    match fs::symlink_metadata(&link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            fs::remove_file(&link).map_err(|source| Error::Io {
                path: link.clone(),
                source,
            })?;
        }
        // Never replace a real file or directory someone named `latest`.
        Ok(_) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::Io { path: link, source }),
    }
    std::os::unix::fs::symlink(run.name(), &link).map_err(|source| Error::Io { path: link, source })
}

/// Unix socket paths are limited to 107 bytes; a longer path fails with a clear error
/// instead of QEMU's.
///
/// # Errors
///
/// [`Error::SocketPathTooLong`] when `path` does not fit.
pub fn check_socket_path(path: &Path) -> Result<(), Error> {
    const SUN_PATH_MAX: usize = 107;
    if path.as_os_str().len() > SUN_PATH_MAX {
        return Err(Error::SocketPathTooLong {
            path: path.to_path_buf(),
            max: SUN_PATH_MAX,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let t = time::Date::from_calendar_date(2026, time::Month::November, 1)
            .and_then(|d| d.with_hms(18, 42, 7))
            .expect("valid date")
            .assume_utc();
        assert_eq!(
            default_name(Path::new("/b/x86_64/wayland/harddrive.img"), t),
            "wayland-20261101-184207"
        );
        assert_eq!(
            default_name(Path::new("/b/cosmic before.img"), t),
            "cosmic-before-20261101-184207"
        );
        assert_eq!(sanitize("a/b c"), "a-b-c");
    }

    #[test]
    fn layout_and_latest() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let run = RunDir::new(tmp.path().join("one"));
        run.create().expect("create");
        assert!(run.shots_dir().is_dir());
        assert_eq!(run.trace_dir(0), run.root().join("trace"));
        assert_eq!(run.trace_dir(2), run.root().join("trace.2"));
        update_latest(&run).expect("link");
        let two = RunDir::new(tmp.path().join("two"));
        two.create().expect("create");
        update_latest(&two).expect("relink");
        assert_eq!(
            fs::read_link(tmp.path().join(LATEST)).expect("link"),
            Path::new("two")
        );
    }

    #[test]
    fn repo_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        fs::write(tmp.path().join("Cargo.toml"), "").expect("write");
        fs::write(tmp.path().join("features.toml"), "").expect("write");
        let deep = tmp.path().join("a/b");
        fs::create_dir_all(&deep).expect("mkdir");
        assert_eq!(find_repo_root(&deep).as_deref(), Some(tmp.path()));
        assert_eq!(harness_root(&deep), tmp.path().join("build/harness"));
    }

    #[test]
    fn socket_paths() {
        assert!(check_socket_path(Path::new("/short")).is_ok());
        let long = PathBuf::from(format!("/{}", "x".repeat(200)));
        assert!(matches!(
            check_socket_path(&long),
            Err(Error::SocketPathTooLong { .. })
        ));
    }
}
