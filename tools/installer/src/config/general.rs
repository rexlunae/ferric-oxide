//! Global filesystem, encryption and bootloader installation settings.

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// Image-wide settings shared by package and disk installation.
pub struct GeneralConfig {
    /// Specify a path where cookbook exists, all packages will be installed locally
    pub cookbook: Option<String>,
    /// Allow prompts for missing information such as user password
    pub prompt: Option<bool>,
    /// Total image size in MiB. The installer grows the image when the
    /// packages and files it installs need more, unless
    /// `filesystem_size_exact` is set.
    pub filesystem_size: Option<u32>,
    /// Make `filesystem_size` the exact image size: the installer fails
    /// before writing anything when the packages and files do not fit,
    /// instead of growing the image.
    pub filesystem_size_exact: Option<bool>,
    /// EFI partition size in MB, default to 2MB
    pub efi_partition_size: Option<u32>,
    /// Skip disk partitioning, assume whole disk is a partition
    pub skip_partitions: Option<bool>,
    /// Set a plain text password to encrypt the disk
    pub encrypt_disk: Option<String>,
    /// Use live disk for bootloader config, default is false
    pub live_disk: Option<bool>,
    /// If set, write bootloader disk into this path
    pub write_bootloader: Option<String>,
    /// Use AR to write files instead of FUSE-based mount
    /// Guest ownership is recorded and applied inside the image, without host chown.
    pub no_mount: Option<bool>,
    /// Also install the image's build tools, the recipes its packages list as
    /// `build.dev-dependencies` (a `host:` tool needs them at compile time but
    /// the finished image does not).
    ///
    /// The guest build environment is made this way so the tools can run
    /// inside it; setting `keep` also leaves them installed in the final image
    /// as the `build-tools` meta-package (`build.keep_build_tools`).
    pub build_tools: Option<BuildToolsConfig>,
}

/// Which build tools an image installation adds.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct BuildToolsConfig {
    /// Leave the tools installed in the finished image, rather than treating
    /// them as a scratch build environment. They are collected in one
    /// `build-tools` meta-package so a package list shows one entry.
    pub keep: Option<bool>,
    /// File listing the tool recipe names, one per line, as
    /// `repo build-tools-list` prints them. Relative paths resolve against
    /// the current directory, like `general.cookbook`.
    pub list: Option<String>,
}

impl GeneralConfig {
    /// Merge two config, "other" is more dominant
    pub(super) fn merge(&mut self, other: GeneralConfig) {
        if let Some(cookbook) = other.cookbook {
            self.cookbook = Some(cookbook);
        }
        self.filesystem_size = other.filesystem_size.or(self.filesystem_size);
        self.filesystem_size_exact = other.filesystem_size_exact.or(self.filesystem_size_exact);
        self.efi_partition_size = other.efi_partition_size.or(self.efi_partition_size);
        self.skip_partitions = other.skip_partitions.or(self.skip_partitions);
        if let Some(encrypt_disk) = other.encrypt_disk {
            self.encrypt_disk = Some(encrypt_disk);
        }
        self.live_disk = other.live_disk.or(self.live_disk);
        if let Some(write_bootloader) = other.write_bootloader {
            self.write_bootloader = Some(write_bootloader);
        }
        self.no_mount = other.no_mount.or(self.no_mount);
        if let Some(build_tools) = other.build_tools {
            let merged = match self.build_tools.take() {
                Some(existing) => BuildToolsConfig {
                    keep: build_tools.keep.or(existing.keep),
                    list: build_tools.list.or(existing.list),
                },
                None => build_tools,
            };
            self.build_tools = Some(merged);
        }
    }
}
