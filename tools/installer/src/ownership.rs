//! Guest ownership for images assembled without changing host file owners.
//!
//! Package archives contain modes but no ownership. Files default to guest root;
//! explicit installer declarations and account home directories override them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use redoxfs::{Disk, FileSystem, Node, Transaction, TreePtr};

#[derive(Debug, Default)]
/// Guest UID/GID overrides captured while installing into an unprivileged directory.
///
/// Use [`crate::install_dir_unprivileged`] to create this map and [`Self::archive`]
/// to apply it while writing a RedoxFS image. Host ownership is never copied.
pub struct ImageOwnership(BTreeMap<PathBuf, (u32, u32)>);

impl ImageOwnership {
    pub(crate) fn record(
        &mut self,
        root: &Path,
        path: &Path,
        uid: Option<u32>,
        gid: Option<u32>,
        recursive: bool,
    ) -> io::Result<()> {
        let relative = path
            .strip_prefix(root)
            .map_err(|source| io::Error::new(io::ErrorKind::InvalidInput, source))?;
        let entry = self.0.entry(relative.to_owned()).or_insert((0, 0));
        if let Some(uid) = uid {
            entry.0 = uid;
        }
        if let Some(gid) = gid {
            entry.1 = gid;
        }
        if recursive && fs::symlink_metadata(path)?.is_dir() {
            for entry in fs::read_dir(path)? {
                self.record(root, &entry?.path(), uid, gid, true)?;
            }
        }
        Ok(())
    }

    /// Archive a staging directory with guest owners and the host's permission bits.
    ///
    /// # Errors
    /// Returns staging-directory read errors or RedoxFS storage errors.
    pub fn archive<D: Disk>(&self, filesystem: &mut FileSystem<D>, root: &Path) -> io::Result<()> {
        // Archive first; the transaction then replaces host IDs before committing.
        filesystem
            .tx(|tx| {
                redoxfs::archive_at(tx, root, TreePtr::root()).map_err(|source| {
                    syscall::Error::new(source.raw_os_error().unwrap_or(syscall::EIO))
                })?;
                self.apply(tx, TreePtr::root(), Path::new(""))?;
                tx.sync(true)?;
                Ok(())
            })
            .map_err(|source| io::Error::from_raw_os_error(source.errno))
    }

    fn apply<D: Disk>(
        &self,
        tx: &mut Transaction<D>,
        ptr: TreePtr<Node>,
        path: &Path,
    ) -> syscall::Result<()> {
        let mut node = tx.read_tree(ptr)?;
        let (uid, gid) = self.0.get(path).copied().unwrap_or((0, 0));
        node.data_mut().set_uid(uid);
        node.data_mut().set_gid(gid);
        let is_dir = node.data().is_dir();
        tx.sync_tree(node)?;
        if is_dir {
            let mut children = Vec::new();
            tx.child_nodes(ptr, &mut children)?;
            for entry in children {
                let name = entry
                    .name()
                    .ok_or_else(|| syscall::Error::new(syscall::EINVAL))?;
                self.apply(tx, entry.node_ptr(), &path.join(name))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn guest_owners_round_trip_without_host_chown() {
        let temp = tempfile::tempdir().unwrap();
        let staging = temp.path().join("staging");
        let config = toml::from_str(
            r#"
            [general]
            prompt = false
            [users.root]
            uid = 0
            gid = 0
            password = ""
            [users.guest]
            uid = 4242
            gid = 4343
            password = ""
            [[files]]
            path = "/etc/special"
            data = "hello"
            uid = 12
            gid = 34
            mode = 384
        "#,
        )
        .unwrap();
        let ownership = crate::install_dir_unprivileged(config, &staging, None).unwrap();
        let host_uid = fs::metadata(&staging).unwrap().uid();
        assert_eq!(
            fs::metadata(staging.join("home/guest")).unwrap().uid(),
            host_uid
        );
        assert_eq!(
            fs::metadata(staging.join("etc/special")).unwrap().uid(),
            host_uid
        );
        symlink("/etc/special", staging.join("link")).unwrap();
        let disk =
            redoxfs::DiskFile::create(temp.path().join("disk.img"), 16 * 1024 * 1024).unwrap();
        crate::with_redoxfs(disk, None, |mut filesystem| {
            ownership.archive(&mut filesystem, &staging)?;
            filesystem
                .tx(|tx| {
                    let root = tx.read_tree(TreePtr::<Node>::root())?;
                    assert_eq!((root.data().uid(), root.data().gid()), (0, 0));
                    let home = tx.find_node(TreePtr::root(), "home")?;
                    let guest = tx.find_node(home.ptr(), "guest")?;
                    assert_eq!((guest.data().uid(), guest.data().gid()), (4242, 4343));
                    let config = tx.find_node(guest.ptr(), ".config")?;
                    assert_eq!((config.data().uid(), config.data().gid()), (4242, 4343));
                    let etc = tx.find_node(TreePtr::root(), "etc")?;
                    let special = tx.find_node(etc.ptr(), "special")?;
                    assert_eq!((special.data().uid(), special.data().gid()), (12, 34));
                    assert_eq!(special.data().mode() & 0o777, 0o600);
                    let shadow = tx.find_node(etc.ptr(), "shadow")?;
                    assert_eq!((shadow.data().uid(), shadow.data().gid()), (0, 0));
                    assert_eq!(shadow.data().mode() & 0o777, 0o600);
                    let link = tx.find_node(TreePtr::root(), "link")?;
                    assert_eq!((link.data().uid(), link.data().gid()), (0, 0));
                    assert!(link.data().is_symlink());
                    Ok(())
                })
                .map_err(|error| io::Error::from_raw_os_error(error.errno))?;
            Ok(())
        })
        .unwrap();
    }
}
