use anyhow::{bail, Context};
use std::collections::{HashMap, HashSet};
#[cfg(feature = "fuse")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::{fs, io};

#[cfg(feature = "fuse")]
use crate::redoxfs::shrink_disk;
use crate::redoxfs::{archive_image, expand_disk, extract_tar};
#[cfg(feature = "fuse")]
use crate::redoxfs::{run_install_mount, RedoxFs};
use crate::writer::{redoxerd_config_files, write_redoxerd_config};
use crate::{host_target, parse_bool_env, redoxer_dir, status_error, target};

// extra disk space to fit large projects
const DISK_SIZE: u64 = 3 * 1024 * 1024 * 1024;
// need to fit under the default RAM
const DISK_SIZE_LIVE: u64 = 1024 * 1024 * 1024;

pub fn qemu_executable() -> &'static str {
    match target() {
        "x86_64-unknown-redox" => "qemu-system-x86_64",
        "aarch64-unknown-redox" => "qemu-system-aarch64",
        "i586-unknown-redox" | "i686-unknown-redox" => "qemu-system-i386",
        "riscv64gc-unknown-redox" => "qemu-system-riscv64",
        _ => panic!("Unknown target architecture for QEMU"),
    }
}

pub fn qemu_has_kvm() -> bool {
    fn get_arch(triple: &str) -> &str {
        triple.split('-').next().unwrap_or(triple)
    }
    let host = get_arch(host_target());
    let target = get_arch(target());
    kvm_accessible().is_ok()
        && match (host, target) {
            ("x86_64", "x86_64" | "i586" | "i686") => true,
            // https://gitlab.redox-os.org/redox-os/redox/-/issues/1714
            ("aarch64", "aarch64") => false,
            (_, _) => false,
        }
}

/// Whether the guest boots through UEFI rather than legacy BIOS.
///
/// Ferric Oxide builds a UEFI-only x86_64 image: the bootloader recipe skips the
/// BIOS binaries when `FERRIC_UEFI_ONLY` is set, which the native toolchain
/// does, and the build errors instead of falling back to BIOS when firmware is
/// missing. Upstream Redox still ships a BIOS bootloader for x86_64, so a
/// checkout that has one keeps using it.
pub fn qemu_use_uefi() -> bool {
    // Once the bootloader package has been installed its contents decide.
    if let Some(firmware) = bootloader_firmware::get() {
        return firmware == bootloader_firmware::Firmware::Uefi;
    }
    match target() {
        // The BIOS bootloader is present only in an upstream-style build.
        "x86_64-unknown-redox" | "i586-unknown-redox" | "i686-unknown-redox" => {
            !bootloader_bios_available()
        }
        "aarch64-unknown-redox" => true,
        "riscv64gc-unknown-redox" => true,
        _ => panic!("Unknown target architecture for QEMU"),
    }
}

/// Which firmware a guest boots with, once the bootloader package's contents
/// have been seen.
///
/// `qemu_use_uefi` is asked while building the image, before the bootloader is
/// installed, so it cannot always answer from the package. The answer is cached
/// here once `bootloader()` has chosen a boot image, and the architecture
/// decides until then.
mod bootloader_firmware {
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Which boot image the bootloader package provides.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Firmware {
        /// The package has a UEFI image.
        Uefi,
        /// The package has only a BIOS image.
        Bios,
    }

    /// Nothing discovered yet: the architecture's default decides.
    const UNKNOWN: u8 = 0;
    static FIRMWARE: AtomicU8 = AtomicU8::new(UNKNOWN);

    /// Records the firmware the bootloader package provides.
    pub(super) fn set(firmware: Firmware) {
        let value = match firmware {
            Firmware::Uefi => 1,
            Firmware::Bios => 2,
        };
        FIRMWARE.store(value, Ordering::Relaxed);
    }

    /// The firmware discovered in this run, if the bootloader was installed.
    pub(super) fn get() -> Option<Firmware> {
        match FIRMWARE.load(Ordering::Relaxed) {
            1 => Some(Firmware::Uefi),
            2 => Some(Firmware::Bios),
            _ => None,
        }
    }
}

/// Whether the cookbook can supply the BIOS bootloader.
///
/// Ferric Oxide builds a UEFI-only x86_64 image: the bootloader recipe skips its
/// BIOS binaries when `FERRIC_UEFI_ONLY` is set, which the native toolchain
/// does, and the build errors instead of falling back to BIOS when firmware is
/// missing. Upstream Redox still builds them, so a checkout that has them boots
/// the way it always did. The recipe's staged output is what the repository is
/// published from, so it answers the question without unpacking a package.
fn bootloader_bios_available() -> bool {
    let Ok(cookbook) = crate::cookbook_dir() else {
        return false;
    };
    cookbook
        .join("recipes/core/bootloader/target")
        .join(target())
        .join("stage/usr/lib/boot/bootloader.bios")
        .is_file()
}

pub fn qemu_use_live_disk() -> bool {
    match target() {
        "x86_64-unknown-redox" => false,
        "i586-unknown-redox" | "i686-unknown-redox" => false,
        "aarch64-unknown-redox" => true,
        "riscv64gc-unknown-redox" => true,
        _ => panic!("Unknown target architecture for QEMU"),
    }
}

pub fn qemu_disk_device() -> &'static str {
    match target() {
        "i586-unknown-redox" | "i686-unknown-redox" => "ide",
        _ => "virtio",
    }
}
/// The size the base image is built at.
///
/// The bootloader reads the whole image into memory before the kernel runs, so
/// it has to fit beside the kernel in the guest's RAM. `qemu_disk_size` is what
/// a run expands its private copy to once the base is loaded.
pub fn qemu_base_disk_size() -> u64 {
    DISK_SIZE_LIVE
}

/// The largest disk a guest can boot, given the memory it is given.
///
/// The bootloader reads the image into RAM, so the disk has to leave room for
/// the kernel, the firmware and the machine's own overhead.
pub fn qemu_working_disk_size() -> u64 {
    DISK_SIZE_LIVE
}

pub fn qemu_disk_size() -> u64 {
    if qemu_use_live_disk() {
        DISK_SIZE_LIVE
    } else {
        DISK_SIZE
    }
}

#[allow(unused)]
fn kvm_accessible() -> Result<(), io::Error> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")?;
    // https://docs.kernel.org/virt/kvm/api.html#kvm-get-api-version
    #[cfg(target_os = "linux")]
    if unsafe { libc::ioctl(file.as_raw_fd(), 0xae00) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[allow(unused)]
pub fn fuse_accessible() -> Result<(), io::Error> {
    // TODO: A faster way than actually try to mount?
    #[cfg(feature = "fuse")]
    {
        let t = tempfile::tempdir()?;
        fuser::Session::new(crate::redoxfs::DummyFS, t.path(), &[])?.unmount();
        Ok(())
    }
    // Without the probe this cannot tell whether FUSE works, so it reports the
    // conservative answer: images are installed through the RedoxFS writer
    // instead of a mount, which needs neither FUSE nor root.
    #[cfg(not(feature = "fuse"))]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "redoxer was built without the fuse-probe feature",
    ))
}

#[rustfmt::skip]
/// The firmware images a UEFI guest can boot from, best first.
///
/// Each entry is a candidate file and the argument that loads it. `-bios` maps
/// the firmware at the reset vector and is what QEMU's `virt` machines want;
/// `-drive if=pflash` exposes it as flash, which is how the x86 and RISC-V
/// machines take it. Nothing here is architecture-neutral, so the table keeps
/// the per-architecture argument choice alongside the paths.
///
/// The packages column names what to install: a missing firmware is a setup
/// problem the user can fix, so it becomes an error rather than a panic or a
/// silent boot of the wrong firmware.
type FirmwareCandidates = &'static [(&'static str, &'static str)];

const FIRMWARE_SEARCHES: &[(&str, FirmwareCandidates, &str)] = &[
    (
        "x86_64-unknown-redox",
        &[
            (
                "/usr/share/OVMF/OVMF_CODE_4M.fd",
                "if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE_4M.fd,readonly=on",
            ),
            (
                "/usr/share/OVMF/OVMF_CODE.fd",
                "if=pflash,format=raw,unit=0,file=/usr/share/OVMF/OVMF_CODE.fd,readonly=on",
            ),
            (
                "/usr/share/qemu/OVMF.fd",
                "if=pflash,format=raw,unit=0,file=/usr/share/qemu/OVMF.fd,readonly=on",
            ),
        ],
        "ovmf (Debian/Ubuntu), edk2-ovmf (Fedora), or qemu (macOS with QEMU installed)",
    ),
    (
        "aarch64-unknown-redox",
        &[
            (
                "/usr/share/AAVMF/AAVMF_CODE.fd",
                // The virt machine takes the firmware as a `-bios` image.
                "AAVMF",
            ),
            (
                "/usr/share/qemu/edk2-aarch64-code.fd",
                "if=pflash,format=raw,unit=0,file=/usr/share/qemu/edk2-aarch64-code.fd,readonly=on",
            ),
        ],
        "qemu-efi-aarch64 (Debian/Ubuntu) or edk2-aarch64 (Fedora)",
    ),
    (
        "riscv64gc-unknown-redox",
        &[
            (
                "/usr/share/qemu-efi-riscv64/RISCV_VIRT_CODE.fd",
                "if=pflash,format=raw,unit=0,file=/usr/share/qemu-efi-riscv64/RISCV_VIRT_CODE.fd,readonly=on",
            ),
            (
                "/usr/share/qemu/edk2-riscv-code.fd",
                "if=pflash,format=raw,unit=0,file=/usr/share/qemu/edk2-riscv-code.fd,readonly=on",
            ),
        ],
        "qemu-efi-riscv64 (Debian/Ubuntu) or edk2-riscv (Fedora)",
    ),
];

/// The firmware of `arch` to boot, as the argument and its value.
///
/// The first candidate in [`FIRMWARE_SEARCHES`] that exists wins, so the order
/// is the preference order.
///
/// # Errors
/// Returns an error when none of the candidates exists, naming the packages
/// that provide one. Boot firmware cannot be defaulted: QEMU's built-in BIOS
/// cannot start a UEFI-only image, so guessing would fail later with an
/// unrelated message, and a panic would give no way to fix it.
fn uefi_firmware(arch: &str) -> anyhow::Result<(&'static str, &'static str)> {
    let Some((_, candidates, packages)) =
        FIRMWARE_SEARCHES.iter().find(|(name, _, _)| *name == arch)
    else {
        anyhow::bail!("no UEFI firmware is known for {arch}");
    };
    uefi_firmware_among(arch, candidates)
        .map_err(|error| anyhow::anyhow!("{error}; install {packages}"))
}

/// Picks the first candidate of `candidates` that exists.
///
/// Split from [`uefi_firmware`] so the "nothing found" path can be tested
/// without depending on which firmware the host has: a caller can pass
/// candidates that cannot exist.
///
/// # Errors
/// Returns an error naming `arch` and every path that was tried.
fn uefi_firmware_among(
    arch: &str,
    candidates: FirmwareCandidates,
) -> anyhow::Result<(&'static str, &'static str)> {
    for (path, argument) in candidates.iter() {
        if Path::new(path).exists() {
            // AArch64's `virt` machine wants the raw image through `-bios`,
            // which is what the marker records; the rest use pflash.
            let flag = if *argument == "AAVMF" {
                "-bios"
            } else {
                "-drive"
            };
            let value = if *argument == "AAVMF" {
                *path
            } else {
                *argument
            };
            return Ok((flag, value));
        }
    }
    anyhow::bail!(
        "no UEFI firmware found for {arch}; QEMU looked in {}",
        candidates
            .iter()
            .map(|(path, _)| *path)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The machine, console and firmware arguments for `arch`.
///
/// Split out so the firmware lookup is the single fallible step: every arm
/// builds the same shape of list, and a missing firmware is reported once,
/// with the architecture that needed it.
///
/// # Errors
/// Returns an error when a UEFI architecture has no firmware installed.
fn machine_args(arch: &str) -> anyhow::Result<Vec<String>> {
    // The debug console every machine exports; the serial consoles differ.
    let debugcon = [
        "-serial",
        "mon:stdio",
        "-device",
        "isa-debugcon,chardev=log",
    ];
    Ok(match arch {
        "i586-unknown-redox" | "i686-unknown-redox" => {
            vec![
                "-machine".to_string(),
                "q35".to_string(),
                debugcon[0].to_string(),
                debugcon[1].to_string(),
                debugcon[2].to_string(),
                debugcon[3].to_string(),
                "-device".to_string(),
                "isa-debug-exit".to_string(),
            ]
        }
        // Ferric Oxide is UEFI-only on x86_64, so its guests need an OVMF image
        // rather than QEMU's default BIOS. An upstream-style build still has the
        // BIOS bootloader, which is what the default firmware boots.
        "x86_64-unknown-redox" if qemu_use_uefi() => {
            let (flag, value) = uefi_firmware(arch)?;
            vec![
                "-machine".to_string(),
                "q35".to_string(),
                debugcon[0].to_string(),
                debugcon[1].to_string(),
                debugcon[2].to_string(),
                debugcon[3].to_string(),
                "-device".to_string(),
                "isa-debug-exit".to_string(),
                flag.to_string(),
                value.to_string(),
            ]
        }
        "x86_64-unknown-redox" => {
            vec![
                "-machine".to_string(),
                "q35".to_string(),
                debugcon[0].to_string(),
                debugcon[1].to_string(),
                debugcon[2].to_string(),
                debugcon[3].to_string(),
                "-device".to_string(),
                "isa-debug-exit".to_string(),
            ]
        }
        "aarch64-unknown-redox" => {
            let (flag, value) = uefi_firmware(arch)?;
            vec![
                "-machine".to_string(),
                "virt".to_string(),
                "-serial".to_string(),
                "chardev:debug".to_string(),
                "-mon".to_string(),
                "chardev=debug".to_string(),
                flag.to_string(),
                value.to_string(),
                "-chardev".to_string(),
                "stdio,id=debug,signal=off,mux=on".to_string(),
                "-semihosting-config".to_string(),
                "enable=on,target=native,userspace=on".to_string(),
            ]
        }
        "riscv64gc-unknown-redox" => {
            let (flag, value) = uefi_firmware(arch)?;
            vec![
                "-machine".to_string(),
                "virt,acpi=off".to_string(),
                "-serial".to_string(),
                "chardev:debug".to_string(),
                "-mon".to_string(),
                "chardev=debug".to_string(),
                flag.to_string(),
                value.to_string(),
                "-chardev".to_string(),
                "stdio,id=debug,signal=off,mux=on".to_string(),
            ]
        }
        other => anyhow::bail!("unknown target architecture for QEMU: {other}"),
    })
}

/// The arguments that do not depend on the architecture or on firmware.
fn common_qemu_args() -> Vec<String> {
    [
        "-cpu",
        "max",
        "-m",
        "2048",
        "-smp",
        "4",
        "-netdev",
        "user,id=net0",
        "-device",
        "e1000,netdev=net0",
    ]
    .iter()
    .map(|argument| argument.to_string())
    .collect()
}

/// QEMU's arguments for a guest of this build's target.
///
/// # Errors
/// Returns an error when a UEFI architecture has no firmware installed; see
/// [`uefi_firmware`].
pub fn qemu_default_args() -> anyhow::Result<Vec<String>> {
    let arch = target();
    let mut default_args = common_qemu_args();
    default_args.extend(machine_args(arch)?);
    Ok(default_args)
}

static BASE_TOML: &str = include_str!("../res/base.toml");
static GUI_TOML: &str = include_str!("../res/gui.toml");

/// Identifies what an image built from `cookbook` contains: the checkout
/// and the name, size and modification time of each built archive whose
/// file name passes `filter`. A rebuilt package changes it.
fn cookbook_stamp(cookbook: &Path, filter: impl Fn(&str) -> bool) -> io::Result<String> {
    let repo = cookbook.join("repo").join(target());
    let mut entries = Vec::new();
    for entry in fs::read_dir(&repo)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !filter(&name) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = metadata
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        entries.push(format!("{name} {} {modified}", metadata.len()));
    }
    entries.sort();
    let cookbook = cookbook.canonicalize()?;
    Ok(format!(
        "cookbook {}\n{}\n",
        cookbook.display(),
        entries.join("\n")
    ))
}

/// Removes `files` if `stamp_file` does not hold `stamp`, so they are built
/// again from the current inputs.
fn clear_stale(name: &str, stamp_file: &Path, stamp: &str, files: &[&Path]) -> io::Result<()> {
    if fs::read_to_string(stamp_file).is_ok_and(|old| old == stamp) {
        return Ok(());
    }
    if files.iter().any(|file| file.exists()) {
        eprintln!("redoxer: clearing old {name}");
    }
    for file in files.iter().copied().chain([stamp_file]) {
        match fs::remove_file(file) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    Ok(())
}

fn bootloader(cookbook: &Path) -> anyhow::Result<PathBuf> {
    let bootloader_bin = redoxer_dir().join("bootloader.bin");
    let stamp_file = redoxer_dir().join("bootloader.stamp");
    let stamp = cookbook_stamp(cookbook, |name| name.starts_with("bootloader."))
        .context("unable to read the cookbook's repository")?;
    clear_stale("bootloader", &stamp_file, &stamp, &[&bootloader_bin])?;
    if !bootloader_bin.is_file() {
        eprintln!("redoxer: building bootloader");

        let bootloader_dir = redoxer_dir().join("bootloader");
        if bootloader_dir.is_dir() {
            fs::remove_dir_all(&bootloader_dir)?;
        }
        fs::create_dir_all(&bootloader_dir)?;

        let mut config = redox_installer::Config::default();
        config.general.cookbook = Some(cookbook.display().to_string());
        config
            .packages
            .insert("bootloader".to_string(), Default::default());
        redox_installer::install(config, &bootloader_dir)
            .map_err(|err| io::Error::other(format!("{err}")))?;

        // Choose by what the package actually contains, not by architecture:
        // Ferric Oxide builds a UEFI-only x86_64 image, while an upstream-style
        // build still ships the BIOS one.
        use bootloader_firmware::Firmware;
        let installed = [
            ("usr/lib/boot/bootloader-live.efi", Firmware::Uefi),
            ("usr/lib/boot/bootloader-live.bios", Firmware::Bios),
        ];
        let (found, firmware) = installed
            .into_iter()
            .find(|(path, _)| bootloader_dir.join(path).is_file())
            .with_context(|| {
                format!(
                    "the bootloader package in {} has neither a UEFI nor a BIOS boot image",
                    cookbook.display()
                )
            })?;
        bootloader_firmware::set(firmware);
        fs::rename(bootloader_dir.join(found), &bootloader_bin)?;
        fs::write(&stamp_file, stamp)?;
    }
    Ok(bootloader_bin)
}

/// creating a base image, returns the base image, bool if orbital exists
fn base(
    // Only the mounting path reads it; the archive path does not.
    #[cfg_attr(not(feature = "fuse"), allow(unused_variables))] bootloader_bin: &Path,
    name: &str,
    mut config: redox_installer::Config,
    config_str: &str,
    cookbook: &Path,
    fuse: bool,
) -> anyhow::Result<(PathBuf, bool)> {
    let ext = if fuse { "bin" } else { "tar" };

    let base_file = redoxer_dir().join(format!("{name}.{ext}"));
    let base_tar = redoxer_dir().join(format!("{}.{}", name, "tar"));
    let base_toml = redoxer_dir().join(format!("{}.{}", name, "toml"));

    let has_orbital = config.packages.contains_key("orbital");

    // The image depends on its configuration and on the packages it is
    // built from, so either changing rebuilds it. The repository index is
    // rewritten on every publish, so it is left out. A FUSE build starts
    // from an existing tar image, so a stale one goes too.
    let stamp = cookbook_stamp(cookbook, |name| name != "repo.toml")
        .context("unable to read the cookbook's repository")?;
    let stamp = format!("{config_str}\n# {stamp}");
    clear_stale(name, &base_toml, &stamp, &[&base_file, &base_tar])?;
    if !base_file.is_file() {
        eprintln!("redoxer: building {name}");

        let base_dir = redoxer_dir().join(name);
        if base_dir.is_dir() {
            fs::remove_dir_all(&base_dir)?;
        }
        fs::create_dir_all(&base_dir)?;

        let base_partial = redoxer_dir().join(format!("{name}.{ext}.partial"));
        if base_partial.is_file() {
            fs::remove_file(&base_partial)?;
        }

        config.general.live_disk = Some(qemu_use_live_disk());
        config.general.cookbook = Some(cookbook.display().to_string());

        #[cfg(feature = "fuse")]
        if fuse {
            run_install_mount(
                config,
                bootloader_bin,
                qemu_use_uefi(),
                qemu_base_disk_size(),
                &base_tar,
                &base_dir,
                &base_partial,
            )?;

            // only shrink disk outside CI
            if !base_tar.exists() {
                eprintln!("redoxer: shrinking {name}");
                // The bootloader reads the whole image into memory, so the
                // image has to leave room in the guest's RAM. The EFI partition
                // the boot image lives in sits at a fixed offset, so resizing
                // the filesystem does not move it.
                shrink_disk(&base_partial)?;
            }
        } else {
            #[cfg(not(feature = "fuse"))]
            require_no_fuse(fuse)?;
            // Without FUSE the tree is installed into a directory and written
            // into a whole-disk image here; that layout is the one QEMU runs.
            redox_installer::install(config, &base_dir)
                .map_err(|err| io::Error::other(format!("{err}")))?;
            archive_image(
                &base_partial,
                &base_dir,
                bootloader_bin,
                qemu_use_uefi(),
                qemu_base_disk_size(),
            )?;
        }

        fs::rename(&base_partial, &base_file)?;
        if base_dir.is_dir() {
            fs::remove_dir_all(&base_dir)?;
        }
        fs::write(base_toml, stamp)?;
    }
    Ok((base_file, has_orbital))
}

/// A mounted image, which unmounts when dropped.
#[cfg(feature = "fuse")]
type MountedImage = RedoxFs;

/// A build without the `fuse` feature never mounts an image.
#[cfg(not(feature = "fuse"))]
type MountedImage = core::convert::Infallible;

/// Puts the image's contents in `dest_dir`, mounting the image when FUSE is
/// asked for and extracting the archive otherwise.
///
/// A build without the `fuse` feature has no mounting code, so asking for FUSE
/// there is a configuration error rather than a silent fallback to the other
/// path: the two produce different images, and quietly changing which one runs
/// would hide the mistake.
fn install_into_disk(
    fuse: bool,
    redoxer_bin: &Path,
    dest_dir: &Path,
    base_file: &Path,
) -> anyhow::Result<Option<MountedImage>> {
    #[cfg(feature = "fuse")]
    {
        if fuse {
            return Ok(Some(
                RedoxFs::new(redoxer_bin, dest_dir).context("unable to init redoxfs")?,
            ));
        }
        let _ = redoxer_bin;
        extract_tar(base_file, dest_dir)?;
        Ok(None)
    }
    // Without FUSE the archive path is the only one; it needs no mount.
    #[cfg(not(feature = "fuse"))]
    {
        let _ = redoxer_bin;
        if fuse {
            return require_no_fuse(true);
        }
        extract_tar(base_file, dest_dir)?;
        Ok(None)
    }
}

/// Fails when a build without the `fuse` feature is asked to mount an image.
#[cfg(not(feature = "fuse"))]
fn require_no_fuse(fuse: bool) -> anyhow::Result<Option<MountedImage>> {
    if fuse {
        anyhow::bail!(
            "redoxer was built without the fuse feature, so it cannot mount images; \
             rebuild without REDOXER_USE_FUSE or with --features fuse"
        );
    }
    Ok(None)
}

fn apply_qemu_args(cmd: &mut Command, default: Vec<&str>, args_opt: Option<Vec<&str>>) {
    let final_args = if let Some(user_args) = args_opt {
        let user_opts: HashSet<&str> = user_args
            .iter()
            .filter(|arg| arg.starts_with('-'))
            .copied()
            .collect();

        let mut merged_args: Vec<String> = Vec::new();
        let mut i = 0;
        while i < default.len() {
            let opt = &default[i];
            if !opt.starts_with('-') {
                continue; // shouldn't go here
            }

            let is_single_flag = default.get(i + 1).is_none_or(|a| a.starts_with('-'));

            if !user_opts.contains(opt) {
                merged_args.push(opt.to_string());
                if !is_single_flag {
                    merged_args.push(default[i + 1].to_string());
                }
            }

            i += if is_single_flag { 1 } else { 2 };
        }

        merged_args.extend(user_args.into_iter().map(String::from));
        merged_args
    } else {
        default.into_iter().map(String::from).collect()
    };

    cmd.args(final_args);
}

fn installed(program: &str) -> io::Result<bool> {
    process::Command::new("which")
        .arg(program)
        .stdout(process::Stdio::null())
        .status()
        .map(|x| x.success())
}

/// Boots `image` and returns the guest's exit code as redoxer reports it.
///
/// `image` is the whole disk to boot. The log goes to a temporary file so the
/// caller can echo it back, and `--output -` hides the guest's console instead.
///
/// # Errors
/// Returns an error when QEMU cannot be started, its status cannot be read, the
/// log cannot be copied, or the UEFI firmware is missing.
fn run_qemu(
    config: &RedoxerExecConfig,
    qemu_binary: &str,
    image: &Path,
    gui: bool,
    qemu_verbose: bool,
) -> anyhow::Result<i32> {
    // The log lives beside the image, which is inside the caller's tempdir, so
    // it is removed with it.
    let redoxer_log = image.with_extension("log");
    let mut command = Command::new(qemu_binary);

    let chardev = format!("file,id=log,path={}", redoxer_log.display());
    let drive = format!(
        "file={},format=raw,if={}",
        image.display(),
        qemu_disk_device()
    );
    let mut default_args = qemu_default_args()?;
    default_args.push("-chardev".to_string());
    default_args.push(chardev);
    default_args.push("-drive".to_string());
    default_args.push(drive);
    if config.kvm {
        default_args.push("-accel".to_string());
        default_args.push("kvm".to_string());
    }
    if !gui {
        default_args.push("-nographic".to_string());
        default_args.push("-vga".to_string());
        default_args.push("none".to_string());
    }

    let default_args: Vec<&str> = default_args.iter().map(String::as_str).collect();
    apply_qemu_args(
        &mut command,
        default_args,
        config.qemu_args.as_ref().map(|s| s.split(' ').collect()),
    );

    if config.output.as_ref().is_some_and(|s| s == "-") {
        command.stdout(Stdio::null());
        command.stderr(Stdio::null());
    }

    if qemu_verbose {
        eprintln!("{command:?}");
    }
    let status = command.status().context("unable to get redoxer status")?;

    eprintln!();

    let code = match status.code() {
        Some(51) => {
            eprintln!("## redoxer (success) ##");
            0
        }
        Some(53) => {
            eprintln!("## redoxer (failure) ##");
            1
        }
        _ => {
            eprintln!("## redoxer (failure, qemu exit status {status}) ##");
            2
        }
    };

    match config.output.as_deref() {
        Some("-") => print!("{}", fs::read_to_string(&redoxer_log)?),
        Some(output) => fs::copy(&redoxer_log, output).map(|_| {})?,
        None => {}
    }

    Ok(code)
}

/// Boots an already-installed whole-disk image and runs the command in it.
///
/// This is `--image`: the caller prepared the image, so nothing is installed
/// and no bootloader is extracted into it. Booting a copy keeps the prepared
/// image exactly as it was — a build reuses it for every tool it runs, and
/// recipes cook in parallel — while the per-invocation files (`/etc/redoxerd`
/// and the folders to copy) go into the copy alone.
///
/// # Errors
/// Returns an error when the image does not exist, QEMU is missing, the copy
/// fails, a file cannot be written into the copy, or QEMU cannot be run.
/// `--artifact` is refused: redoxer writes into an image but does not mount
/// it, so it cannot read files back out of one.
fn inner_image(config: &RedoxerExecConfig) -> anyhow::Result<i32> {
    let path = config
        .image
        .as_ref()
        .context("--image needs a path to a guest image")?;
    // The mode boots what is there. A missing image is an environment that was
    // never installed, not a request to install a new one, so it fails where it
    // is noticed rather than silently building another image.
    if !path.is_file() {
        anyhow::bail!(
            "guest image {} does not exist; install it before running a command in it \
             (`make build_tools`)",
            path.display()
        );
    }
    let image = fs::canonicalize(path)
        .with_context(|| format!("unable to resolve the guest image {}", path.display()))?;

    let qemu_binary = config
        .qemu_binary
        .as_deref()
        .unwrap_or_else(|| qemu_executable());
    if !installed(qemu_binary)? {
        anyhow::bail!("{qemu_binary} not found, please install before continuing");
    }

    let tempdir = tempfile::tempdir().context("unable to create tempdir")?;
    let working_image = tempdir.path().join("redoxer.img");

    // A copy, not the image itself: the files below are written into it, and
    // the environment is shared by every command a build runs. `--sparse=always`
    // keeps a mostly empty image the size of its contents.
    Command::new("cp")
        .arg("--sparse=always")
        .arg(&image)
        .arg(&working_image)
        .status()
        .and_then(status_error)
        .context("copying the guest image failed")?;

    let mut files = redoxerd_config_files(
        &config.arguments,
        config.folders.get("root").map(String::as_str),
    )?;
    files.extend(folder_files(config, &image)?);
    if !files.is_empty() {
        crate::redoxfs::write_files(&working_image, &files)
            .context("unable to write into the guest image")?;
    }

    let code = run_qemu(config, qemu_binary, &working_image, false, false)?;

    if code == 0 && !config.artifacts.is_empty() {
        anyhow::bail!(
            "--artifact cannot read files out of a --image guest: redoxer writes into an image \
             but does not mount it, so use REDOXER_USE_FUSE with an install configuration"
        );
    }

    tempdir.close()?;
    Ok(code)
}

/// The files that put a `--folder` source into the guest image.
///
/// Each folder is read from the host and written under its destination, so the
/// image needs no mount and no `rsync`. Only regular files and directories are
/// copied: a build passes source trees, and following a symlink out of one
/// would write host files into the image at a path the build did not name.
///
/// # Errors
/// Returns an error when a source directory cannot be walked or a destination
/// path is not image-absolute.
fn folder_files(
    config: &RedoxerExecConfig,
    image: &Path,
) -> anyhow::Result<Vec<(PathBuf, Vec<u8>, u16)>> {
    let mut files = Vec::new();
    for (sysroot, folder) in config.folders.iter() {
        let destination = redox_installer::image_path(image, Path::new(sysroot))?;
        let destination = destination.strip_prefix(image).with_context(|| {
            format!("the destination of {folder} is not inside the guest image")
        })?;
        copy_into(
            folder.as_ref(),
            Path::new("/").join(destination),
            &mut files,
            0,
        )?;
    }
    Ok(files)
}

/// Reads the tree at `source` into `target` in the guest.
///
/// `depth` bounds the walk so a symlink loop cannot spin here; the guest
/// directories a build copies are shallow.
///
/// # Errors
/// Returns an error when a directory cannot be read or a file cannot be read.
fn copy_into(
    source: &Path,
    target: PathBuf,
    files: &mut Vec<(PathBuf, Vec<u8>, u16)>,
    depth: usize,
) -> anyhow::Result<()> {
    const MAX_DEPTH: usize = 64;
    if depth > MAX_DEPTH {
        anyhow::bail!(
            "{} is nested deeper than {MAX_DEPTH} directories",
            source.display()
        );
    }
    let metadata = fs::symlink_metadata(source)
        .with_context(|| format!("unable to read {}", source.display()))?;
    if metadata.is_dir() {
        for entry in
            fs::read_dir(source).with_context(|| format!("unable to list {}", source.display()))?
        {
            let entry = entry?;
            copy_into(
                &entry.path(),
                target.join(entry.file_name()),
                files,
                depth + 1,
            )?;
        }
    } else if metadata.is_file() {
        let data =
            fs::read(source).with_context(|| format!("unable to read {}", source.display()))?;
        // Keep the source's mode so an executable stays executable in the guest.
        let mode = std::os::unix::fs::PermissionsExt::mode(&metadata.permissions()) as u16 & 0o777;
        files.push((target, data, mode));
    }
    Ok(())
}

fn inner(config: &RedoxerExecConfig) -> anyhow::Result<i32> {
    if config.image.is_some() {
        return inner_image(config);
    }

    let qemu_binary = config
        .qemu_binary
        .as_deref()
        .unwrap_or_else(|| qemu_executable());
    // it is unusual to request custom qemu binary
    let qemu_verbose = config.qemu_binary.is_some();

    if !installed(qemu_binary)? {
        eprintln!(
            "redoxer: {} not found, please install before continuing",
            qemu_binary
        );
        process::exit(1);
    }
    let fuse = config.fuse;

    if fuse {
        if !installed("fusermount")? {
            eprintln!("redoxer: fuse not found, please install before continuing");
            process::exit(1);
        }
    } else if !installed("tar")? {
        eprintln!("redoxer: tar not found, please install before continuing");
        process::exit(1);
    }

    let install_config: redox_installer::Config =
        toml::from_str(&config.config_toml).context("Unable to parse install-config")?;
    // The bootloader and the image come from the same cookbook: the install
    // config's, or else REDOXER_COOKBOOK's.
    let cookbook = match &install_config.general.cookbook {
        Some(cookbook) => PathBuf::from(cookbook),
        None => crate::cookbook_dir()?,
    };
    let bootloader_bin = bootloader(&cookbook).context("unable to init bootloader")?;
    let (base_file, gui) = base(
        &bootloader_bin,
        &config.config_name,
        install_config,
        &config.config_toml,
        &cookbook,
        fuse,
    )
    .context("unable to init base")?;

    eprintln!("redoxer: creating temporary disk");
    let tempdir = tempfile::tempdir().context("unable to create tempdir")?;
    let redoxer_bin = tempdir.path().join("redoxer.bin");
    let dest_dir = tempdir.path().join("redoxer");

    let code = {
        if fuse {
            Command::new("cp")
                .arg(&base_file)
                .arg(&redoxer_bin)
                .status()
                .and_then(status_error)
                .context("copy base to redoxer bin failed")?;

            // The bootloader loads the whole image into RAM before the kernel
            // runs, so the working copy cannot grow past what fits beside it:
            // a larger disk would be one the guest cannot boot.
            expand_disk(&redoxer_bin, qemu_working_disk_size())?;
        }

        fs::create_dir_all(&dest_dir).context("unable to create redoxer dir")?;

        {
            let redoxfs_opt = install_into_disk(fuse, &redoxer_bin, &dest_dir, &base_file)?;

            write_redoxerd_config(
                &dest_dir,
                &config.arguments,
                config.folders.get("root").map(std::string::String::as_str),
            )?;

            for (sysroot, folder) in config.folders.iter() {
                eprintln!("redoxer: copying '{folder}' to '/{sysroot}'");

                let dst_dir = redox_installer::image_path(&dest_dir, Path::new(sysroot))?;
                if !dst_dir.is_dir() {
                    fs::create_dir_all(&dst_dir)
                        .context("unable to create destination directory")?;
                }
                Command::new("rsync")
                    .arg("--archive")
                    .arg(folder)
                    .arg(&dst_dir)
                    .status()
                    .and_then(status_error)
                    .context("rsync failed")?;
            }

            #[cfg(feature = "fuse")]
            if let Some(mut redoxfs) = redoxfs_opt {
                redoxfs.unmount().context("unable to unmount")?;
            }
            #[cfg(not(feature = "fuse"))]
            let _ = redoxfs_opt;
        }

        if !fuse {
            archive_image(
                &redoxer_bin,
                &dest_dir,
                &bootloader_bin,
                qemu_use_uefi(),
                qemu_disk_size(),
            )?;
        }

        run_qemu(config, qemu_binary, &redoxer_bin, gui, qemu_verbose)?
    };

    if code == 0 && !config.artifacts.is_empty() {
        let redoxfs_opt = install_into_disk(fuse, &redoxer_bin, &dest_dir, &base_file)?;

        for (sysroot, folder) in config.artifacts.iter() {
            eprintln!("redoxer: copying '/{sysroot}' to '{folder}'");

            let dst_dir = Path::new(folder);
            if !dst_dir.is_dir() {
                fs::create_dir_all(dst_dir).context("unable to create destination directory")?;
            }
            Command::new("rsync")
                .arg("--archive")
                .arg(format!(
                    "{}/",
                    redox_installer::image_path(&dest_dir, Path::new(sysroot))?.display()
                ))
                .arg(dst_dir)
                .status()
                .and_then(status_error)
                .context("rsync failed")?;
        }

        #[cfg(feature = "fuse")]
        if let Some(mut redoxfs) = redoxfs_opt {
            redoxfs.unmount().context("unable to unmount")?;
        }
        #[cfg(not(feature = "fuse"))]
        let _ = redoxfs_opt;
    }

    tempdir.close()?;

    Ok(code)
}

fn usage() -> ! {
    eprintln!(
        "redoxer exec [options...] [--] <command> [arguments]... \n\
        options: \
        \n    [-f|--folder folder] \
        \n    [-f|--folder folder:/path/in/redox] \
        \n    [-a|--artifact folder] \
        \n    [-a|--artifact folder:/path/in/redox] \
        \n    [-g|--gui] \
        \n    [-h|--help] \
        \n    [-i|--install-config file] \
        \n    [-o|--output file]"
    );
    eprintln!();
    eprintln!("redoxer exec - /dev/kvm: {:?}", kvm_accessible());
    eprintln!("redoxer exec - /dev/fuse: {:?}", fuse_accessible());
    process::exit(1);
}

#[derive(Clone, Default)]
pub struct RedoxerExecConfig {
    // Qemu config
    pub qemu_binary: Option<String>,
    pub qemu_args: Option<String>,
    pub fuse: bool,
    pub kvm: bool,
    // Installer config
    pub config_name: String,
    pub config_toml: String,
    pub install_config: Option<PathBuf>,
    /// A whole-disk image to boot as it is, instead of installing one from
    /// `install_config`. A build that prepared an environment (the guest build
    /// tools) boots that one: the tools are in it, and installing from a base
    /// configuration would produce an image without them.
    pub image: Option<PathBuf>,
    // Folders to copy (host -> qemu)
    pub folders: HashMap<String, String>,
    // Folders to extract (qemu -> host)
    pub artifacts: HashMap<String, String>,
    // Output log
    pub output: Option<String>,
    // Commands to execute
    pub arguments: Vec<String>,
}

impl RedoxerExecConfig {
    pub fn new(mut args: impl Iterator<Item = String>) -> anyhow::Result<Self> {
        use std::env::var;

        fn parse_folder(
            map: &mut HashMap<String, String>,
            folder: String,
            argname: &str,
        ) -> anyhow::Result<()> {
            let (dir, sysroot) = if let Some((dir, destination)) = folder.split_once(':') {
                if destination.contains(':') {
                    bail!("{argname} can be 'directory' or 'directory:path'");
                }
                if !destination.starts_with('/') {
                    bail!("path on {argname} with format 'directory:path' must be an absolute path (starting with '/')");
                }
                (dir.to_string(), destination.to_string())
            } else {
                (folder, "/root".to_string())
            };
            let sysroot = redox_installer::image_relative_path(Path::new(&sysroot))?;
            if map
                .insert(sysroot.to_string_lossy().into_owned(), dir)
                .is_some()
            {
                bail!(
                    "path {:?} on {argname} with format 'directory:path' must be unique",
                    sysroot
                );
            }

            Ok(())
        }

        let mut config = RedoxerExecConfig {
            qemu_binary: var("REDOXER_QEMU_BINARY").ok(),
            qemu_args: var("REDOXER_QEMU_ARGS").ok(),
            fuse: parse_bool_env("REDOXER_USE_FUSE").unwrap_or_else(|| fuse_accessible().is_ok()),
            kvm: parse_bool_env("REDOXER_USE_KVM").unwrap_or_else(|| qemu_has_kvm()),
            config_name: "base".into(),
            config_toml: BASE_TOML.into(),
            // other options should be passed from args
            ..Default::default()
        };

        // Matching flags
        let mut matching = true;
        while let Some(arg) = args.next() {
            match (arg.as_str(), matching) {
                ("-f" | "--folder", true) => match args.next() {
                    Some(folder) => parse_folder(&mut config.folders, folder, "--folder")?,
                    None => bail!("--folder requires a path to a directory"),
                },
                ("-a" | "--artifact", true) => match args.next() {
                    Some(folder) => parse_folder(&mut config.artifacts, folder, "--artifact")?,
                    None => bail!("--folder requires a path to a directory"),
                },
                ("-g" | "--gui", true) => {
                    config.config_name = "gui".into();
                    config.config_toml = GUI_TOML.into();
                    config.install_config = None;
                }
                ("--image", true) => match args.next() {
                    Some(image) => {
                        if config.install_config.is_some() {
                            bail!("--image and --install-config cannot be combined");
                        }
                        config.image = Some(PathBuf::from(image));
                    }
                    None => bail!("--image requires a path to a guest image"),
                },
                ("-i" | "--install-config", true) => match args.next() {
                    Some(file) => {
                        let path = Path::new(&file);
                        config.config_name = path
                            .file_stem()
                            .ok_or_else(|| anyhow::anyhow!("--install-config requires a filename"))?
                            .to_string_lossy()
                            .to_string();
                        if &config.config_name == "base" || &config.config_name == "gui" {
                            bail!("--install-config file path cannot be 'base' or 'gui'");
                        }
                        config.config_toml = fs::read_to_string(path)
                            .context("unable to read --install-config file")?;
                        config.install_config = Some(fs::canonicalize(path)?);
                        config.image = None;
                    }
                    None => bail!("--install-config requires a path to a file"),
                },
                ("-h" | "--help", true) => bail!(""),
                ("-o" | "--output", true) => match args.next() {
                    Some(output) => config.output = Some(output),
                    None => bail!("--output requires a path to a directory"),
                },
                ("--", true) => matching = false,
                _ => {
                    matching = false;
                    config.arguments.push(arg);
                }
            }
        }

        if !config.folders.contains_key("root")
            && let Some(cmd) = config.arguments.first()
            && Path::new(cmd).is_file()
        {
            if !cmd.contains('/') {
                eprintln!(
                    "WARN: Skipping copy, you might mean to run exec with ./{}",
                    cmd
                )
            } else {
                config.folders.insert("root".to_string(), cmd.to_string());
            }
        }

        #[cfg(feature = "cli-pkg")]
        if let Some(sysroot) = crate::pkg::get_sysroot() {
            parse_folder(
                &mut config.folders,
                format!("{}/:/", sysroot.display()),
                "REDOXER_SYSROOT",
            )?
        }

        if !config.artifacts.is_empty() && !config.fuse {
            bail!("--artifact requires REDOXER_USE_FUSE=true");
        }

        Ok(config)
    }

    pub fn to_args(&self) -> Vec<String> {
        let mut args = Vec::new();

        for (sysroot, host_dir) in &self.folders {
            args.push("--folder".to_string());
            args.push(format!("{host_dir}:/{sysroot}"));
        }

        for (sysroot, host_dir) in &self.artifacts {
            args.push("--artifact".to_string());
            args.push(format!("{host_dir}:/{sysroot}"));
        }

        if let Some(path) = &self.image {
            args.push("--image".to_string());
            args.push(path.to_string_lossy().into_owned());
        } else if let Some(path) = &self.install_config {
            args.push("--install-config".to_string());
            args.push(path.to_string_lossy().into_owned());
        } else if self.config_name == "gui" {
            args.push("--gui".to_string());
        }

        if let Some(ref output) = self.output {
            args.push("--output".to_string());
            args.push(output.clone());
        }

        if !self.arguments.is_empty() {
            args.push("--".to_string());

            for arg in &self.arguments {
                args.push(arg.clone());
            }
        }

        args
    }
}

pub fn main(args: &[String]) {
    let config = match RedoxerExecConfig::new(args.iter().skip(2).cloned()) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("{err:?}");
            usage();
        }
    };

    if config.arguments.is_empty() {
        usage();
    }
    match inner(&config) {
        Ok(code) => {
            process::exit(code);
        }
        Err(err) => {
            eprintln!("redoxer exec: {err:#}");
            process::exit(3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--image` boots an installed image, so it cannot be combined with the
    /// install configuration `--install-config` provides instead.
    #[test]
    fn image_mode_takes_a_path_and_rules_out_an_install_config() {
        let config =
            RedoxerExecConfig::new(["--image".into(), "guest.img".into()].into_iter()).unwrap();
        assert_eq!(config.image, Some(PathBuf::from("guest.img")));

        // A later --install-config replaces the mode rather than mixing them.
        // A configuration file also names the configuration, and `base`/`gui`
        // are the built-in ones, so this one is its own.
        let temporary = tempfile::tempdir().unwrap();
        let install_config = temporary.path().join("custom.toml");
        std::fs::write(&install_config, "[general]\nfilesystem_size = 512\n").unwrap();
        let config = RedoxerExecConfig::new(
            [
                "--image".into(),
                "guest.img".into(),
                "--install-config".into(),
                install_config.to_string_lossy().into_owned(),
            ]
            .into_iter(),
        )
        .unwrap();
        assert_eq!(config.image, None);
        assert!(config.install_config.is_some());

        // The other order is refused, because the image would silently win.
        let error = RedoxerExecConfig::new(
            [
                "--install-config".into(),
                install_config.to_string_lossy().into_owned(),
                "--image".into(),
                "guest.img".into(),
            ]
            .into_iter(),
        )
        .err()
        .expect("--image and --install-config cannot be combined");
        assert!(
            error.to_string().contains("cannot be combined"),
            "unexpected error: {error}"
        );

        let error = RedoxerExecConfig::new(["--image".into()].into_iter())
            .err()
            .expect("--image needs a value");
        assert!(error.to_string().contains("--image requires a path"));
    }

    /// `redoxer cargo` re-executes `redoxer exec` with the arguments it was
    /// given, so the image has to survive that round trip.
    #[test]
    fn image_mode_survives_a_round_trip_through_to_args() {
        let config = RedoxerExecConfig::new(
            [
                "--image".into(),
                "guest.img".into(),
                "--".into(),
                "mdbook".into(),
            ]
            .into_iter(),
        )
        .unwrap();
        let args = config.to_args();
        assert!(args.contains(&"--image".to_string()), "{args:?}");
        let rebuilt = RedoxerExecConfig::new(args.into_iter()).unwrap();
        assert_eq!(rebuilt.image, Some(PathBuf::from("guest.img")));
        assert_eq!(rebuilt.arguments, ["mdbook"]);
    }

    /// A missing firmware is a setup problem the user can fix, so it is an
    /// error naming what to install rather than a panic. The lookup runs
    /// against a path that cannot exist, so the test does not depend on which
    /// firmware this host happens to have.
    #[test]
    fn a_missing_firmware_names_the_packages_to_install() {
        for arch in [
            "x86_64-unknown-redox",
            "aarch64-unknown-redox",
            "riscv64gc-unknown-redox",
        ] {
            let error = uefi_firmware_among(arch, &[("/nonexistent/firmware.fd", "value")])
                .expect_err("a firmware that does not exist is an error");
            let message = error.to_string();
            assert!(message.contains(arch), "{message}");
            assert!(
                message.contains("/nonexistent/firmware.fd"),
                "the message lists where it looked: {message}"
            );
            // The caller's wrapper is what tells the user what to install.
            let (_, candidates, packages) = FIRMWARE_SEARCHES
                .iter()
                .find(|(name, _, _)| *name == arch)
                .unwrap();
            assert!(!candidates.is_empty(), "{arch}");
            assert!(!packages.is_empty(), "{arch} names where to get it");
        }
        // An architecture with no entry at all is rejected, not guessed.
        assert!(uefi_firmware("mips-unknown-redox").is_err());
    }

    /// The search table is the only record of where firmware lives, so every
    /// architecture `machine_args` handles must have an entry.
    #[test]
    fn every_uefi_architecture_has_a_firmware_search() {
        for arch in [
            "x86_64-unknown-redox",
            "aarch64-unknown-redox",
            "riscv64gc-unknown-redox",
        ] {
            assert!(
                FIRMWARE_SEARCHES.iter().any(|(name, _, _)| *name == arch),
                "{arch} needs a firmware search"
            );
        }
        // i586/i686 are BIOS only: they must not claim UEFI firmware.
        for arch in ["i586-unknown-redox", "i686-unknown-redox"] {
            assert!(
                !FIRMWARE_SEARCHES.iter().any(|(name, _, _)| *name == arch),
                "{arch} boots through BIOS"
            );
        }
    }

    #[test]
    fn folder_destinations_reject_traversal() {
        for path in ["src:/../../outside", "src:/etc/../outside"] {
            assert!(RedoxerExecConfig::new(["--folder".into(), path.into()].into_iter()).is_err());
        }
        let config =
            RedoxerExecConfig::new(["--folder".into(), "src:/etc/./data".into()].into_iter())
                .unwrap();
        assert_eq!(config.folders.get("etc/data"), Some(&"src".to_string()));
    }

    #[test]
    fn stale_images_are_cleared_when_the_cookbook_changes() {
        let dir = tempfile::tempdir().unwrap();
        let make_cookbook = |name: &str| {
            let cookbook = dir.path().join(name);
            let repo = cookbook.join("repo").join(target());
            fs::create_dir_all(&repo).unwrap();
            fs::write(repo.join("bootloader.pkgar"), name).unwrap();
            fs::write(repo.join("kernel.pkgar"), name).unwrap();
            cookbook
        };
        let first = make_cookbook("first");
        let second = make_cookbook("second");
        let image = dir.path().join("base.tar");
        let stamp_file = dir.path().join("base.toml");

        let stamp = cookbook_stamp(&first, |_| true).unwrap();
        fs::write(&image, "image").unwrap();
        fs::write(&stamp_file, &stamp).unwrap();
        clear_stale("base", &stamp_file, &stamp, &[&image]).unwrap();
        assert!(image.is_file(), "an unchanged cookbook keeps the image");

        let other = cookbook_stamp(&second, |_| true).unwrap();
        clear_stale("base", &stamp_file, &other, &[&image]).unwrap();
        assert!(!image.exists() && !stamp_file.exists());

        let kernel_only = |name: &str| name.starts_with("kernel.");
        let before = cookbook_stamp(&first, |name| name.starts_with("bootloader.")).unwrap();
        let kernel = cookbook_stamp(&first, kernel_only).unwrap();
        fs::write(
            first.join("repo").join(target()).join("kernel.pkgar"),
            "rebuilt",
        )
        .unwrap();
        assert_ne!(cookbook_stamp(&first, kernel_only).unwrap(), kernel);
        assert_eq!(
            cookbook_stamp(&first, |name| name.starts_with("bootloader.")).unwrap(),
            before,
            "the bootloader ignores other packages"
        );
    }
}
