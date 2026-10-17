//! Image file declarations and human-readable installation summaries.

use std::fmt::Display;

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
/// A file, directory or symbolic link to create in the image.
pub struct FileConfig {
    /// Destination inside the image, optionally beginning with `/`.
    pub path: String,
    /// File contents, or the target of a symbolic link.
    pub data: String,
    #[serde(default)]
    /// Create a symbolic link instead of a regular file.
    pub symlink: bool,
    #[serde(default)]
    /// Create a directory instead of a regular file.
    pub directory: bool,
    /// Unix permission bits; defaults depend on the entry kind.
    pub mode: Option<u32>,
    /// Owner user ID; an absent value preserves ownership.
    pub uid: Option<u32>,
    /// Owner group ID; an absent value preserves ownership.
    pub gid: Option<u32>,
    #[serde(default)]
    /// Apply directory ownership to its descendants.
    pub recursive_chown: bool,
    #[serde(default)]
    /// Create this entry after package installation.
    pub postinstall: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    /// Append data instead of replacing regular file contents.
    pub append: bool,
}

impl FileConfig {
    /// Construct a regular-file declaration with default permissions.
    pub fn new_file(path: String, data: String) -> FileConfig {
        FileConfig {
            path,
            data,
            ..Default::default()
        }
    }

    /// Construct a directory declaration with default permissions.
    pub fn new_directory(path: String) -> FileConfig {
        FileConfig {
            path,
            data: String::new(),
            directory: true,
            ..Default::default()
        }
    }

    /// Set permission bits and owner IDs, returning this declaration.
    pub fn with_mod(&mut self, mode: u32, uid: u32, gid: u32) -> &mut FileConfig {
        self.mode = Some(mode);
        self.uid = Some(uid);
        self.gid = Some(gid);
        self
    }

    /// Set permissions and owner IDs, enabling recursive ownership changes.
    pub fn with_recursive_mod(&mut self, mode: u32, uid: u32, gid: u32) -> &mut FileConfig {
        self.with_mod(mode, uid, gid);
        self.recursive_chown = true;
        self
    }
}

impl Display for FileConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.path)?;
        if self.symlink {
            write!(f, " -> {}", self.data)?;
        } else if self.directory {
            write!(f, " type=dir")?;
            if self.recursive_chown {
                write!(f, " chown=yes")?;
            }
        } else {
            write!(f, " size={}", format_bytes(self.data.len() as u64))?;
            if self.postinstall {
                write!(f, "!")?;
            }
            if self.append {
                write!(f, " append=yes")?;
            }
        }
        if let Some(uid) = self.uid {
            write!(f, " uid={}", uid)?;
        }
        if let Some(gid) = self.gid {
            write!(f, " gid={}", gid)?;
        }
        if let Some(mode) = self.mode {
            write!(f, " mode={:3o}", mode)?;
        }
        Ok(())
    }
}

/// Convert bytes into readable string
pub fn format_bytes(len: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    const KB: u64 = 1024;

    if len > GB {
        format_bytes_inner(len, GB, "GB")
    } else if len > MB {
        format_bytes_inner(len, MB, "MB")
    } else if len > KB {
        format_bytes_inner(len, KB, "KB")
    } else {
        format!("{len} B")
    }
}

fn format_bytes_inner(len: u64, divisor: u64, suffix: &'static str) -> String {
    use std::fmt::Write;
    let mut s = format!("{}", len / divisor);
    if s.len() == 1 {
        let _ = write!(s, ".{:02}", (len % divisor) / (divisor / 100));
    } else if s.len() == 2 {
        let _ = write!(s, ".{:01}", (len % divisor) / (divisor / 10));
    }

    let _ = write!(s, " {suffix}");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn summary_distinguishes_user_and_group() {
        let file = FileConfig {
            uid: Some(1000),
            gid: Some(42),
            ..Default::default()
        };
        assert!(file.to_string().contains("uid=1000 gid=42"));
        let group_only = FileConfig {
            gid: Some(42),
            ..Default::default()
        };
        assert!(group_only.to_string().contains("gid=42"));
        assert!(!group_only.to_string().contains("uid="));
    }
}
