use anyhow::{anyhow, bail, Context};
use redoxfs::{BlockAddr, BlockMeta, DiskFile, FileSystem, Node, TreePtr};
use std::fs;
use std::fs::File;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
#[cfg(feature = "fuse")]
use std::sync::mpsc::{channel, TryRecvError};
#[cfg(feature = "fuse")]
use std::{thread, time};

use crate::status_error;

/// An image mounted through RedoxFS's FUSE support.
///
/// Only the mounting path needs this; the archive path installs through the
/// Rust writer instead, so it is absent without the `fuse` feature.
#[cfg(feature = "fuse")]
pub struct RedoxFs {
    image: PathBuf,
    dir: PathBuf,
}

/// A RedoxFS error as the `io::Error` the rest of this module reports.
///
/// RedoxFS speaks `syscall::Error` in its transaction closures; the image
/// writer and the mounting path both need the same conversion, so it is here
/// once rather than in each caller.
pub(crate) fn syscall_error(err: syscall::Error) -> io::Error {
    io::Error::from_raw_os_error(err.errno)
}

#[cfg(feature = "fuse")]
impl RedoxFs {
    pub fn new<P: AsRef<Path>, Q: AsRef<Path>>(image: P, dir: Q) -> io::Result<Self> {
        let image = image.as_ref().to_owned();
        let dir = fs::canonicalize(dir)?;
        let mut s = Self { image, dir };
        s.mount()?;
        Ok(s)
    }

    pub fn mount(&mut self) -> io::Result<()> {
        if self.mounted()? {
            return Err(io::Error::other("directory was already mounted"));
        }

        let (tx, rx) = channel();

        let disk = DiskFile::open(&self.image).map_err(syscall_error)?;
        let fs = FileSystem::open(disk, None, None, true).map_err(syscall_error)?;
        let dir = self.dir.clone();
        thread::spawn(move || {
            let _ = tx.send(redoxfs::mount(fs, dir, |_| {}));
        });

        while !self.mounted()? {
            match rx.try_recv() {
                Ok(res) => match res {
                    Ok(()) => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "redoxfs thread exited early",
                        ))
                    }
                    Err(err) => return Err(err),
                },
                Err(err) => match err {
                    TryRecvError::Empty => (),
                    TryRecvError::Disconnected => {
                        return Err(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "redoxfs thread did not send a result",
                        ))
                    }
                },
            }
            thread::sleep(time::Duration::from_millis(1));
        }

        Ok(())
    }

    pub fn unmount(&mut self) -> io::Result<()> {
        if self.mounted()? {
            Command::new("fusermount")
                .arg("-u")
                .arg(&self.dir)
                .status()
                .and_then(status_error)?;

            if self.mounted()? {
                return Err(io::Error::other("directory was still mounted"));
            }
        }

        Ok(())
    }

    pub fn mounted(&self) -> io::Result<bool> {
        use proc_mounts::MountIter;

        for mount_res in MountIter::new()? {
            let mount = mount_res?;
            if mount.dest == self.dir {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

#[cfg(feature = "fuse")]
impl Drop for RedoxFs {
    fn drop(&mut self) {
        if let Err(err) = self.unmount() {
            panic!(
                "RedoxFs::drop: failed to unmount '{}': {}",
                self.dir.display(),
                err
            );
        }
    }
}

#[cfg(feature = "fuse")]
pub fn run_install_mount(
    config: redox_installer::Config,
    bootloader_bin: &Path,
    bootloader_uefi: bool,
    base_size: u64,
    base_tar: &Path,
    base_dir: &Path,
    base_bin: &PathBuf,
) -> Result<(), anyhow::Error> {
    let (bootloader_bios, bootloader_efi) =
        read_bootloader(base_bin, bootloader_bin, bootloader_uefi, base_size)?;
    let disk_option = redox_installer::DiskOption {
        bootloader_bios: &bootloader_bios,
        bootloader_efi: &bootloader_efi,
        password_opt: None,
        efi_partition_size: None,
        efi_partition_path: None,
        skip_partitions: false,
    };
    redox_installer::with_whole_disk(base_bin, &disk_option, move |fs| {
        redox_installer::with_redoxfs_mount(fs, Some(base_dir), move |base_dir| {
            if base_tar.exists() {
                // redoxer in docker was built without FUSE, then CI has FUSE
                eprintln!("redoxer: extracting archive");
                extract_tar(base_tar, base_dir)?;
            } else {
                redox_installer::install(config, base_dir)
                    .map_err(|err| io::Error::other(format!("{err}")))?;
            }

            Ok(())
        })
    })
    .context("Unable to create image from redoxfs-mount")?;
    Ok(())
}

pub fn archive_image(
    disk_path: &Path,
    folder_path: &Path,
    bootloader_bin: &Path,
    use_uefi: bool,
    free_space: u64,
) -> anyhow::Result<()> {
    let (bootloader_bios, bootloader_efi) =
        read_bootloader(disk_path, bootloader_bin, use_uefi, free_space)?;
    let disk_option = redox_installer::DiskOption {
        bootloader_bios: &bootloader_bios,
        bootloader_efi: &bootloader_efi,
        password_opt: None,
        efi_partition_size: None,
        efi_partition_path: None,
        skip_partitions: false,
    };
    redox_installer::with_whole_disk(disk_path, &disk_option, move |fs| {
        redox_installer::with_redoxfs_ar(fs, Some(folder_path), move |_| Ok(()))
    })
    .context("Unable to create image from redoxfs-ar")?;

    Ok(())
}

/// A partition of a whole-disk image, in 512-byte sectors.
///
/// Redoxer writes whole-disk images through [`archive_image`] and runs whole
/// disks under QEMU, so an installed image already has the layout
/// `redox_installer::with_whole_disk` produces: a GPT with a BIOS partition, an
/// EFI system partition, and the RedoxFS partition that holds everything else.
/// Rewriting only the file system means finding that last partition without
/// touching the rest of the disk.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Partition {
    /// First sector of the partition, counted from the start of the disk.
    pub(crate) first: u64,
    /// Last sector of the partition, inclusive.
    pub(crate) last: u64,
}

/// Finds the RedoxFS partition of the whole-disk image at `image`.
///
/// # Errors
/// Returns an error when the image has no valid GPT, none of its partitions is
/// of the type the installer writes for RedoxFS, or it has more than one.
pub(crate) fn redoxfs_partition(image: &Path) -> anyhow::Result<Partition> {
    let disk = fs::OpenOptions::new().read(true).open(image)?;
    // `initialized(true)` says the disk already has a partition table, which an
    // installed image does; a blank disk would otherwise be formatted.
    let gpt = gpt::GptConfig::new()
        .initialized(true)
        .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
        .open_from_device(Box::new(disk))
        .context("unable to read the image's partition table")?;

    let mut found: Vec<Partition> = gpt
        .partitions()
        .values()
        .filter(|partition| partition.part_type_guid == gpt::partition_types::LINUX_FS)
        .map(|partition| Partition {
            first: partition.first_lba,
            last: partition.last_lba,
        })
        .collect();
    // The installer writes exactly one, and a second would make "the RedoxFS
    // partition" ambiguous rather than something to choose between.
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => {
            anyhow::bail!("{image:?} has no RedoxFS partition; it is not an installed Redox image")
        }
        count => anyhow::bail!("{image:?} has {count} RedoxFS partitions, expected one"),
    }
}

/// Opens the RedoxFS file system of the whole-disk image at `image`.
///
/// The partition table, both bootloaders and every byte outside the RedoxFS
/// partition stay as they are, so rewriting this only changes the file system:
/// the image keeps the kernel and boot configuration it was installed with.
///
/// # Errors
/// Returns an error when the partition cannot be found, or the image cannot be
/// opened or holds no valid RedoxFS file system.
pub(crate) fn open_partition(
    image: &Path,
) -> anyhow::Result<FileSystem<redoxfs::DiskIo<fscommon::StreamSlice<File>>>> {
    let partition = redoxfs_partition(image)?;
    let file = fs::OpenOptions::new().read(true).write(true).open(image)?;
    let block_size = 512;
    let start = partition.first * block_size;
    let end = (partition.last + 1) * block_size;
    let disk_size = file.metadata()?.len();
    if end > disk_size {
        anyhow::bail!("the RedoxFS partition ends at {end} bytes, past the image's {disk_size}");
    }
    let disk = redoxfs::DiskIo(fscommon::StreamSlice::new(file, start, end)?);
    let fs = FileSystem::open(disk, None, None, true)
        .map_err(|err| anyhow!("unable to open the image's file system: {err}"))?;
    Ok(fs)
}

/// Writes `files` into the file system of the whole-disk image at `image`.
///
/// Each entry is a guest path, its contents and its permission bits. Only the
/// given paths are touched: everything the image was installed with — the
/// kernel, the boot configuration, the packages and their signed records —
/// stays exactly as it is, which is the whole point of running in a prepared
/// environment. Reading the image out and archiving a staging copy back would
/// rewrite all of it, and the RedoxFS writer cannot read an image out in the
/// first place (that is what the FUSE mount is for).
///
/// Missing directories on the way to a file are created, and a file that
/// already exists keeps its node and gets the new contents.
///
/// # Errors
/// Returns an error when the partition or its file system cannot be opened, a
/// path is not inside the image or is not valid UTF-8, or a node cannot be
/// created or written.
pub(crate) fn write_files(image: &Path, files: &[(PathBuf, Vec<u8>, u16)]) -> anyhow::Result<()> {
    let mut fs = open_partition(image)?;
    // The transaction closure can only report a `syscall::Error`, which carries
    // an errno and no message, so the path that was refused is kept here and
    // turned into the returned error below.
    let mut invalid_path: Option<String> = None;
    fs.tx(|tx| {
        for (path, data, mode) in files {
            // A path that leaves the image is a mistake in the caller, not an
            // I/O condition: remember it so the error below says so.
            let relative = redox_installer::image_relative_path(path).map_err(|err| {
                invalid_path = Some(format!("{}: {err}", path.display()));
                syscall::Error::new(syscall::EINVAL)
            })?;
            let components: Vec<&str> = relative
                .iter()
                .map(|component| {
                    component
                        .to_str()
                        .ok_or_else(|| syscall::Error::new(syscall::EINVAL))
                })
                .collect::<Result<_, _>>()?;
            let Some((name, dirs)) = components.split_last() else {
                continue;
            };
            let mut parent = TreePtr::root();
            for dir in dirs {
                parent = match tx.find_node(parent, dir) {
                    Ok(node) => node.ptr(),
                    Err(_) => tx
                        .create_node(parent, dir, Node::MODE_DIR | 0o755, 0, 0)
                        .map(|node| node.ptr())?,
                };
            }
            let node = match tx.find_node(parent, name) {
                Ok(node) => node.ptr(),
                Err(_) => tx
                    .create_node(parent, name, Node::MODE_FILE | (mode & 0o777), 0, 0)
                    .map(|node| node.ptr())?,
            };
            tx.write_node(node, 0, data, 0, 0)?;
        }
        tx.sync(true)?;
        Ok(())
    })
    .map_err(|err| match &invalid_path {
        Some(path) => anyhow::anyhow!("{path} is outside the guest image"),
        None => anyhow::Error::from(syscall_error(err)),
    })?;
    Ok(())
}

fn read_bootloader(
    disk_path: &Path,
    bootloader_bin: &Path,
    use_uefi: bool,
    free_space: u64,
) -> Result<(Vec<u8>, Vec<u8>), anyhow::Error> {
    {
        let file = File::create(disk_path)?;
        file.set_len(free_space)?;
    }
    let bootloader_bios = if use_uefi {
        Vec::new()
    } else {
        fs::read(bootloader_bin)?.to_vec()
    };
    let bootloader_efi = if use_uefi {
        fs::read(bootloader_bin)?.to_vec()
    } else {
        Vec::new()
    };
    Ok((bootloader_bios, bootloader_efi))
}

pub(crate) fn extract_tar(tar_file: &Path, dest_dir: &Path) -> anyhow::Result<()> {
    Command::new("tar")
        .arg("-x")
        .arg("-p")
        .arg("--same-owner")
        .arg("-f")
        .arg(tar_file)
        .arg("-C")
        .arg(dest_dir)
        .arg(".")
        .status()
        .and_then(status_error)
        .context("tar extract failed")?;
    Ok(())
}

#[cfg(feature = "fuse")]
pub(crate) fn shrink_disk(disk_path: &Path) -> anyhow::Result<()> {
    let shrink_size = {
        let mut fs = open_fs(disk_path)?;
        let (old_size, new_size) = resize(&mut fs, true).map_err(|e| anyhow!(e))?;
        if old_size > new_size {
            Some(old_size - new_size)
        } else {
            None
        }
    };

    if let Some(shrink_size) = shrink_size {
        let f = open_disk(disk_path)?.file;
        let size = f
            .metadata()
            .context("Unable to get disk file metadata")?
            .size();
        f.set_len(size - shrink_size)
            .context("Unable to shrink disk file metadata")?;
    }

    Ok(())
}

pub(crate) fn expand_disk(disk_path: &Path, desired_size: u64) -> anyhow::Result<()> {
    {
        let disk = open_disk(disk_path)?;
        let size = disk
            .file
            .metadata()
            .context("Unable to get disk file metadata")?
            .size();
        if size < desired_size {
            disk.file
                .set_len(desired_size)
                .context("Unable to expand disk file metadata")?;
        } else {
            return Ok(());
        }
    }

    {
        let mut fs = open_fs(disk_path)?;
        resize(&mut fs, false).map_err(|e| anyhow!(e))?;
    }

    Ok(())
}

fn open_fs(disk_path: &Path) -> anyhow::Result<FileSystem<DiskFile>> {
    let disk = open_disk(disk_path)?;
    let fs = match FileSystem::open(disk, None, None, true) {
        Ok(fs) => fs,
        Err(err) => {
            bail!(
                "redoxfs-resize: failed to open filesystem on {}: {}",
                disk_path.display(),
                err
            );
        }
    };
    Ok(fs)
}

fn open_disk(disk_path: &Path) -> anyhow::Result<DiskFile> {
    let disk = match DiskFile::open(disk_path) {
        Ok(disk) => disk,
        Err(err) => {
            bail!(
                "redoxfs-resize: failed to open disk image {}: {}",
                disk_path.display(),
                err
            );
        }
    };
    Ok(disk)
}

// copied from redoxfs-resize
fn resize<D: redoxfs::Disk>(fs: &mut FileSystem<D>, shrink: bool) -> Result<(u64, u64), String> {
    let disk_size = fs
        .disk
        .size()
        .map_err(|err| format!("failed to read disk size: {err}"))?;

    // Find contiguous free region
    //TODO: better error management
    let mut last_free = None;
    let mut last_end = 0;
    fs.tx(|tx| {
        let mut alloc_ptr = tx.header.alloc;
        while !alloc_ptr.is_null() {
            let alloc = tx.read_block(alloc_ptr)?;
            alloc_ptr = alloc.data().prev;
            for entry in alloc.data().entries.iter() {
                let count = entry.count();
                if count <= 0 {
                    continue;
                }
                let end = entry.index() + count as u64;
                if end > last_end {
                    last_free = Some(*entry);
                    last_end = end;
                }
            }
        }
        Ok(())
    })
    .map_err(|err| format!("failed to read alloc log: {err}"))?;

    let old_size = fs.header.size();
    let min_size = if let Some(entry) = last_free {
        entry.index() * redoxfs::BLOCK_SIZE
    } else {
        old_size
    };
    // Leave the last block out. The bootloader reads the filesystem into memory
    // a chunk at a time from the partition's first block, and a chunk that ends
    // exactly at the end of the disk makes the final read run past it, which the
    // firmware refuses ("invalid parameter"). One block of slack keeps every
    // read inside the partition.
    let max_size = disk_size - (fs.block * redoxfs::BLOCK_SIZE) - redoxfs::BLOCK_SIZE;

    let new_size = if shrink { min_size } else { max_size };

    let old_blocks = old_size / redoxfs::BLOCK_SIZE;
    let new_blocks = new_size / redoxfs::BLOCK_SIZE;
    let (start, end, shrink) = if new_size == old_size {
        return Ok((old_size, new_size));
    } else if new_size < old_size {
        (new_blocks, old_blocks, true)
    } else {
        (old_blocks, new_blocks, false)
    };

    // Allocate or deallocate blocks as needed
    unsafe {
        let allocator = fs.allocator_mut();
        for index in start..end {
            if shrink {
                //TODO: replace assert with error?
                let addr = BlockAddr::new(index, BlockMeta::default());
                assert_eq!(allocator.allocate_exact(addr), Some(addr));
            } else {
                let addr = BlockAddr::new(index, BlockMeta::default());
                allocator.deallocate(addr);
            }
        }
    }

    fs.tx(|tx| {
        // Update header
        tx.header.size = new_size.into();
        tx.header_changed = true;

        // Sync with squash
        tx.sync(true)?;

        Ok(())
    })
    .map_err(|err| format!("transaction failed: {err}"))?;

    Ok((old_size, new_size))
}

/// A filesystem that does nothing, used only to check that a mount succeeds.
#[cfg(feature = "fuse")]
pub(crate) struct DummyFS;

#[cfg(feature = "fuse")]
impl fuser::Filesystem for DummyFS {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a whole-disk image with one RedoxFS partition holding `files`,
    /// laid out the way `redox_installer::with_whole_disk` lays one out.
    fn whole_disk_with(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        use redox_installer::DiskOption;

        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("disk.img");
        let disk_option = DiskOption {
            // No bootloader: this test is about the file system, and reading
            // one would need a packages repository.
            bootloader_bios: &[],
            bootloader_efi: &[],
            password_opt: None,
            efi_partition_size: None,
            efi_partition_path: None,
            skip_partitions: false,
        };
        let staging = temp.path().join("staging");
        for (path, data) in files {
            let full = staging.join(path.trim_start_matches('/'));
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, data).unwrap();
        }
        // `with_whole_disk` partitions whatever size the file already has, so
        // give it room for the partition table and the files.
        fs::File::create(&image)
            .unwrap()
            .set_len(64 * 1024 * 1024)
            .unwrap();
        redox_installer::with_whole_disk(&image, &disk_option, |fs| {
            redox_installer::with_redoxfs_ar(fs, Some(&staging), |_| Ok(()))
        })
        .unwrap();
        (temp, image)
    }

    /// Reads a guest path out of the image, the way a booted guest would.
    fn read_guest_file(image: &Path, path: &str) -> Option<Vec<u8>> {
        let mut fs = open_partition(image).ok()?;
        let relative = redox_installer::image_relative_path(Path::new(path)).ok()?;
        let mut parent = TreePtr::root();
        let names: Vec<&str> = relative.iter().filter_map(|c| c.to_str()).collect();
        let (name, dirs) = names.split_last()?;
        for dir in dirs {
            parent = fs
                .tx(|tx| tx.find_node(parent, dir).map(|n| n.ptr()))
                .ok()?;
        }
        fs.tx(|tx| {
            let node = tx.find_node(parent, name)?;
            let mut buf = vec![0u8; node.data().size() as usize];
            tx.read_node(node.ptr(), 0, &mut buf, 0, 0)?;
            Ok(buf)
        })
        .ok()
    }

    /// Writing the per-invocation files must leave the environment's own files
    /// alone: the guest image is prepared once and reused by every tool, so
    /// losing what was installed would make the environment useless.
    #[test]
    fn writing_files_keeps_the_images_existing_files() {
        let (_temp, image) = whole_disk_with(&[
            ("bin/tool", b"the tool"),
            ("etc/os-release", b"Ferric Oxide"),
        ]);

        write_files(
            &image,
            &[(PathBuf::from("/etc/redoxerd"), b"tool\n".to_vec(), 0o644)],
        )
        .unwrap();

        assert_eq!(
            read_guest_file(&image, "/etc/redoxerd").as_deref(),
            Some(&b"tool\n"[..]),
            "the per-invocation file is written"
        );
        assert_eq!(
            read_guest_file(&image, "/etc/os-release").as_deref(),
            Some(&b"Ferric Oxide"[..]),
            "the environment's own files are preserved"
        );
        assert_eq!(
            read_guest_file(&image, "/bin/tool").as_deref(),
            Some(&b"the tool"[..]),
            "the installed tools are preserved"
        );
    }

    /// Writing a path whose directories do not exist yet creates them, so a
    /// `--folder` destination needs no pre-made layout in the image.
    #[test]
    fn writing_files_creates_missing_directories() {
        let (_temp, image) = whole_disk_with(&[("etc/os-release", b"Ferric Oxide")]);
        write_files(
            &image,
            &[(
                PathBuf::from("/root/project/build.sh"),
                b"true\n".to_vec(),
                0o755,
            )],
        )
        .unwrap();
        assert_eq!(
            read_guest_file(&image, "/root/project/build.sh").as_deref(),
            Some(&b"true\n"[..])
        );
    }

    /// A path outside the image is refused rather than turned into a file at
    /// the image's root.
    #[test]
    fn writing_files_refuses_a_path_outside_the_image() {
        let (_temp, image) = whole_disk_with(&[("etc/os-release", b"Ferric Oxide")]);
        let error = write_files(
            &image,
            &[(PathBuf::from("/../outside"), b"data".to_vec(), 0o644)],
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("outside the guest image"),
            "the error names the refused path: {error}"
        );
    }
}
