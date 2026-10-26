//! The command line: the scheme name and the steps that build the volume.
//!
//! ```text
//! ferric-blockd SCHEME STEP...
//! ```
//!
//! The steps work on a stack of volumes, left to right:
//!
//! | Step | Effect |
//! | --- | --- |
//! | `dev:PATH` | pushes the disk, partition or image file at `PATH` |
//! | `slice:OFFSET:LENGTH` | replaces the top volume with its bytes `OFFSET..OFFSET+LENGTH` (`LENGTH` `-` means to the end) |
//! | `luks:KEYFILE` | replaces the top volume with the LUKS volume on it, unlocked with the passphrase in `KEYFILE` |
//! | `lvm:NAME` | replaces every volume on the stack (all the group's physical volumes) with its logical volume `NAME` |
//!
//! Exactly one volume must be left at the end, and it is served at `/scheme/SCHEME`. For
//! example, LVM on LUKS on a partition:
//!
//! ```text
//! ferric-blockd root dev:/scheme/disk.nvme/0p2 luks:/etc/keys/root lvm:root
//! ```
//!
//! The passphrase is the whole content of `KEYFILE`, byte for byte (a trailing newline is part
//! of it), as cryptsetup reads `--key-file`. It never comes from the command line, which other
//! processes can read.

use std::num::ParseIntError;
use std::path::PathBuf;

use thiserror::Error;

/// The parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The scheme to serve the volume as.
    pub scheme: String,
    /// The steps, in order.
    pub steps: Vec<Step>,
}

/// One step of building the volume.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Step {
    /// Push a device or image file.
    Device(PathBuf),
    /// Replace the top volume with a byte window of it.
    Slice {
        /// The first byte.
        offset: u64,
        /// The length, or `None` for the rest of the volume.
        len: Option<u64>,
    },
    /// Replace the top volume with the LUKS volume on it.
    Luks {
        /// The file holding the passphrase.
        key_file: PathBuf,
    },
    /// Replace all volumes with one logical volume of the group they are.
    Lvm {
        /// The logical volume's name.
        name: String,
    },
}

/// The usage line.
pub const USAGE: &str = "usage: ferric-blockd SCHEME STEP... \
     (STEP: dev:PATH | slice:OFFSET:LENGTH | luks:KEYFILE | lvm:NAME)";

/// A command line that does not parse.
#[derive(Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum SpecError {
    /// No scheme name, or no steps.
    #[error("{USAGE}")]
    Usage,
    /// The scheme name is empty or holds a `/` or `:`.
    #[error("{0:?} is not a scheme name")]
    Scheme(String),
    /// A step is not one of the known kinds, or is missing its argument.
    #[error("{0:?} is not a step ({USAGE})")]
    Step(String),
    /// A number in a step does not parse.
    #[error("{value:?} in {step:?} is not a number of bytes")]
    Number {
        /// The step.
        step: String,
        /// The text that is not a number.
        value: String,
        /// Why.
        #[source]
        source: ParseIntError,
    },
    /// The step's layer is not built into this daemon.
    #[error(
        "{step:?} needs the {layer} layer, which this build leaves out (features.toml {option})"
    )]
    Disabled {
        /// The step.
        step: String,
        /// The layer.
        layer: &'static str,
        /// The features.toml option that builds it in.
        option: &'static str,
    },
}

impl Spec {
    /// Parses the arguments after the program name.
    ///
    /// # Errors
    ///
    /// A [`SpecError`] for the first argument that is wrong.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, SpecError> {
        let mut args = args.into_iter();
        let scheme = args.next().ok_or(SpecError::Usage)?;
        if scheme.is_empty() || scheme.contains(['/', ':']) {
            return Err(SpecError::Scheme(scheme));
        }
        let steps = args.map(|arg| step(&arg)).collect::<Result<Vec<_>, _>>()?;
        if steps.is_empty() {
            return Err(SpecError::Usage);
        }
        Ok(Self { scheme, steps })
    }
}

fn step(arg: &str) -> Result<Step, SpecError> {
    let bad = || SpecError::Step(arg.to_owned());
    let (kind, rest) = arg.split_once(':').ok_or_else(bad)?;
    if rest.is_empty() {
        return Err(bad());
    }
    match kind {
        "dev" => Ok(Step::Device(rest.into())),
        "slice" => {
            let (offset, len) = rest.split_once(':').ok_or_else(bad)?;
            Ok(Step::Slice {
                offset: number(arg, offset)?,
                len: match len {
                    "-" => None,
                    len => Some(number(arg, len)?),
                },
            })
        }
        "luks" => enabled(arg, cfg!(feature = "luks"), "LUKS", "fs.luks").map(|()| Step::Luks {
            key_file: rest.into(),
        }),
        "lvm" => enabled(arg, cfg!(feature = "lvm"), "LVM", "fs.lvm").map(|()| Step::Lvm {
            name: rest.to_owned(),
        }),
        _ => Err(bad()),
    }
}

fn number(step: &str, value: &str) -> Result<u64, SpecError> {
    value.parse().map_err(|source| SpecError::Number {
        step: step.to_owned(),
        value: value.to_owned(),
        source,
    })
}

fn enabled(
    step: &str,
    built: bool,
    layer: &'static str,
    option: &'static str,
) -> Result<(), SpecError> {
    if built {
        Ok(())
    } else {
        Err(SpecError::Disabled {
            step: step.to_owned(),
            layer,
            option,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Spec, SpecError> {
        Spec::parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn steps_parse_in_order() {
        let spec = parse(&[
            "root",
            "dev:/scheme/disk.nvme/0",
            "slice:1048576:-",
            "slice:0:4096",
            "luks:/etc/keys/root",
            "lvm:root",
        ]);
        let expected = Spec {
            scheme: "root".into(),
            steps: vec![
                Step::Device("/scheme/disk.nvme/0".into()),
                Step::Slice {
                    offset: 1 << 20,
                    len: None,
                },
                Step::Slice {
                    offset: 0,
                    len: Some(4096),
                },
                Step::Luks {
                    key_file: "/etc/keys/root".into(),
                },
                Step::Lvm {
                    name: "root".into(),
                },
            ],
        };
        #[cfg(all(feature = "luks", feature = "lvm"))]
        assert_eq!(spec, Ok(expected));
        #[cfg(not(all(feature = "luks", feature = "lvm")))]
        assert!(
            matches!(spec, Err(SpecError::Disabled { .. })),
            "{expected:?}"
        );
    }

    #[test]
    fn paths_keep_their_colons() {
        assert_eq!(
            parse(&["v", "dev:/tmp/a:b"]).map(|s| s.steps),
            Ok(vec![Step::Device("/tmp/a:b".into())])
        );
    }

    #[test]
    fn mistakes_are_named() {
        assert_eq!(parse(&[]), Err(SpecError::Usage));
        assert_eq!(parse(&["v"]), Err(SpecError::Usage));
        assert_eq!(
            parse(&["a/b", "dev:x"]),
            Err(SpecError::Scheme("a/b".into()))
        );
        assert_eq!(parse(&["", "dev:x"]), Err(SpecError::Scheme(String::new())));
        assert_eq!(parse(&["v", "dev:"]), Err(SpecError::Step("dev:".into())));
        assert_eq!(parse(&["v", "disk"]), Err(SpecError::Step("disk".into())));
        assert_eq!(
            parse(&["v", "raid:x"]),
            Err(SpecError::Step("raid:x".into()))
        );
        assert_eq!(
            parse(&["v", "slice:5"]),
            Err(SpecError::Step("slice:5".into()))
        );
        let err = parse(&["v", "dev:x", "slice:5:ten"]).unwrap_err();
        assert!(matches!(&err, SpecError::Number { value, .. } if value == "ten"));
        assert_eq!(
            err.to_string(),
            "\"ten\" in \"slice:5:ten\" is not a number of bytes"
        );
    }
}
