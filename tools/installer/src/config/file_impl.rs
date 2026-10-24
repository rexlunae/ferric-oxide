//! Create configured image entries without traversing paths outside the image root.

use anyhow::{Context, Result};
use libc::{gid_t, uid_t};

use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{Error, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

fn chown<P: AsRef<Path>>(path: P, uid: uid_t, gid: gid_t, recursive: bool) -> Result<()> {
    let path = path.as_ref();

    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: c_path is NUL-terminated and valid for the duration of lchown.
    if unsafe { libc::lchown(c_path.as_ptr(), uid, gid) } != 0 {
        return Err(Error::last_os_error().into());
    }

    if recursive && fs::symlink_metadata(path)?.is_dir() {
        for entry_res in fs::read_dir(path)? {
            let entry = entry_res?;
            chown(entry.path(), uid, gid, recursive)?;
        }
    }

    Ok(())
}

// TODO: Rewrite impls
impl crate::FileConfig {
    #[cfg(test)]
    pub(crate) fn create<P: AsRef<Path>>(&self, prefix: P) -> Result<()> {
        self.create_with_ownership(prefix, &mut None)
    }

    pub(crate) fn create_with_ownership<P: AsRef<Path>>(
        &self,
        prefix: P,
        ownership: &mut Option<crate::ImageOwnership>,
    ) -> Result<()> {
        let prefix = prefix.as_ref();
        let relative = crate::image_relative_path(Path::new(&self.path))?;
        let target_file = if self.symlink {
            // Replacing the link itself is safe; only its parent is traversed.
            let parent = crate::image_path(prefix, relative.parent().unwrap_or(Path::new("")))?;
            parent.join(relative.file_name().ok_or_else(|| {
                Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "symlink requires a filename",
                )
            })?)
        } else {
            crate::image_path(prefix, &relative)?
        };

        if self.directory {
            println!("Create directory {}", target_file.display());
            fs::create_dir_all(&target_file)
                .with_context(|| format!("failed to create directory {}", target_file.display()))?;
            self.apply_perms(prefix, &target_file, ownership)?;
            return Ok(());
        } else if let Some(parent) = target_file.parent() {
            println!("Create file parent {}", parent.display());
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create file parent {}", parent.display()))?;
        }

        if self.symlink {
            println!("Create symlink {} to {}", target_file.display(), self.data);
            if target_file.is_symlink() {
                fs::remove_file(&target_file).with_context(|| {
                    format!("failed to remove old symlink {}", target_file.display())
                })?;
            }
            symlink(&OsStr::new(&self.data), &target_file).with_context(|| {
                format!(
                    "failed to create symlink {} to {}",
                    target_file.display(),
                    self.data
                )
            })?;
            Ok(())
        } else {
            let action = if self.append { "Append" } else { "Create" };
            println!("{action} file {}", target_file.display());

            let mut file = if self.append {
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&target_file)
                    .with_context(|| format!("failed to append file {}", target_file.display()))?
            } else {
                File::create(&target_file)
                    .with_context(|| format!("failed to create file {}", target_file.display()))?
            };
            file.write_all(self.data.as_bytes())?;

            self.apply_perms(prefix, target_file, ownership)
        }
    }

    fn apply_perms<P: AsRef<Path>>(
        &self,
        root: &Path,
        target: P,
        ownership: &mut Option<crate::ImageOwnership>,
    ) -> Result<()> {
        let path = target.as_ref();
        let mode = self
            .mode
            .unwrap_or_else(|| if self.directory { 0o0755 } else { 0o0644 });
        let uid = self.uid.unwrap_or(!0);
        let gid = self.gid.unwrap_or(!0);

        // chmod
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?;

        if let Some(ownership) = ownership {
            ownership.record(root, path, self.uid, self.gid, self.recursive_chown)?;
            return Ok(());
        }
        // chown
        chown(path, uid, gid, self.recursive_chown)
            .with_context(|| format!("failed to chown {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileConfig;

    struct TestDirectory(std::path::PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.keep().canonicalize().unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn public_directory_installation_creates_an_absent_root() {
        let sandbox = TestDirectory::new();
        let root = sandbox.path().join("new/image");
        let config = crate::Config {
            files: vec![FileConfig::new_file(
                "/etc/hostname".into(),
                "ferric".into(),
            )],
            ..Default::default()
        };
        crate::install_dir(config, &root, None).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("etc/hostname")).unwrap(),
            "ferric"
        );
    }

    #[test]
    fn no_mount_installation_creates_and_archives_a_new_output_tree() {
        let sandbox = TestDirectory::new();
        let image = sandbox.path().join("image.bin");
        File::create(&image)
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        let output = sandbox.path().join("new-root");
        let config = crate::Config {
            files: vec![FileConfig::new_file(
                "/etc/hostname".into(),
                "ferric".into(),
            )],
            ..Default::default()
        };
        let disk = redoxfs::DiskFile::open(&image).unwrap();
        crate::with_redoxfs(disk, None, |filesystem| {
            crate::with_redoxfs_ar(filesystem, Some(&output), |root| {
                crate::install_dir(config, root, None)
            })
        })
        .unwrap();
        assert!(!output.exists());
        assert!(
            redoxfs::FileSystem::open(redoxfs::DiskFile::open(&image).unwrap(), None, None, true)
                .is_ok()
        );
    }

    #[test]
    fn public_directory_installation_rejects_a_symlink_root_before_writing() {
        let sandbox = TestDirectory::new();
        let outside = sandbox.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let root = sandbox.path().join("image");
        symlink(&outside, &root).unwrap();
        let config = crate::Config {
            files: vec![FileConfig::new_file("/etc/hostname".into(), "bad".into())],
            ..Default::default()
        };
        assert!(crate::install_dir(config, &root, None).is_err());
        assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    }

    #[test]
    fn traversal_is_rejected_before_any_write() {
        let sandbox = TestDirectory::new();
        let root = sandbox.path().join("image");
        fs::create_dir(&root).unwrap();
        for path in ["../outside", "/../../outside", "/etc/../outside"] {
            let file = FileConfig::new_file(path.into(), "overwrite".into());
            assert!(file.create(&root).is_err());
        }
        assert!(!sandbox.path().join("outside").exists());
        assert_eq!(fs::read_dir(root).unwrap().count(), 0);
    }

    #[test]
    fn symlink_ancestors_and_targets_cannot_redirect_writes() {
        let sandbox = TestDirectory::new();
        let root = sandbox.path().join("image");
        let outside = sandbox.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), "original").unwrap();
        symlink("../outside", root.join("link")).unwrap();
        let through_parent = FileConfig::new_file("/link/file".into(), "bad".into());
        assert!(through_parent.create(&root).is_err());
        symlink("../outside/file", root.join("file")).unwrap();
        let through_target = FileConfig::new_file("/file".into(), "bad".into());
        assert!(through_target.create(&root).is_err());
        assert_eq!(
            fs::read_to_string(outside.join("file")).unwrap(),
            "original"
        );
    }

    #[test]
    fn internal_image_links_support_writes_and_artifact_reads() {
        let sandbox = TestDirectory::new();
        let root = sandbox.path();
        fs::create_dir_all(root.join("usr/bin")).unwrap();
        fs::create_dir(root.join("nested")).unwrap();
        symlink("usr/bin", root.join("bin")).unwrap();
        symlink("../usr/bin", root.join("nested/bin")).unwrap();
        symlink("/usr/bin", root.join("absolute-bin")).unwrap();
        for path in ["/bin/app", "/nested/bin/app", "/absolute-bin/app"] {
            FileConfig::new_file(path.into(), "application".into())
                .create(root)
                .unwrap();
            let artifact = crate::image_path(root, Path::new(path)).unwrap();
            assert_eq!(artifact, root.join("usr/bin/app"));
            assert_eq!(fs::read_to_string(artifact).unwrap(), "application");
        }
        assert_eq!(
            crate::image_path(root, Path::new("/bin")).unwrap(),
            root.join("usr/bin")
        );
    }

    #[test]
    fn image_root_symlinks_and_escaping_relative_links_are_rejected() {
        let sandbox = TestDirectory::new();
        let root = sandbox.path().join("image");
        let outside = sandbox.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, &root).unwrap();
        let file = FileConfig::new_file("/file".into(), "bad".into());
        assert!(file.create(&root).is_err());
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
        fs::remove_file(&root).unwrap();
        fs::create_dir(&root).unwrap();
        symlink("../outside", root.join("escape")).unwrap();
        assert!(
            FileConfig::new_file("/escape/file".into(), "bad".into())
                .create(&root)
                .is_err()
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    }

    #[test]
    fn absolute_image_links_never_write_to_the_host_root() {
        let sandbox = TestDirectory::new();
        let root = sandbox.path().join("image");
        let outside = sandbox.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), "host data").unwrap();
        symlink(&outside, root.join("absolute")).unwrap();
        FileConfig::new_file("/absolute/file".into(), "guest data".into())
            .create(&root)
            .unwrap();
        assert_eq!(
            fs::read_to_string(outside.join("file")).unwrap(),
            "host data"
        );
        let resolved = crate::image_path(&root, Path::new("absolute/file")).unwrap();
        assert!(resolved.starts_with(&root));
        assert_eq!(fs::read_to_string(resolved).unwrap(), "guest data");
    }

    #[test]
    fn image_symlink_cycles_are_bounded() {
        let sandbox = TestDirectory::new();
        symlink("b", sandbox.path().join("a")).unwrap();
        symlink("a", sandbox.path().join("b")).unwrap();
        assert!(matches!(
            crate::image_path(sandbox.path(), Path::new("a/file")),
            Err(crate::ImagePathError::SymlinkLoop)
        ));
    }

    #[test]
    fn valid_files_and_link_replacement_stay_inside_image() {
        let sandbox = TestDirectory::new();
        let file = FileConfig::new_file("/etc/example".into(), "contents".into());
        file.create(sandbox.path()).unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.path().join("etc/example")).unwrap(),
            "contents"
        );
        let link = FileConfig {
            path: "/etc/link".into(),
            data: "example".into(),
            symlink: true,
            ..Default::default()
        };
        link.create(sandbox.path()).unwrap();
        link.create(sandbox.path()).unwrap();
        assert_eq!(
            fs::read_link(sandbox.path().join("etc/link")).unwrap(),
            Path::new("example")
        );
    }
}
