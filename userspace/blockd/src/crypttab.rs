//! `/etc/ferric/crypttab`: the volumes `ferric-unlock` builds at boot (Cargo feature
//! `unlock`, features.toml `fs.unlock_at_boot`).
//!
//! Like `/etc/crypttab`, it has one volume per line, with fields separated by spaces or tabs.
//! `#` starts a comment that runs to the end of the line, and blank lines are skipped:
//!
//! ```text
//! # NAME     STEP...                                                 [FILESYSTEM:SCHEME]
//! secret     dev:/scheme/disk.nvme/0p2 luks:/etc/keys/secret lvm:home   ext4:home
//! scratch    dev:/scheme/disk.nvme/0p3 luks:/etc/keys/scratch
//! ```
//!
//! - `NAME` is the scheme that `ferric-blockd` serves the unlocked volume as
//!   (`/scheme/NAME`).
//! - The `STEP`s build the volume, exactly as on `ferric-blockd`'s command line (see
//!   [`crate::spec`]): `dev:PATH`, `slice:OFFSET:LENGTH`, `luks:KEYFILE` and `lvm:NAME`. A
//!   LUKS passphrase is always read from a key file. Like `ferric-blockd` itself, the table
//!   never holds a passphrase.
//! - An optional last field, `ext4:SCHEME`, has `ext4d` mount the volume at
//!   `/scheme/SCHEME`. `redoxfs:SCHEME` is recognised but refused for now: RedoxFS opens its
//!   disk for writing, and `ferric-blockd` serves volumes read-only.
//!
//! Volumes are unlocked in the order they are listed. Every scheme name in the table, block
//! or file system, must be unique. The format is Ferric Oxide's own, written for this
//! daemon; it borrows only the shape of a crypttab line.

use std::fmt;

use thiserror::Error;

use crate::spec::{Spec, SpecError};

/// Where `ferric-unlock` reads the table from by default.
pub const PATH: &str = "/etc/ferric/crypttab";

/// One volume to unlock: the arguments for `ferric-blockd`, and what to mount on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    /// The scheme and steps, as parsed (and so checked) by [`Spec::parse`].
    pub spec: Spec,
    /// The steps as written, passed through to `ferric-blockd` unchanged.
    steps: Vec<String>,
    /// The file system to mount on the volume, if any.
    pub mount: Option<Mount>,
}

impl Volume {
    /// `ferric-blockd`'s arguments: the scheme name, then the steps.
    pub fn blockd_args(&self) -> Vec<String> {
        let mut args = Vec::with_capacity(1 + self.steps.len());
        args.push(self.spec.scheme.clone());
        args.extend(self.steps.iter().cloned());
        args
    }

    /// The path the unlocked volume is served at, `/scheme/NAME`.
    pub fn device(&self) -> String {
        format!("/scheme/{}", self.spec.scheme)
    }
}

/// A file system to mount on an unlocked volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// Which daemon mounts it.
    pub filesystem: Filesystem,
    /// The scheme it is mounted as, `/scheme/SCHEME`.
    pub scheme: String,
}

impl Mount {
    /// The file system daemon's arguments for mounting `device`.
    pub fn args(&self, device: &str) -> Vec<String> {
        match self.filesystem {
            Filesystem::Ext4 => vec![device.to_owned(), self.scheme.clone()],
        }
    }
}

/// A file system `ferric-unlock` can mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Filesystem {
    /// ext4, mounted read-only by `ext4d` (features.toml `fs.ext4`).
    Ext4,
}

impl Filesystem {
    /// The daemon that mounts it.
    pub const fn program(self) -> &'static str {
        match self {
            Self::Ext4 => "/usr/bin/ext4d",
        }
    }
}

impl fmt::Display for Filesystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ext4 => "ext4",
        })
    }
}

/// A table that does not parse.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum CrypttabError {
    /// A line is wrong.
    #[error("line {line}")]
    Line {
        /// The line number, from 1.
        line: usize,
        /// What is wrong with it.
        #[source]
        source: LineError,
    },
    /// A scheme name is used twice.
    #[error("line {line}: the scheme {scheme:?} is already used on line {first}")]
    Duplicate {
        /// The line that uses it again.
        line: usize,
        /// The line that used it first.
        first: usize,
        /// The scheme name.
        scheme: String,
    },
}

/// What is wrong with one line of the table.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineError {
    /// The name or a step is not what `ferric-blockd` takes.
    #[error("bad volume")]
    Spec(#[from] SpecError),
    /// A file system field is followed by more fields.
    #[error("{field:?} must be the last field")]
    MountNotLast {
        /// The file system field.
        field: String,
    },
    /// The file system's scheme name is empty or holds a `/` or `:`.
    #[error("{0:?} is not a scheme name")]
    MountScheme(String),
    /// RedoxFS cannot be mounted on a volume that `ferric-blockd` serves read-only.
    #[error("{field:?}: RedoxFS needs a writable volume, and ferric-blockd serves read-only")]
    ReadOnlyRedoxFs {
        /// The field.
        field: String,
    },
}

/// Parses the table's text.
///
/// # Errors
///
/// [`CrypttabError::Line`] for the first line that is wrong, and
/// [`CrypttabError::Duplicate`] for a scheme name used twice.
///
/// # Examples
///
/// ```
/// use ferric_blockd::crypttab::{self, Filesystem};
///
/// // `luks:` and `lvm:` steps go between `dev:` and `ext4:`; this one is a plain partition.
/// let table = "# ext4 on the second partition\n\
///              home dev:/scheme/disk.nvme/0p2 ext4:home-fs\n";
/// let volumes = crypttab::parse(table)?;
/// assert_eq!(volumes[0].blockd_args()[..2], ["home", "dev:/scheme/disk.nvme/0p2"]);
/// let mount = volumes[0].mount.as_ref().expect("a file system");
/// assert_eq!((mount.filesystem, mount.scheme.as_str()), (Filesystem::Ext4, "home-fs"));
/// # Ok::<(), crypttab::CrypttabError>(())
/// ```
pub fn parse(text: &str) -> Result<Vec<Volume>, CrypttabError> {
    let mut volumes = Vec::new();
    // Every scheme name so far, with the line that took it.
    let mut taken: Vec<(String, usize)> = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        let content = raw.split('#').next().unwrap_or_default();
        let fields: Vec<&str> = content.split_whitespace().collect();
        if fields.is_empty() {
            continue;
        }
        let volume = parse_line(&fields).map_err(|source| CrypttabError::Line { line, source })?;
        let names = std::iter::once(&volume.spec.scheme)
            .chain(volume.mount.as_ref().map(|mount| &mount.scheme));
        for scheme in names {
            if let Some((_, first)) = taken.iter().find(|(name, _)| name == scheme) {
                return Err(CrypttabError::Duplicate {
                    line,
                    first: *first,
                    scheme: scheme.clone(),
                });
            }
            taken.push((scheme.clone(), line));
        }
        volumes.push(volume);
    }
    Ok(volumes)
}

/// Parses the fields of one line that is not blank.
fn parse_line(fields: &[&str]) -> Result<Volume, LineError> {
    let mut steps = Vec::new();
    let mut mount = None;
    for field in &fields[1..] {
        if let Some(found) = &mount {
            let Mount { filesystem, scheme } = found;
            return Err(LineError::MountNotLast {
                field: format!("{filesystem}:{scheme}"),
            });
        }
        match field.split_once(':') {
            Some(("ext4", scheme)) => mount = Some(mount_field(Filesystem::Ext4, scheme)?),
            Some(("redoxfs", _)) => {
                return Err(LineError::ReadOnlyRedoxFs {
                    field: (*field).to_owned(),
                });
            }
            _ => steps.push((*field).to_owned()),
        }
    }
    let spec = Spec::parse(std::iter::once(fields[0].to_owned()).chain(steps.iter().cloned()))?;
    Ok(Volume { spec, steps, mount })
}

fn mount_field(filesystem: Filesystem, scheme: &str) -> Result<Mount, LineError> {
    if scheme.is_empty() || scheme.contains(['/', ':']) {
        return Err(LineError::MountScheme(scheme.to_owned()));
    }
    Ok(Mount {
        filesystem,
        scheme: scheme.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Step;

    fn line_error(text: &str) -> LineError {
        match parse(text) {
            Err(CrypttabError::Line { source, .. }) => source,
            other => panic!("{other:?}"),
        }
    }

    // The steps are checked against the layers this build has.
    #[cfg(all(feature = "luks", feature = "lvm"))]
    #[test]
    fn a_table_lists_volumes_in_order() {
        let table = "\
# The boot disk's encrypted partitions.

home\tdev:/scheme/disk.nvme/0p2   luks:/etc/keys/home lvm:home ext4:home-fs   # LVM on LUKS
scratch dev:/scheme/disk.nvme/0p3 slice:0:-  luks:/etc/keys/scratch
";
        let volumes = parse(table).expect("parses");
        assert_eq!(volumes.len(), 2);
        let home = &volumes[0];
        assert_eq!(
            home.blockd_args(),
            [
                "home",
                "dev:/scheme/disk.nvme/0p2",
                "luks:/etc/keys/home",
                "lvm:home"
            ]
        );
        assert_eq!(home.device(), "/scheme/home");
        assert_eq!(
            home.mount,
            Some(Mount {
                filesystem: Filesystem::Ext4,
                scheme: "home-fs".into()
            })
        );
        assert_eq!(
            home.mount.as_ref().map(|m| m.args(&home.device())),
            Some(vec!["/scheme/home".to_owned(), "home-fs".to_owned()])
        );
        let scratch = &volumes[1];
        assert_eq!(scratch.mount, None);
        assert_eq!(
            scratch.spec.steps[..2],
            [
                Step::Device("/scheme/disk.nvme/0p3".into()),
                Step::Slice {
                    offset: 0,
                    len: None
                }
            ]
        );
    }

    #[test]
    fn an_empty_table_unlocks_nothing() {
        assert_eq!(parse(""), Ok(Vec::new()));
        assert_eq!(parse("# nothing yet\n\n   \t\n"), Ok(Vec::new()));
    }

    #[test]
    fn a_plain_device_can_be_mounted_without_layers() {
        let volumes = parse("data dev:/scheme/disk.nvme/0p4 ext4:data-fs").expect("parses");
        assert_eq!(
            volumes[0].spec.steps,
            [Step::Device("/scheme/disk.nvme/0p4".into())]
        );
        assert_eq!(Filesystem::Ext4.program(), "/usr/bin/ext4d");
    }

    #[test]
    fn bad_lines_are_named() {
        assert_eq!(
            parse("ok dev:/a\nbad raid:/b"),
            Err(CrypttabError::Line {
                line: 2,
                source: LineError::Spec(SpecError::Step("raid:/b".into()))
            })
        );
        assert_eq!(
            line_error("lonely"),
            LineError::Spec(SpecError::Usage),
            "a name and no steps"
        );
        assert_eq!(
            line_error("only-fs ext4:x"),
            LineError::Spec(SpecError::Usage),
            "a file system but no steps"
        );
        assert_eq!(
            line_error("a/b dev:/x"),
            LineError::Spec(SpecError::Scheme("a/b".into()))
        );
        assert_eq!(
            line_error("v dev:/x ext4:fs luks:/k"),
            LineError::MountNotLast {
                field: "ext4:fs".into()
            }
        );
        assert_eq!(
            line_error("v dev:/x ext4:a/b"),
            LineError::MountScheme("a/b".into())
        );
        assert_eq!(
            line_error("v dev:/x ext4:"),
            LineError::MountScheme("".into())
        );
        assert_eq!(
            line_error("v dev:/x redoxfs:root"),
            LineError::ReadOnlyRedoxFs {
                field: "redoxfs:root".into()
            }
        );
    }

    #[test]
    fn scheme_names_are_unique() {
        assert_eq!(
            parse("a dev:/x\nb dev:/y ext4:a"),
            Err(CrypttabError::Duplicate {
                line: 2,
                first: 1,
                scheme: "a".into()
            })
        );
        assert_eq!(
            parse("a dev:/x ext4:a"),
            Err(CrypttabError::Duplicate {
                line: 1,
                first: 1,
                scheme: "a".into()
            })
        );
    }

    #[test]
    fn errors_read_as_sentences() {
        let err = parse("\n\nv dev:/x raid:y").expect_err("bad");
        assert_eq!(err.to_string(), "line 3");
        let source = std::error::Error::source(&err).expect("the line's error");
        assert_eq!(source.to_string(), "bad volume");
    }
}
