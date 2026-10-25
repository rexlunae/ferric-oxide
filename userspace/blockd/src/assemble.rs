//! Building the volume from the steps of a [`Spec`](crate::spec::Spec).

use std::io;
use std::path::{Path, PathBuf};

#[cfg(feature = "lvm")]
use ferric_blocklayer::LvmOpenError;
use ferric_blocklayer::{BlockDevice, OutOfRange, Stack, StackError};
#[cfg(feature = "luks")]
use ferric_blocklayer::{CryptOpenError, Limits};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::spec::Step;

/// Where devices and passphrases come from: files on the system, or test images.
pub trait Sources {
    /// The devices `dev:` steps open.
    type Device: BlockDevice;

    /// Opens the device or image file at `path`.
    ///
    /// # Errors
    ///
    /// The I/O error from opening it.
    fn device(&mut self, path: &Path) -> io::Result<Self::Device>;

    /// Reads the passphrase in the key file at `path`.
    ///
    /// # Errors
    ///
    /// The I/O error from reading it.
    fn passphrase(&mut self, path: &Path) -> io::Result<Zeroizing<Vec<u8>>>;
}

/// The volumes the steps leave, with the device error type of `S`.
pub type Volume<S> = Stack<<S as Sources>::Device>;

/// Runs `steps` and returns the one volume they leave.
///
/// # Errors
///
/// An [`AssembleError`] naming the step that failed and why.
pub fn assemble<S: Sources>(
    steps: &[Step],
    sources: &mut S,
) -> Result<Volume<S>, AssembleError<<S::Device as BlockDevice>::Error>> {
    let mut stack: Vec<Volume<S>> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let at = index + 1;
        match step {
            Step::Device(path) => {
                let device = sources.device(path).map_err(|source| AssembleError::Open {
                    path: path.clone(),
                    source,
                })?;
                stack.push(Stack::Device(device));
            }
            Step::Slice { offset, len } => {
                let top = stack.pop().ok_or(AssembleError::Empty { step: at })?;
                let len = len.unwrap_or_else(|| top.size().saturating_sub(*offset));
                let slice = top
                    .slice(*offset, len)
                    .map_err(|source| AssembleError::Slice { step: at, source })?;
                stack.push(slice);
            }
            #[cfg(feature = "luks")]
            Step::Luks { key_file } => {
                let top = stack.pop().ok_or(AssembleError::Empty { step: at })?;
                let passphrase =
                    sources
                        .passphrase(key_file)
                        .map_err(|source| AssembleError::Key {
                            path: key_file.clone(),
                            source,
                        })?;
                let volume = top
                    .crypt(&passphrase, &Limits::default())
                    .map_err(|source| AssembleError::Luks { step: at, source })?;
                stack.push(volume);
            }
            #[cfg(feature = "lvm")]
            Step::Lvm { name } => {
                if stack.is_empty() {
                    return Err(AssembleError::Empty { step: at });
                }
                let pvs = core::mem::take(&mut stack);
                let volume = Stack::lvm(pvs, name)
                    .map_err(|source| AssembleError::Lvm { step: at, source })?;
                stack.push(volume);
            }
            // Spec::parse refuses steps for layers this build leaves out.
            #[allow(unreachable_patterns)]
            _ => return Err(AssembleError::Unsupported { step: at }),
        }
    }
    match (stack.pop(), stack.len()) {
        (Some(volume), 0) => Ok(volume),
        (None, _) => Err(AssembleError::Left { count: 0 }),
        (Some(_), more) => Err(AssembleError::Left { count: more + 1 }),
    }
}

/// Why the volume could not be built.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum AssembleError<E: std::error::Error + 'static> {
    /// A device could not be opened.
    #[error("cannot open the device {}", path.display())]
    Open {
        /// Its path.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// A key file could not be read.
    #[error("cannot read the key file {}", path.display())]
    Key {
        /// Its path.
        path: PathBuf,
        /// Why.
        #[source]
        source: io::Error,
    },
    /// A step needs a volume and there is none.
    #[error("step {step} has no volume to work on")]
    Empty {
        /// The step, from 1.
        step: usize,
    },
    /// A slice does not fit its volume.
    #[error("the slice of step {step} does not fit")]
    Slice {
        /// The step, from 1.
        step: usize,
        /// Why.
        #[source]
        source: OutOfRange,
    },
    /// A LUKS volume could not be opened.
    #[cfg(feature = "luks")]
    #[error("cannot open the LUKS volume of step {step}")]
    Luks {
        /// The step, from 1.
        step: usize,
        /// Why.
        #[source]
        source: CryptOpenError<StackError<E>>,
    },
    /// A logical volume could not be opened.
    #[cfg(feature = "lvm")]
    #[error("cannot open the logical volume of step {step}")]
    Lvm {
        /// The step, from 1.
        step: usize,
        /// Why.
        #[source]
        source: LvmOpenError<StackError<E>>,
    },
    /// The step's layer is not built in.
    #[error("step {step} needs a layer this build leaves out")]
    Unsupported {
        /// The step, from 1.
        step: usize,
    },
    /// The steps leave other than one volume.
    #[error("the steps leave {count} volumes; exactly one can be served")]
    Left {
        /// How many.
        count: usize,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ferric_blocklayer::Memory;

    use super::*;

    /// Images and key files in memory.
    #[derive(Default)]
    struct Files(BTreeMap<PathBuf, Vec<u8>>);

    impl Sources for Files {
        type Device = Memory<Vec<u8>>;

        fn device(&mut self, path: &Path) -> io::Result<Self::Device> {
            let bytes = self.0.get(path).ok_or(io::ErrorKind::NotFound)?;
            Ok(Memory::new(bytes.clone()))
        }

        fn passphrase(&mut self, path: &Path) -> io::Result<Zeroizing<Vec<u8>>> {
            let bytes = self.0.get(path).ok_or(io::ErrorKind::NotFound)?;
            Ok(Zeroizing::new(bytes.clone()))
        }
    }

    fn files() -> Files {
        let mut files = Files::default();
        files.0.insert("/disk".into(), (0..=255).collect());
        files
    }

    #[test]
    fn slices_stack() {
        let steps = [
            Step::Device("/disk".into()),
            Step::Slice {
                offset: 16,
                len: None,
            },
            Step::Slice {
                offset: 4,
                len: Some(8),
            },
        ];
        let volume = assemble(&steps, &mut files()).unwrap();
        assert_eq!(volume.size(), 8);
        let mut buf = [0; 2];
        volume.read_at(6, &mut buf).unwrap();
        assert_eq!(buf, [26, 27]);
    }

    #[test]
    fn bad_steps_are_named() {
        let err = |steps: &[Step]| assemble(steps, &mut files()).map(|_| ()).unwrap_err();
        assert!(matches!(
            err(&[Step::Device("/nope".into())]),
            AssembleError::Open { .. }
        ));
        assert!(matches!(
            err(&[Step::Slice {
                offset: 0,
                len: None
            }]),
            AssembleError::Empty { step: 1 }
        ));
        assert!(matches!(
            err(&[
                Step::Device("/disk".into()),
                Step::Slice {
                    offset: 200,
                    len: Some(100)
                }
            ]),
            AssembleError::Slice { step: 2, .. }
        ));
        assert!(matches!(
            err(&[Step::Device("/disk".into()), Step::Device("/disk".into())]),
            AssembleError::Left { count: 2 }
        ));
        assert!(matches!(err(&[]), AssembleError::Left { count: 0 }));
        #[cfg(feature = "luks")]
        assert!(matches!(
            err(&[
                Step::Device("/disk".into()),
                Step::Luks {
                    key_file: "/no-key".into()
                }
            ]),
            AssembleError::Key { .. }
        ));
    }
}
