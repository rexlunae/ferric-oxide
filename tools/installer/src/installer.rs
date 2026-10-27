//! Package, account and disk installation orchestrated around RedoxFS callbacks.

use anyhow::Context;
use anyhow::{Result, bail};
use fox_store::deps::{LocalRepository, PlanError, Request};
use rand::{TryRngCore, rngs::OsRng};
use redoxfs::{BLOCK_SIZE, Disk, DiskIo, FileSystem, unmount_path};
use termion::input::TermRead;

use crate::config::Config;
use crate::config::file::FileConfig;
use crate::config::package::PackageConfig;
use crate::disk_wrapper::DiskWrapper;

use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process,
    sync::mpsc::channel,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

/// Disk layout, encryption and bootloader inputs for image creation.
pub struct DiskOption<'a> {
    /// Legacy bootloader bytes; an empty slice omits them.
    pub bootloader_bios: &'a [u8],
    /// UEFI bootloader bytes to place in the EFI system partition.
    pub bootloader_efi: &'a [u8],
    /// Optional password for filesystem encryption.
    pub password_opt: Option<&'a [u8]>,
    /// Optional EFI partition size in MiB.
    pub efi_partition_size: Option<u32>, //MiB
    /// Optional existing EFI partition path.
    pub efi_partition_path: Option<&'a str>,
    /// Treat the disk as a filesystem partition without creating a partition table.
    pub skip_partitions: bool,
}

/// Return the installation target from `TARGET`, or the architecture default.
pub fn get_target() -> String {
    // TODO: Configurable from filesystem config?
    env::var("TARGET").unwrap_or_else(|_| {
        if cfg!(target_arch = "aarch64") {
            "aarch64-unknown-redox"
        } else if cfg!(target_arch = "x86") {
            "i586-unknown-redox"
        } else if cfg!(target_arch = "riscv64") {
            "riscv64gc-unknown-redox"
        } else {
            "x86_64-unknown-redox"
        }
        .to_string()
    })
}

/// Converts a password to a serialized argon2rs hash, understandable
/// by redox_users. If the password is blank, the hash is blank.
fn hash_password(password: &str) -> Result<String> {
    if !password.is_empty() {
        let salt = format!("{:X}", OsRng.try_next_u64()?);
        let config = argon2::Config::default();
        let hash = argon2::hash_encoded(password.as_bytes(), salt.as_bytes(), &config)?;
        Ok(hash)
    } else {
        Ok("".into())
    }
}

fn syscall_error(err: syscall::Error) -> io::Error {
    io::Error::from_raw_os_error(err.errno)
}

/// Returns a password collected from the user (plaintext)
/// Read a password twice, returning `None` for an empty password.
///
/// # Errors
/// Returns terminal I/O errors; mismatched passwords prompt again.
pub fn prompt_password(prompt: &str, confirm_prompt: &str) -> Result<Option<String>> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    for i in 0..3 {
        print!("{}", prompt);
        let mut password = stdin.read_passwd(&mut stdout)?;
        if let Some(password) = password.as_mut() {
            *password = password.trim().to_string();
        }
        password.take_if(|s| s.is_empty());

        if password.is_none() {
            return Ok(None);
        }

        print!("\n{}", confirm_prompt);
        let confirm_password = stdin.read_passwd(&mut stdout)?;

        // Note: Actually comparing two Option<String> values
        if confirm_password == password {
            return Ok(password);
        } else if i < 2 {
            eprintln!("passwords do not match, please try again");
        }
    }
    bail!("passwords do not match, giving up");
}

/// Why the configured packages could not be installed.
#[derive(Debug, thiserror::Error)]
pub enum PackageInstallError {
    /// Packages are configured but no cookbook was given to take them from.
    #[error("packages are configured, but there is no cookbook to install them from")]
    NoRepository,
    /// A configured package is not in the cookbook's repository.
    #[error("package {name} is not in {}", repository.display())]
    NotInRepository {
        /// The package.
        name: fox_core::PackageName,
        /// The repository directory.
        repository: PathBuf,
    },
    /// Dependencies of the configured packages are not in the repository.
    #[error("missing dependencies: {}", missing_list(.0))]
    MissingDependencies(Vec<fox_store::deps::MissingDependency>),
}

fn missing_list(missing: &[fox_store::deps::MissingDependency]) -> String {
    missing
        .iter()
        .map(|entry| format!("{} (needed by {})", entry.dependency, entry.package))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The packages the configuration installs: every listed package except
/// those marked `"ignore"`.
pub(crate) fn package_names(config: &Config) -> anyhow::Result<Vec<fox_core::PackageName>> {
    Ok(config
        .packages
        .iter()
        .filter(|(_, package)| !matches!(package, PackageConfig::Build(rule) if rule == "ignore"))
        .map(|(name, _)| fox_core::PackageName::new(name.as_str()))
        .collect::<Result<Vec<_>, _>>()?)
}

/// Plans installing `names` and their dependencies into `store` from the
/// cookbook's repository (`<cookbook>/repo/<target>/`), accepting archives
/// signed by `keys`.
///
/// # Errors
/// [`PackageInstallError::NotInRepository`] if a package is not in the
/// repository, [`PackageInstallError::MissingDependencies`] if a
/// dependency is not, and the planner's errors otherwise.
pub(crate) fn plan_cookbook_packages(
    names: Vec<fox_core::PackageName>,
    cookbook: &Path,
    keys: fox_core::TrustedKeys,
    store: &fox_store::Store,
) -> anyhow::Result<fox_store::deps::Plan> {
    let repository = cookbook.join("repo").join(get_target());
    let mut source = LocalRepository::new(&repository, keys);
    let requested = names.into_iter().map(Request::Name).collect();
    let plan = match fox_store::deps::plan(store, requested, &mut source) {
        Err(PlanError::NotFound(name)) => {
            return Err(PackageInstallError::NotInRepository { name, repository }.into());
        }
        plan => plan?,
    };
    if !plan.missing.is_empty() {
        return Err(PackageInstallError::MissingDependencies(plan.missing).into());
    }
    Ok(plan)
}

/// Installs the configured packages and their dependencies from the
/// cookbook's repository (`<cookbook>/repo/<target>/`) with fox's store, so
/// the image's packages are recorded in fox's database (`var/lib/fox/`) as
/// they are placed. The cookbook's signing key becomes a key the image
/// trusts.
fn install_packages(config: &Config, dest: &Path, cookbook: Option<&str>) -> anyhow::Result<()> {
    let names = package_names(config)?;
    if names.is_empty() {
        return Ok(());
    }
    let cookbook = Path::new(cookbook.ok_or(PackageInstallError::NoRepository)?);

    let key = install_fox_key(cookbook, dest)?;
    let mut keys = fox_core::TrustedKeys::new();
    keys.insert(FOX_COOKBOOK_LABEL, key.key);
    let store = fox_store::Store::open(dest)?;
    let plan = plan_cookbook_packages(names, cookbook, keys, &store)?;
    // Without features.toml packages.signed_manifests, cookbook archives
    // carry only the unsigned metadata file the cookbook wrote beside them.
    let options = fox_store::InstallOptions {
        force: false,
        allow_unsigned_metadata: true,
    };
    for (archive, _) in &plan.install {
        let manifest = archive.manifest();
        println!("installing {} {}", manifest.name, manifest.version);
    }
    store.install_plan(&plan, options)?;
    Ok(())
}

/// Install configured files, packages and accounts, creating the output directory if needed.
///
/// # Errors
/// Returns package, filesystem, account or configuration errors, including
/// rejection of a symbolic-link output root.
///
/// # Panics
/// May panic when the random source fails during password salt generation.
pub fn install_dir(
    config: Config,
    output_dir: impl AsRef<Path>,
    cookbook: Option<&str>,
) -> Result<()> {
    install_dir_inner(config, output_dir.as_ref(), cookbook, &mut None)
}

/// Install an image root without changing host ownership.
///
/// Guest IDs are recorded for [`crate::ImageOwnership::archive`]. Files from
/// packages default to root ownership; file declarations and user homes retain
/// their configured guest IDs. This does not make a host directory suitable for
/// executing as the guest root: it is staging data for an image.
///
/// # Errors
/// Returns package, configuration, filesystem or account errors.
///
/// # Examples
/// ```no_run
/// # fn main() -> anyhow::Result<()> {
/// let config = redox_installer::Config::default();
/// let owners = redox_installer::install_dir_unprivileged(config, "build/staging", None)?;
/// // Pass `owners` and the staging directory to ImageOwnership::archive.
/// # let _ = owners;
/// # Ok(())
/// # }
/// ```
///
/// # Panics
/// May panic when the random source fails during password salt generation.
pub fn install_dir_unprivileged(
    config: Config,
    output_dir: impl AsRef<Path>,
    cookbook: Option<&str>,
) -> Result<crate::ImageOwnership> {
    let mut ownership = Some(crate::ImageOwnership::default());
    install_dir_inner(config, output_dir.as_ref(), cookbook, &mut ownership)?;
    Ok(ownership.unwrap_or_default())
}

fn install_dir_inner(
    config: Config,
    output_dir: &Path,
    cookbook: Option<&str>,
    ownership: &mut Option<crate::ImageOwnership>,
) -> Result<()> {
    // Check the root before creating it: create_dir_all follows existing links.
    match fs::symlink_metadata(output_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(crate::ImagePathError::Symlink(output_dir.to_path_buf()).into());
        }
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(output_dir)?;
        }
        Err(source) => return Err(source.into()),
    }
    let output_dir = crate::image_path(output_dir, Path::new(""))?;

    for file in &config.files {
        if !file.postinstall {
            file.create_with_ownership(&output_dir, ownership)?;
        }
    }

    install_packages(&config, &output_dir, cookbook)?;

    for file in &config.files {
        if file.postinstall {
            file.create_with_ownership(&output_dir, ownership)?;
        }
    }

    let mut passwd = String::new();
    let mut shadow = String::new();
    let mut next_uid = 1000;
    let mut next_gid = 1000;

    let mut groups = vec![];

    for (username, user) in config.users {
        // plaintext
        let password = if let Some(password) = user.password {
            password
        } else if config.general.prompt.unwrap_or(true) {
            prompt_password(
                &format!("{}: enter password: ", username),
                &format!("{}: confirm password: ", username),
            )?
            .unwrap_or_default()
        } else {
            String::new()
        };

        let uid = user.uid.unwrap_or(next_uid);

        if uid >= next_uid {
            next_uid = uid + 1;
        }

        let gid = user.gid.unwrap_or(next_gid);

        if gid >= next_gid {
            next_gid = gid + 1;
        }

        let name = user.name.unwrap_or(username.clone());
        let home = user.home.unwrap_or(format!("/home/{}", username));
        let shell = user.shell.unwrap_or("/bin/ion".into());

        println!("Adding user {username}:");
        if password.is_empty() {
            println!("\tPassword: unset");
        } else {
            println!("\tPassword: set");
        }
        println!("\tUID: {uid}");
        println!("\tGID: {gid}");
        println!("\tName: {name}");
        println!("\tHome: {home}");
        println!("\tShell: {shell}");

        FileConfig::new_directory(home.clone())
            .with_recursive_mod(0o700, uid, gid)
            .create_with_ownership(&output_dir, ownership)?;

        if uid >= 1000 {
            prepare_user_home(&output_dir, uid, gid, &home, ownership)?;
        }

        let password = hash_password(&password)?;

        passwd.push_str(&format!("{username};{uid};{gid};{name};{home};{shell}\n",));
        shadow.push_str(&format!("{username};{password}\n"));
        groups.push((username.clone(), gid, vec![username]));
    }

    for (group, group_config) in config.groups {
        // FIXME this assumes there is no overlap between auto-created groups for users
        // and explicitly specified groups.
        let gid = group_config.gid.unwrap_or(next_gid);

        if gid >= next_gid {
            next_gid = gid + 1;
        }

        groups.push((group, gid, group_config.members));
    }

    if !passwd.is_empty() {
        FileConfig::new_file("/etc/passwd".to_string(), passwd)
            .create_with_ownership(&output_dir, ownership)?;
    }

    if !shadow.is_empty() {
        FileConfig::new_file("/etc/shadow".to_string(), shadow)
            .with_mod(0o0600, 0, 0)
            .create_with_ownership(&output_dir, ownership)?;
    }

    if !groups.is_empty() {
        let mut groups_data = String::new();

        for (name, gid, members) in groups {
            use std::fmt::Write;
            writeln!(groups_data, "{name};x;{gid};{}", members.join(","))?;

            println!("Adding group {name}:");
            println!("\tGID: {gid}");
            println!("\tMembers: {}", members.join(", "));
        }

        FileConfig::new_file("/etc/group".to_string(), groups_data)
            .create_with_ownership(&output_dir, ownership)?;
    }

    // Ferric Oxide: fox adopts, syncs and describes the finished root, so
    // its database and SBOM cover everything the image holds.
    if let Some(fox) = &config.fox {
        fox.run(&output_dir)?;
    }

    Ok(())
}

fn prepare_user_home(
    output_dir: &PathBuf,
    uid: u32,
    gid: u32,
    home: &String,
    ownership: &mut Option<crate::ImageOwnership>,
) -> Result<(), anyhow::Error> {
    for xdg_folder in &[
        "Desktop",
        "Documents",
        "Downloads",
        "Music",
        "Pictures",
        "Public",
        "Templates",
        "Videos",
        ".config",
        ".local",
        ".local/share",
        ".local/share/Trash",
        ".local/share/Trash/info",
    ] {
        FileConfig::new_directory(format!("{}/{}", home, xdg_folder))
            .with_mod(0o0700, uid, gid)
            .create_with_ownership(output_dir, ownership)?;
    }
    FileConfig::new_file(
        format!("{}/.config/user-dirs.dirs", home),
        r#"# Produced by redox installer
XDG_DESKTOP_DIR="$HOME/Desktop"
XDG_DOCUMENTS_DIR="$HOME/Documents"
XDG_DOWNLOAD_DIR="$HOME/Downloads"
XDG_MUSIC_DIR="$HOME/Music"
XDG_PICTURES_DIR="$HOME/Pictures"
XDG_PUBLICSHARE_DIR="$HOME/Public"
XDG_TEMPLATES_DIR="$HOME/Templates"
XDG_VIDEOS_DIR="$HOME/Videos"
"#
        .to_string(),
    )
    .with_mod(0o0600, uid, gid)
    .create_with_ownership(output_dir, ownership)?;

    let skel_dir = output_dir.join("etc/skel");
    if skel_dir.is_dir() {
        copy_dir_all(&skel_dir, home.clone(), output_dir, uid, gid, ownership)?;
    }
    Ok(())
}

fn copy_dir_all(
    src: impl AsRef<Path>,
    dst: String,
    output_dir: &Path,
    uid: u32,
    gid: u32,
    ownership: &mut Option<crate::ImageOwnership>,
) -> anyhow::Result<()> {
    if !Path::new(dst.as_str()).is_dir() {
        FileConfig::new_directory(dst.clone())
            .with_mod(0o0700, uid, gid)
            .create_with_ownership(&output_dir, ownership)?;
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let dst_path = format!("{}/{}", dst, entry.file_name().display());
        if file_type.is_dir() {
            copy_dir_all(entry.path(), dst_path, output_dir, uid, gid, ownership)?;
        } else if file_type.is_file() {
            FileConfig::new_file(
                dst_path,
                fs::read_to_string(entry.path())
                    .with_context(|| format!("Reading {}", entry.path().display()))?,
            )
            .with_mod(0o0600, uid, gid)
            .create_with_ownership(&output_dir, ownership)?;
        } else if file_type.is_symlink() {
            // TODO
        }
    }
    Ok(())
}

/// Create a RedoxFS filesystem and pass ownership to a callback.
///
/// # Errors
/// Returns clock, filesystem creation or callback errors.
pub fn with_redoxfs<D, T, F>(disk: D, password_opt: Option<&[u8]>, callback: F) -> Result<T>
where
    D: Disk + Send + 'static,
    F: FnOnce(FileSystem<D>) -> Result<T>,
{
    let ctime = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let fs = FileSystem::create(disk, password_opt, ctime.as_secs(), ctime.subsec_nanos())
        .map_err(syscall_error)?;
    callback(fs)
}

fn decide_mount_path(mount_path: Option<&Path>) -> PathBuf {
    let mount_path = mount_path.map(|p| p.to_path_buf()).unwrap_or_else(|| {
        PathBuf::from(if cfg!(target_os = "redox") {
            format!("file.redox_installer_{}", process::id())
        } else {
            format!("/tmp/redox_installer_{}", process::id())
        })
    });
    mount_path
}

/// Mount a filesystem for a callback, then unmount and remove the mount point.
///
/// # Errors
/// Returns mount, callback, channel or filesystem cleanup errors.
///
/// # Panics
/// Panics if the mount path is not UTF-8 or the mount thread panics.
pub fn with_redoxfs_mount<D, T, F>(
    fs: FileSystem<D>,
    mount_path: Option<&Path>,
    callback: F,
) -> Result<T>
where
    D: Disk + Send + 'static,
    F: FnOnce(&Path) -> Result<T>,
{
    let mount_path = decide_mount_path(mount_path);

    if cfg!(not(target_os = "redox")) && !mount_path.exists() {
        fs::create_dir(&mount_path)?;
    }

    let (tx, rx) = channel();
    let join_handle = {
        let mount_path = mount_path.clone();
        thread::spawn(move || {
            let res = redoxfs::mount(fs, &mount_path, |real_path| {
                tx.send(Ok(real_path.to_owned())).unwrap();
            });
            match res {
                Ok(()) => (),
                Err(err) => {
                    tx.send(Err(err)).unwrap();
                }
            };
        })
    };

    let res = match rx.recv() {
        Ok(ok) => match ok {
            Ok(real_path) => callback(&real_path),
            Err(err) => return Err(err.into()),
        },
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "redoxfs thread did not send a result",
            )
            .into());
        }
    };

    unmount_path(&mount_path.as_os_str().to_str().unwrap())?;

    join_handle.join().unwrap();

    if cfg!(not(target_os = "redox")) {
        fs::remove_dir_all(&mount_path)?;
    }

    res
}

/// Run a callback in a directory and archive its successful output into RedoxFS.
///
/// # Errors
/// Returns callback, filesystem archive or cleanup errors.
pub fn with_redoxfs_ar<D, T, F>(
    mut fs: FileSystem<D>,
    mount_path: Option<&Path>,
    callback: F,
) -> Result<T>
where
    D: Disk + Send + 'static,
    F: FnOnce(&Path) -> Result<T>,
{
    let mount_path = decide_mount_path(mount_path);

    let res = callback(Path::new(&mount_path));

    if res.is_ok() {
        let _end_block = fs
            .tx(|tx| {
                // Archive_at root node
                redoxfs::archive_at(tx, Path::new(&mount_path), redoxfs::TreePtr::root())
                    .map_err(|err| syscall::Error::new(err.raw_os_error().unwrap()))?;

                // Squash alloc log
                tx.sync(true)?;

                let end_block = tx.header.size() / BLOCK_SIZE;
                /* TODO: Cut off any free blocks at the end of the filesystem
                let mut end_changed = true;
                while end_changed {
                    end_changed = false;

                    let allocator = fs.allocator();
                    let levels = allocator.levels();
                    for level in 0..levels.len() {
                        let level_size = 1 << level;
                        for &block in levels[level].iter() {
                            if block < end_block && block + level_size >= end_block {
                                end_block = block;
                                end_changed = true;
                            }
                        }
                    }
                }
                */

                // Update header
                tx.header.size = (end_block * BLOCK_SIZE).into();
                tx.header_changed = true;
                tx.sync(false)?;

                Ok(end_block)
            })
            .map_err(syscall_error)?;

        // let size = (fs.block + end_block) * BLOCK_SIZE;
        // fs.disk.file.set_len(size)?;
    }

    fs::remove_dir_all(&mount_path)?;

    res
}

fn with_redoxfs_ar_owned<D, F>(
    mut fs: FileSystem<D>,
    mount_path: Option<&Path>,
    callback: F,
) -> Result<()>
where
    D: Disk + Send + 'static,
    F: FnOnce(&Path) -> Result<crate::ImageOwnership>,
{
    let mount_path = decide_mount_path(mount_path);

    let res = callback(Path::new(&mount_path));

    if let Ok(ownership) = &res {
        ownership.archive(&mut fs, &mount_path)?;
    }
    fs::remove_dir_all(&mount_path)?;
    res.map(|_| ())
}

/// Fetch BIOS and UEFI bootloader bytes using the selected package configuration.
///
/// # Errors
/// Returns package installation or temporary-directory I/O errors.
pub fn fetch_bootloaders(
    config: &Config,
    cookbook: Option<&str>,
    live: bool,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let bootloader_dir =
        PathBuf::from(format!("/tmp/redox_installer_bootloader_{}", process::id()));

    if bootloader_dir.exists() {
        fs::remove_dir_all(&bootloader_dir)?;
    }

    fs::create_dir(&bootloader_dir)?;

    let mut bootloader_config = Config::bootloader_config();
    bootloader_config.general = config.general.clone();
    install_packages(&bootloader_config, &bootloader_dir, cookbook)?;

    let boot_dir = bootloader_dir.join("usr/lib/boot");
    let bios_path = boot_dir.join(if live {
        "bootloader-live.bios"
    } else {
        "bootloader.bios"
    });
    let efi_path = boot_dir.join(if live {
        "bootloader-live.efi"
    } else {
        "bootloader.efi"
    });

    let bios_data = if bios_path.exists() {
        fs::read(bios_path)?
    } else {
        Vec::new()
    };
    let efi_data = if efi_path.exists() {
        fs::read(efi_path)?
    } else {
        Vec::new()
    };

    fs::remove_dir_all(&bootloader_dir)?;

    Ok((bios_data, efi_data))
}

//TODO: make bootloaders use Option, dynamically create BIOS and EFI partitions
/// Partition and format a disk, then invoke a callback on its RedoxFS partition.
///
/// # Errors
/// Returns disk, partition-table, formatting, bootloader or callback errors.
///
/// # Panics
/// Panics if required partition entries are absent or the disk layout cannot fit.
pub fn with_whole_disk<P, F, T>(disk_path: P, disk_option: &DiskOption, callback: F) -> Result<T>
where
    P: AsRef<Path>,
    F: FnOnce(FileSystem<DiskIo<fscommon::StreamSlice<DiskWrapper>>>) -> Result<T>,
{
    let bootloader_efi_name = bootloader_efi_name()?;
    // Open disk and read metadata
    eprintln!(
        "Opening {} {}",
        if disk_option.skip_partitions {
            "partition"
        } else {
            "disk"
        },
        disk_path.as_ref().display()
    );
    let mut disk_file = DiskWrapper::open(disk_path.as_ref())?;
    let disk_size = disk_file.size();
    let block_size = disk_file.block_size() as u64;

    if let Some(efi_partition_path) = disk_option.efi_partition_path {
        let mut disk_efi = DiskWrapper::open(efi_partition_path)?;
        eprintln!("Writing bootloader partition to {:?}", efi_partition_path);
        format_and_install_efi(
            &mut disk_efi,
            bootloader_efi_name,
            disk_option.bootloader_efi,
        )?;
    }

    if disk_option.skip_partitions {
        return with_redoxfs(
            DiskIo(fscommon::StreamSlice::new(
                disk_file,
                0,
                disk_size.next_multiple_of(block_size),
            )?),
            disk_option.password_opt,
            callback,
        );
    }

    let gpt_block_size = match block_size {
        512 => gpt::disk::LogicalBlockSize::Lb512,
        _ => {
            // TODO: support (and test) other block sizes
            bail!("block size {block_size} not supported");
        }
    };

    // Calculate partition offsets
    let gpt_reserved = 34 * 512; // GPT always reserves 34 512-byte sectors
    let mibi = 1024 * 1024;

    // First megabyte of the disk is reserved for BIOS partition, wich includes GPT tables
    let bios_start = gpt_reserved / block_size;
    let bios_end = (mibi / block_size) - 1;

    // Second megabyte of the disk is reserved for EFI partition
    let efi_start = bios_end + 1;
    let efi_size = disk_option.efi_partition_size.unwrap_or(1) as u64;
    let efi_end = efi_start + (efi_size * mibi / block_size) - 1;

    // The rest of the disk is RedoxFS, reserving the GPT table mirror at the end of disk
    let redoxfs_start = efi_end + 1;
    let redoxfs_end = ((((disk_size - gpt_reserved) / mibi) * mibi) / block_size) - 1;

    // Format and install BIOS partition
    {
        // Write BIOS bootloader to disk
        eprintln!(
            "Write bootloader with size {:#x}",
            disk_option.bootloader_bios.len()
        );
        disk_file.seek(SeekFrom::Start(0))?;
        disk_file.write_all(&disk_option.bootloader_bios)?;

        // Replace MBR tables with protective MBR
        // TODO: div_ceil
        let mbr_blocks = ((disk_size + block_size - 1) / block_size) - 1;
        eprintln!("Writing protective MBR with disk blocks {mbr_blocks:#x}");
        gpt::mbr::ProtectiveMBR::with_lb_size(mbr_blocks as u32)
            .update_conservative(&mut disk_file)?;

        // Open disk, mark it as not initialized
        let mut gpt_disk = gpt::GptConfig::new()
            .initialized(false)
            .writable(true)
            .logical_block_size(gpt_block_size)
            .create_from_device(Box::new(&mut disk_file), None)?;

        // Add BIOS boot partition
        let mut partitions = BTreeMap::new();
        let mut partition_id = 1;
        partitions.insert(
            partition_id,
            gpt::partition::Partition {
                part_type_guid: gpt::partition_types::BIOS,
                part_guid: uuid::Uuid::new_v4(),
                first_lba: bios_start,
                last_lba: bios_end,
                flags: 0, // TODO
                name: "BIOS".to_string(),
            },
        );
        partition_id += 1;

        // Add EFI boot partition
        partitions.insert(
            partition_id,
            gpt::partition::Partition {
                part_type_guid: gpt::partition_types::EFI,
                part_guid: uuid::Uuid::new_v4(),
                first_lba: efi_start,
                last_lba: efi_end,
                flags: 0, // TODO
                name: "EFI".to_string(),
            },
        );
        partition_id += 1;

        // Add RedoxFS partition
        partitions.insert(
            partition_id,
            gpt::partition::Partition {
                //TODO: Use REDOX_REDOXFS type (needs GPT crate changes)
                part_type_guid: gpt::partition_types::LINUX_FS,
                part_guid: uuid::Uuid::new_v4(),
                first_lba: redoxfs_start,
                last_lba: redoxfs_end,
                flags: 0,
                name: "REDOX".to_string(),
            },
        );

        eprintln!("Writing GPT tables: {partitions:#?}");

        // Initialize GPT table
        gpt_disk.update_partitions(partitions)?;

        // Write partition layout, returning disk file
        gpt_disk.write()?;
    }

    // Format and install EFI partition
    {
        let disk_efi_start = efi_start * block_size;
        let disk_efi_end = (efi_end + 1) * block_size;
        let mut disk_efi =
            fscommon::StreamSlice::new(&mut disk_file, disk_efi_start, disk_efi_end)?;

        format_and_install_efi(
            &mut disk_efi,
            bootloader_efi_name,
            disk_option.bootloader_efi,
        )?;
    }

    // Format and install RedoxFS partition
    eprintln!(
        "Installing to RedoxFS partition with size {:#x}",
        (redoxfs_end - redoxfs_start) * block_size
    );
    let disk_redoxfs = DiskIo(fscommon::StreamSlice::new(
        disk_file,
        redoxfs_start * block_size,
        (redoxfs_end + 1) * block_size,
    )?);
    with_redoxfs(disk_redoxfs, disk_option.password_opt, callback)
}

/// Return the UEFI fallback bootloader filename for the installation target.
///
/// # Errors
/// Returns an error if the selected target has no supported UEFI filename.
pub fn bootloader_efi_name() -> Result<&'static str> {
    let target = get_target();
    let name = match target.as_str() {
        "aarch64-unknown-redox" => "BOOTAA64.EFI",
        "i586-unknown-redox" | "i686-unknown-redox" => "BOOTIA32.EFI",
        "x86_64-unknown-redox" => "BOOTX64.EFI",
        "riscv64gc-unknown-redox" => "BOOTRISCV64.EFI",
        _ => {
            bail!("target '{target}' not supported");
        }
    };
    Ok(name)
}

fn format_and_install_efi<T: fatfs::ReadWriteSeek>(
    disk_efi: &mut T,
    bootloader_name: &str,
    bootloader_data: &[u8],
) -> Result<()> {
    eprintln!("Formatting EFI partition...");
    fatfs::format_volume(&mut *disk_efi, fatfs::FormatVolumeOptions::new())?;

    eprintln!("Opening EFI partition");
    let fs = fatfs::FileSystem::new(disk_efi, fatfs::FsOptions::new())?;

    eprintln!("Creating EFI directory");
    let root_dir = fs.root_dir();
    root_dir.create_dir("EFI")?;

    eprintln!("Creating EFI/BOOT directory");
    let efi_dir = root_dir.open_dir("EFI")?;
    efi_dir.create_dir("BOOT")?;

    eprintln!(
        "Writing EFI/BOOT/{} file with size {:#x}",
        bootloader_name,
        bootloader_data.len()
    );
    let boot_dir = efi_dir.open_dir("BOOT")?;
    let mut file = boot_dir.create_file(bootloader_name)?;
    file.truncate()?;
    file.write_all(bootloader_data)?;

    Ok(())
}

#[cfg(not(target_os = "redox"))]
/// Attempt to copy a live filesystem, returning false when unavailable.
///
/// # Errors
/// Returns live-memory mapping, disk copy or filesystem update errors on Redox.
pub fn try_fast_install<D: redoxfs::Disk, F: FnMut(u64, u64)>(
    _fs: &mut redoxfs::FileSystem<D>,
    _progress: F,
) -> Result<bool> {
    Ok(false)
}

/// Try fast install using live disk memory
#[cfg(target_os = "redox")]
/// Attempt to copy a live filesystem, returning false when unavailable.
///
/// # Errors
/// Returns live-memory mapping, disk copy or filesystem update errors on Redox.
pub fn try_fast_install<D: redoxfs::Disk, F: FnMut(u64, u64)>(
    fs: &mut redoxfs::FileSystem<D>,
    mut progress: F,
) -> Result<bool> {
    use libredox::{call::MmapArgs, flag};
    use std::os::fd::AsRawFd;
    use syscall::PAGE_SIZE;

    let phys = env::var("DISK_LIVE_ADDR")
        .ok()
        .and_then(|x| usize::from_str_radix(&x, 16).ok())
        .unwrap_or(0);
    let size = env::var("DISK_LIVE_SIZE")
        .ok()
        .and_then(|x| usize::from_str_radix(&x, 16).ok())
        .unwrap_or(0);
    if phys == 0 || size == 0 {
        return Ok(false);
    }

    let start = (phys / PAGE_SIZE) * PAGE_SIZE;
    let end = phys
        .checked_add(size)
        .context("phys + size overflow")?
        .next_multiple_of(PAGE_SIZE);
    let size = end - start;

    let original = unsafe {
        //TODO: unmap this memory
        let file = fs::File::open("/scheme/memory/physical")?;
        let base = libredox::call::mmap(MmapArgs {
            fd: file.as_raw_fd() as usize,
            addr: core::ptr::null_mut(),
            offset: start as u64,
            length: size,
            prot: flag::PROT_READ,
            flags: flag::MAP_SHARED,
        })
        .map_err(|err| anyhow::anyhow!("failed to mmap livedisk: {}", err))?;

        std::slice::from_raw_parts(base as *const u8, size)
    };

    struct DiskLive {
        original: &'static [u8],
    }

    impl redoxfs::Disk for DiskLive {
        unsafe fn read_at(&mut self, block: u64, buffer: &mut [u8]) -> syscall::Result<usize> {
            let offset = (block * redoxfs::BLOCK_SIZE) as usize;
            if offset + buffer.len() > self.original.len() {
                return Err(syscall::Error::new(syscall::EINVAL));
            }
            buffer.copy_from_slice(&self.original[offset..offset + buffer.len()]);
            Ok(buffer.len())
        }

        unsafe fn write_at(&mut self, _block: u64, _buffer: &[u8]) -> syscall::Result<usize> {
            Err(syscall::Error::new(syscall::EINVAL))
        }

        fn size(&mut self) -> syscall::Result<u64> {
            Ok(self.original.len() as u64)
        }
    }

    let mut fs_old = redoxfs::FileSystem::open(DiskLive { original }, None, None, false)?;
    let size_old = fs_old.header.size();
    let free_old = fs_old.allocator().free() * redoxfs::BLOCK_SIZE;
    let used_old = size_old - free_old;
    redoxfs::clone(&mut fs_old, fs, move |used| {
        progress(used, used_old);
    })?;

    Ok(true)
}

fn install_inner(config: Config, output: &Path) -> Result<()> {
    println!("Installing to {}:\n{}", output.display(), config);
    let cookbook = config.general.cookbook.clone();
    let cookbook = cookbook.as_ref().map(|p| p.as_str());
    if output.is_dir() {
        install_dir(config, output, cookbook)
    } else {
        let size = crate::image_size(&config, cookbook)?;
        eprintln!(
            "Image needs {} MiB ({} MiB of packages); building it at {} MiB",
            size.required_mib,
            size.packages.div_ceil(1024 * 1024),
            size.mib
        );
        let fs_size = u64::from(size.mib);
        // arbitrary size approximately fit just for initfs
        if fs_size < 32 {
            bail!("Refusing to create image disk less than 32 MB");
        }
        let bytes = fs_size * 1024 * 1024;
        if !output.exists() {
            eprintln!(
                "Creating a new file to {} with size {} MB",
                output.display(),
                fs_size
            );
            let file = fs::File::create(output)?;
            file.set_len(bytes)?;
        } else if fs::metadata(output)?.is_file() {
            let file = fs::OpenOptions::new().write(true).open(output)?;
            if file.metadata()?.len() < bytes {
                eprintln!("Growing {} to {} MB", output.display(), fs_size);
                file.set_len(bytes)?;
            } else {
                eprintln!("Overwriting {}", output.display());
            }
        } else {
            // A device: its size is what it is.
            let disk_mib = DiskWrapper::open(output)?.size() / (1024 * 1024);
            if disk_mib < u64::from(size.required_mib) {
                bail!(
                    "{} holds {disk_mib} MiB, but the image needs {} MiB",
                    output.display(),
                    size.required_mib
                );
            }
            eprintln!("Overwriting {} ({disk_mib} MiB)", output.display());
        }
        let live = config.general.live_disk.unwrap_or(false);
        let password_opt = config.general.encrypt_disk.clone();
        let password_opt = password_opt.as_ref().map(|p| p.as_bytes());
        let (bootloader_bios, bootloader_efi) = fetch_bootloaders(&config, cookbook, live)?;
        let efi_partition_path = match config
            .general
            .write_bootloader
            .as_ref()
            .map(|w| (std::fs::metadata(w), w))
        {
            Some((Ok(meta), write_bootloader)) => {
                if !meta.is_dir() && !meta.is_file() {
                    Some(write_bootloader.clone())
                } else if meta.is_dir() {
                    let dir = Path::new(write_bootloader);
                    std::fs::create_dir_all(&dir)?;
                    std::fs::write(dir.join(bootloader_efi_name()?), &bootloader_efi)?;
                    None
                } else {
                    std::fs::write(write_bootloader, &bootloader_efi)?;
                    None
                }
            }
            Some((Err(_), write_bootloader)) => {
                std::fs::write(write_bootloader, &bootloader_efi)?;
                None
            }
            None => None,
        };
        let disk_option = DiskOption {
            bootloader_bios: &bootloader_bios,
            bootloader_efi: &bootloader_efi,
            password_opt: password_opt,
            efi_partition_size: config.general.efi_partition_size,
            efi_partition_path: efi_partition_path.as_ref().map(|s| s.as_str()),
            skip_partitions: config.general.skip_partitions.unwrap_or(false),
        };
        with_whole_disk(output, &disk_option, move |fs| {
            if config.general.no_mount.unwrap_or(false) {
                with_redoxfs_ar_owned(fs, None, move |mount_path| {
                    install_dir_unprivileged(config, mount_path, cookbook)
                })
            } else {
                with_redoxfs_mount(fs, None, move |mount_path| {
                    install_dir(config, mount_path, cookbook)
                })
            }
        })
    }
}

/// Install RedoxFS into a new disk file, or a sysroot directory.
/// This function assumes all interactive prompts resolved by the caller.
///
/// # Errors
/// Returns package, configuration, disk formatting or filesystem errors.
///
/// # Panics
/// May panic for invalid disk layouts or random-source failure during account setup.
pub fn install(config: Config, output: impl AsRef<Path>) -> Result<()> {
    install_inner(config, output.as_ref())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A cookbook checkout with a signing key and a repository holding
    /// `app` (which needs `lib`) and `lib`.
    pub(crate) fn cookbook() -> (tempfile::TempDir, fox_core::fixtures::FixtureKey) {
        use fox_core::fixtures::{FixtureEntry, FixtureKey, build_package};
        use fox_core::{Manifest, PackageName};

        let cookbook = tempfile::tempdir().unwrap();
        let build = cookbook.path().join("build");
        fs::create_dir_all(&build).unwrap();
        let key = FixtureKey::generate(&build, "id_ed25519").unwrap();
        let repo = cookbook.path().join("repo").join(get_target());
        fs::create_dir_all(&repo).unwrap();
        let lib = Manifest::new(PackageName::new("lib").unwrap(), "1.0");
        let lib_file = [FixtureEntry::file("usr/lib/liblib.so", b"lib", 0o644)];
        build_package(&repo, &key, Some(&lib), &lib_file, false).unwrap();
        let mut app = Manifest::new(PackageName::new("app").unwrap(), "2.0");
        app.depends = vec![lib.name.clone()];
        let app_file = [FixtureEntry::file("usr/bin/app", b"app", 0o755)];
        build_package(&repo, &key, Some(&app), &app_file, false).unwrap();
        (cookbook, key)
    }

    pub(crate) fn config(packages: &[&str]) -> Config {
        let mut config = Config::default();
        for name in packages {
            config
                .packages
                .insert((*name).to_string(), PackageConfig::default());
        }
        config
    }

    #[test]
    fn packages_and_dependencies_are_installed_with_fox() {
        let (cookbook, key) = cookbook();
        let dest = tempfile::tempdir().unwrap();
        let cookbook_path = cookbook.path().to_str().unwrap();
        install_packages(&config(&["app"]), dest.path(), Some(cookbook_path)).unwrap();

        assert!(dest.path().join("usr/bin/app").is_file());
        assert!(dest.path().join("usr/lib/liblib.so").is_file());
        let installed = fs::read(dest.path().join("etc/fox/keys/cookbook.toml")).unwrap();
        assert_eq!(installed, fs::read(&key.public_path).unwrap());
        let store = fox_store::Store::open(dest.path()).unwrap();
        let names: Vec<_> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|package| (package.name.to_string(), package.signer_label))
            .collect();
        assert_eq!(
            names,
            [
                (
                    "app".to_string(),
                    fox_core::keys::COOKBOOK_LABEL.to_string()
                ),
                (
                    "lib".to_string(),
                    fox_core::keys::COOKBOOK_LABEL.to_string()
                )
            ]
        );
    }

    #[test]
    fn ignored_and_absent_packages() {
        let (cookbook, _) = cookbook();
        let dest = tempfile::tempdir().unwrap();
        let cookbook_path = cookbook.path().to_str().unwrap();
        let mut ignored = config(&[]);
        ignored
            .packages
            .insert("app".into(), PackageConfig::Build("ignore".into()));
        install_packages(&ignored, dest.path(), None).unwrap();
        assert!(!dest.path().join("usr/bin/app").exists());

        let error = install_packages(&config(&["app"]), dest.path(), None).unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(PackageInstallError::NoRepository)
        ));
        let error =
            install_packages(&config(&["absent"]), dest.path(), Some(cookbook_path)).unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(PackageInstallError::NotInRepository { .. })
        ));
    }

    #[test]
    fn missing_dependencies_fail_the_install() {
        let (cookbook, _) = cookbook();
        let repo = cookbook.path().join("repo").join(get_target());
        fs::remove_file(repo.join("lib.pkgar")).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let error = install_packages(
            &config(&["app"]),
            dest.path(),
            Some(cookbook.path().to_str().unwrap()),
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref(),
            Some(PackageInstallError::MissingDependencies(missing)) if missing.len() == 1
        ));
        assert!(!dest.path().join("usr/bin/app").exists());
    }
}
