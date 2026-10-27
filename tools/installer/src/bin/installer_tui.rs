use anyhow::{Result, anyhow, bail};
use redox_installer::{Config, DiskOption, try_fast_install, with_redoxfs_mount, with_whole_disk};
use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, symlink},
    path::{Path, PathBuf},
    process,
};

// TODO: This is not the TUI a regular user would expect it does
// 1. Linux: Implement disk listing, use "dd" to write into whole disk
// 2. Allow partitioning to allow dual boot, possibly an integration with systemd-boot/grub
// 3. Prompt everything (disk password, users, preconfigured packages, import from existing img)

#[cfg(not(target_os = "redox"))]
fn disk_paths(_paths: &mut Vec<(PathBuf, u64)>) {}

#[cfg(target_os = "redox")]
fn disk_paths(paths: &mut Vec<(PathBuf, u64)>) {
    let mut schemes = Vec::new();
    match fs::read_dir("/scheme") {
        Ok(entries) => {
            for entry_res in entries {
                if let Ok(entry) = entry_res {
                    if let Ok(file_name) = entry.file_name().into_string() {
                        if file_name.starts_with("disk") {
                            schemes.push(entry.path());
                        }
                    }
                }
            }
        }
        Err(err) => {
            eprintln!("redox_installer_tui: failed to list schemes: {}", err);
        }
    }

    for scheme in schemes {
        if scheme.is_dir() {
            match fs::read_dir(&scheme) {
                Ok(entries) => {
                    for entry_res in entries {
                        if let Ok(entry) = entry_res {
                            if let Ok(file_name) = entry.file_name().into_string() {
                                if file_name.contains('p') {
                                    // Skip partitions
                                    continue;
                                }

                                if let Ok(metadata) = entry.metadata() {
                                    let size = metadata.len();
                                    if size > 0 {
                                        paths.push((entry.path(), size));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    eprintln!(
                        "redox_installer_tui: failed to list '{}': {}",
                        scheme.display(),
                        err
                    );
                }
            }
        }
    }
}

fn copy_file(src: &Path, dest: &Path, buf: &mut [u8]) -> Result<()> {
    if let Some(parent) = dest.parent() {
        // Parent may be a symlink
        if !parent.is_symlink() {
            match fs::create_dir_all(&parent) {
                Ok(()) => (),
                Err(err) => {
                    bail!("failed to create directory {}: {}", parent.display(), err);
                }
            }
        }
    }

    let metadata = match fs::symlink_metadata(&src) {
        Ok(ok) => ok,
        Err(err) => {
            bail!("failed to read metadata of {}: {}", src.display(), err);
        }
    };

    if metadata.file_type().is_symlink() {
        let real_src = match fs::read_link(&src) {
            Ok(ok) => ok,
            Err(err) => {
                bail!("failed to read link {}: {}", src.display(), err);
            }
        };

        match symlink(&real_src, &dest) {
            Ok(()) => (),
            Err(err) => {
                bail!(
                    "failed to copy link {} ({}) to {}: {}",
                    src.display(),
                    real_src.display(),
                    dest.display(),
                    err
                );
            }
        }
    } else {
        let mut src_file = match fs::File::open(&src) {
            Ok(ok) => ok,
            Err(err) => {
                bail!("failed to open file {}: {}", src.display(), err);
            }
        };

        let mut dest_file = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(metadata.mode())
            .open(&dest)
        {
            Ok(ok) => ok,
            Err(err) => {
                bail!("failed to create file {}: {}", dest.display(), err);
            }
        };

        loop {
            let count = match src_file.read(buf) {
                Ok(ok) => ok,
                Err(err) => {
                    bail!("failed to read file {}: {}", src.display(), err);
                }
            };

            if count == 0 {
                break;
            }

            match dest_file.write_all(&buf[..count]) {
                Ok(()) => (),
                Err(err) => {
                    bail!("failed to write file {}: {}", dest.display(), err);
                }
            }
        }
    }

    Ok(())
}

/// Lists every file of the packages installed on the running system, from
/// fox's database, and fox's own state, so the copy keeps them installed.
fn package_files(
    root_path: &Path,
    config: &mut Config,
    files: &mut Vec<String>,
) -> Result<(), anyhow::Error> {
    //TODO: Remove packages from config where all files are located (and have valid shasum?)
    config.packages.clear();

    let store = fox_store::Store::open(root_path)?;
    for package in store.list()? {
        files.extend(package.files.into_iter().map(|file| file.path));
    }
    for dir in [fox_store::STATE_DIR, fox_core::keys::TRUSTED_KEYS_DIR] {
        state_files(root_path, &root_path.join(dir), files)?;
    }
    // fox's configuration is reserved rather than owned by a package.
    for file in [
        fox_core::registries::REGISTRIES_FILE,
        fox_core::policy::POLICY_FILE,
        fox_core::trust::TRUST_FILE,
    ] {
        if root_path.join(file).is_file() {
            files.push(file.to_string());
        }
    }
    Ok(())
}

/// Adds the regular files under `dir`, relative to `root_path`, skipping
/// fox's registry checkouts and source caches, which are rebuilt on demand.
fn state_files(root_path: &Path, dir: &Path, files: &mut Vec<String>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let relative = path
            .strip_prefix(root_path)?
            .to_str()
            .ok_or_else(|| anyhow!("{} is not UTF-8", path.display()))?
            .to_string();
        if path.is_symlink() || path.is_file() {
            files.push(relative);
        } else if path.is_dir()
            && !matches!(
                path.file_name().and_then(OsStr::to_str),
                Some("registries" | "git")
            )
        {
            state_files(root_path, &path, files)?;
        }
    }
    Ok(())
}

fn choose_disk() -> PathBuf {
    let mut paths = Vec::new();
    disk_paths(&mut paths);
    loop {
        for (i, (path, size)) in paths.iter().enumerate() {
            eprintln!(
                "\x1B[1m{}\x1B[0m: {}: {}",
                i + 1,
                path.display(),
                redox_installer::format_bytes(*size)
            );
        }

        if paths.is_empty() {
            eprintln!("redox_installer_tui: no RedoxFS partition found");
            eprintln!(
                "redox_installer_tui: this tool is used to overwrite unmounted RedoxFS disk in Redox OS"
            );
            process::exit(1);
        } else {
            eprint!("Select a drive from 1 to {}: ", paths.len());

            let mut line = String::new();
            match io::stdin().read_line(&mut line) {
                Ok(0) => {
                    eprintln!("redox_installer_tui: failed to read line: end of input");
                    process::exit(1);
                }
                Ok(_) => (),
                Err(err) => {
                    eprintln!("redox_installer_tui: failed to read line: {}", err);
                    process::exit(1);
                }
            }

            match line.trim().parse::<usize>() {
                Ok(i) => {
                    if i >= 1 && i <= paths.len() {
                        break paths[i - 1].0.clone();
                    } else {
                        eprintln!("{} not from 1 to {}", i, paths.len());
                    }
                }
                Err(err) => {
                    eprintln!("invalid input: {}", err);
                }
            }
        }
    }
}

fn main() {
    let root_path = Path::new("/");

    let disk_path = choose_disk();

    let Ok(password_opt) = redox_installer::prompt_password(
        "redox_installer_tui: redoxfs password (empty for none)",
        "redox_installer_tui: confirm password",
    ) else {
        process::exit(1);
    };

    let instant = std::time::Instant::now();

    let bootloader_bios = {
        let path = root_path.join("usr/lib/boot/bootloader.bios");
        if path.exists() {
            match fs::read(&path) {
                Ok(ok) => ok,
                Err(err) => {
                    eprintln!(
                        "redox_installer_tui: {}: failed to read: {}",
                        path.display(),
                        err
                    );
                    process::exit(1);
                }
            }
        } else {
            Vec::new()
        }
    };

    let bootloader_efi = {
        let path = root_path.join("usr/lib/boot/bootloader.efi");
        if path.exists() {
            match fs::read(&path) {
                Ok(ok) => ok,
                Err(err) => {
                    eprintln!(
                        "redox_installer_tui: {}: failed to read: {}",
                        path.display(),
                        err
                    );
                    process::exit(1);
                }
            }
        } else {
            Vec::new()
        }
    };

    let disk_option = DiskOption {
        bootloader_bios: &bootloader_bios,
        bootloader_efi: &bootloader_efi,
        password_opt: password_opt.as_ref().map(|x| x.as_bytes()),
        efi_partition_size: None,
        efi_partition_path: None,
        skip_partitions: false, // TODO?
    };
    let res = with_whole_disk(&disk_path, &disk_option, |mut fs| {
        // Fast install method via filesystem clone
        let mut last_percent = 0;
        if try_fast_install(&mut fs, move |used, used_old| {
            let percent = (used * 100) / used_old;
            if percent != last_percent {
                eprint!(
                    "\r{}%: {} MB/{} MB",
                    percent,
                    used / 1000 / 1000,
                    used_old / 1000 / 1000
                );
                last_percent = percent;
            }
        })? {
            eprintln!("\rfinished installing using fast mode");
            return Ok(());
        }

        // Slow install method via file copy
        with_redoxfs_mount(fs, None, |mount_path| {
            let mut config: Config = Config::from_file(&root_path.join("filesystem.toml"))?;

            // Copy filesystem.toml, which is not packaged
            let mut files = vec!["filesystem.toml".to_string()];

            // Copy files from locally installed packages
            package_files(&root_path, &mut config, &mut files)
                // TODO: implement Error trait
                .map_err(|err| anyhow!("failed to read package files: {err}"))?;

            // Perform config install (after packages have been converted to files)
            eprintln!("configuring system");
            let cookbook: Option<&'static str> = None;
            redox_installer::install_dir(config, mount_path, cookbook)
                .map_err(|err| io::Error::other(err))?;

            // Sort and remove duplicates
            files.sort();
            files.dedup();

            // Install files
            let mut buf = vec![0; 4096 * 1024];
            for (i, name) in files.iter().enumerate() {
                eprintln!("copy {} [{}/{}]", name, i, files.len());

                let src = root_path.join(name);
                let dest = mount_path.join(name);
                copy_file(&src, &dest, &mut buf)?;
            }

            eprintln!("finished installing, unmounting filesystem");

            Ok(())
        })
    });

    match res {
        Ok(()) => {
            eprintln!(
                "redox_installer_tui: installed successfully in {:?}",
                instant.elapsed()
            );
            process::exit(0);
        }
        Err(err) => {
            eprintln!("redox_installer_tui: failed to install: {:?}", err);
            process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_files_keep_fox_state_and_configuration() {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path();
        let kept = [
            fox_core::registries::REGISTRIES_FILE,
            fox_core::policy::POLICY_FILE,
            fox_core::trust::TRUST_FILE,
            "etc/fox/keys/cookbook.pub.toml",
        ];
        for file in kept
            .into_iter()
            .chain(["var/lib/fox/registries/main/index.toml"])
        {
            let path = root_path.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "").unwrap();
        }
        fox_store::Store::open(root_path).unwrap();

        let mut files = Vec::new();
        package_files(root_path, &mut Config::default(), &mut files).unwrap();
        for file in kept {
            assert!(files.iter().any(|f| f == file), "{file} is copied");
        }
        assert!(
            !files
                .iter()
                .any(|f| f.starts_with("var/lib/fox/registries")),
            "registry checkouts are rebuilt, not copied"
        );
    }
}
