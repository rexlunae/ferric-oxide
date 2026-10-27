//! How big an image must be for the packages and files its configuration
//! installs.
//!
//! A configuration's `filesystem_size` is the size it asks for. The
//! installer adds up what the configured packages and their dependencies
//! take once installed, from the archives in the cookbook's repository, and
//! grows the image when that does not fit. With `filesystem_size_exact` the
//! asked-for size is kept, and a size that is too small fails before
//! anything is written.

use std::path::Path;

use redoxfs::BLOCK_SIZE;

use crate::config::Config;
use crate::installer::{PackageInstallError, package_names, plan_cookbook_packages};

const MIB: u64 = 1024 * 1024;

/// MiB of the disk outside RedoxFS besides the EFI partition: the first
/// MiB (GPT and the BIOS bootloader) and the GPT mirror at the end.
const PARTITION_MIB: u64 = 2;

/// MiB always added on top of the contents, for RedoxFS's own metadata, the
/// account files and home directories, fox's database and the image SBOM.
const FIXED_HEADROOM_MIB: u64 = 64;

/// The contents are multiplied by this many quarters, so a quarter of the
/// image stays free for the packages a fox package set adds after the
/// configured ones, logs, and whatever the running system writes.
const HEADROOM_QUARTERS: u64 = 5;

/// The size an image is built at, and what it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageSize {
    /// Bytes the configured packages and their dependencies take on RedoxFS.
    pub packages: u64,
    /// Bytes the configuration's own files take on RedoxFS.
    pub files: u64,
    /// The smallest image, in MiB, that holds both with headroom.
    pub required_mib: u32,
    /// The size to build, in MiB: the configured size, grown to
    /// `required_mib` unless the configuration makes its size exact.
    pub mib: u32,
}

/// Why an image cannot be sized.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ImageSizeError {
    /// The configuration fixes a size its contents do not fit in.
    #[error(
        "the image needs at least {required} MiB ({} MiB of packages), but its size is fixed \
         at {size} MiB; raise image.filesystem_size in ferric.toml (or FILESYSTEM_SIZE), or \
         set it to 0 so the image is sized to fit",
        .packages.div_ceil(MIB)
    )]
    TooSmall {
        /// The fixed size, in MiB.
        size: u32,
        /// The smallest size that fits, in MiB.
        required: u32,
        /// Bytes of packages.
        packages: u64,
    },
    /// `filesystem_size_exact` is set without a `filesystem_size`.
    #[error("filesystem_size_exact is set, but filesystem_size is not")]
    ExactWithoutSize,
    /// The contents need more MiB than an image size can say.
    #[error("the image would need {0} MiB")]
    Overflow(u64),
}

/// Bytes a file of `len` bytes takes on RedoxFS: its node block and whole
/// data blocks.
fn stored_size(len: u64) -> u64 {
    BLOCK_SIZE + len.next_multiple_of(BLOCK_SIZE)
}

/// Bytes the configured packages and their dependencies take once
/// installed, from the archives in the cookbook's repository.
///
/// # Errors
/// [`PackageInstallError::NoRepository`] if packages are configured and no
/// cookbook is given, and the errors of reading the cookbook's key and
/// planning the packages.
pub fn packages_size(config: &Config, cookbook: Option<&str>) -> anyhow::Result<u64> {
    let names = package_names(config)?;
    if names.is_empty() {
        return Ok(0);
    }
    let cookbook = Path::new(cookbook.ok_or(PackageInstallError::NoRepository)?);
    let key = fox_core::keys::load_key_file(&cookbook.join(fox_core::cookbook::PUBLIC_KEY))?;
    let mut keys = fox_core::TrustedKeys::new();
    keys.insert(key.label, key.key);
    // Planned against an empty root, every package is in the plan.
    let empty = tempfile::tempdir()?;
    let store = fox_store::Store::open(empty.path())?;
    let plan = plan_cookbook_packages(names, cookbook, keys, &store)?;
    Ok(plan
        .install
        .iter()
        .flat_map(|(archive, _)| archive.files())
        .map(|file| stored_size(file.size))
        .sum())
}

/// Bytes the configuration's file declarations take on RedoxFS.
fn files_size(config: &Config) -> u64 {
    config
        .files
        .iter()
        .map(|file| stored_size(file.data.len() as u64))
        .sum()
}

/// The smallest image, in MiB, that holds `contents` bytes with headroom,
/// with an EFI partition of `efi_mib`.
fn required_mib(contents: u64, efi_mib: u64) -> u64 {
    (contents * HEADROOM_QUARTERS / 4).div_ceil(MIB) + FIXED_HEADROOM_MIB + PARTITION_MIB + efi_mib
}

/// The size to build the image for `config` at.
///
/// # Errors
/// [`ImageSizeError::TooSmall`] if the configuration fixes a size that is
/// too small, [`ImageSizeError::ExactWithoutSize`] if it fixes none, and the
/// errors of [`packages_size`].
pub fn image_size(config: &Config, cookbook: Option<&str>) -> anyhow::Result<ImageSize> {
    let packages = packages_size(config, cookbook)?;
    let files = files_size(config);
    let efi_mib = u64::from(config.general.efi_partition_size.unwrap_or(1));
    let required = required_mib(packages + files, efi_mib);
    let required_mib = u32::try_from(required).map_err(|_| ImageSizeError::Overflow(required))?;
    let configured = config.general.filesystem_size;
    let mib = if config.general.filesystem_size_exact.unwrap_or(false) {
        let size = configured.ok_or(ImageSizeError::ExactWithoutSize)?;
        if size < required_mib {
            return Err(ImageSizeError::TooSmall {
                size,
                required: required_mib,
                packages,
            }
            .into());
        }
        size
    } else {
        configured.unwrap_or(0).max(required_mib)
    };
    Ok(ImageSize {
        packages,
        files,
        required_mib,
        mib,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::tests::{config, cookbook};

    #[test]
    fn files_take_whole_blocks() {
        assert_eq!(stored_size(0), BLOCK_SIZE);
        assert_eq!(stored_size(1), 2 * BLOCK_SIZE);
        assert_eq!(stored_size(BLOCK_SIZE), 2 * BLOCK_SIZE);
    }

    #[test]
    fn headroom_is_added() {
        assert_eq!(required_mib(0, 1), 67);
        assert_eq!(required_mib(400 * MIB, 1), 500 + 67);
    }

    #[test]
    fn packages_and_dependencies_are_counted() {
        let (cookbook, _) = cookbook();
        let cookbook = cookbook.path().to_str().unwrap();
        // app needs lib; each holds one small file besides its metadata.
        let lib = packages_size(&config(&["lib"]), Some(cookbook)).unwrap();
        assert!(lib >= stored_size(3), "{lib}");
        assert_eq!(
            packages_size(&config(&["app"]), Some(cookbook)).unwrap(),
            2 * lib
        );
        assert_eq!(packages_size(&config(&[]), None).unwrap(), 0);
        let error = packages_size(&config(&["app"]), None).unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(PackageInstallError::NoRepository)
        ));
    }

    #[test]
    fn the_configured_size_is_grown_unless_exact() {
        let mut config = config(&[]);
        config.general.filesystem_size = Some(32);
        let size = image_size(&config, None).unwrap();
        assert_eq!(size.required_mib, 67);
        assert_eq!(size.mib, 67);

        config.general.filesystem_size = Some(1024);
        assert_eq!(image_size(&config, None).unwrap().mib, 1024);

        config.general.filesystem_size_exact = Some(true);
        assert_eq!(image_size(&config, None).unwrap().mib, 1024);
        config.general.filesystem_size = Some(32);
        let error = image_size(&config, None).unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(ImageSizeError::TooSmall {
                size: 32,
                required: 67,
                ..
            })
        ));
        config.general.filesystem_size = None;
        let error = image_size(&config, None).unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(ImageSizeError::ExactWithoutSize)
        ));
    }
}
