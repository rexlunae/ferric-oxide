//! Validate paths within an exclusively owned image tree before host filesystem writes.

#![forbid(unsafe_code)]

use std::{
    collections::VecDeque,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
};

/// Failure to resolve a path within an image tree.
#[derive(Debug)]
#[non_exhaustive]
pub enum ImagePathError {
    /// A parent-directory component could escape the image.
    ParentTraversal,
    /// A platform-specific path prefix is unsupported.
    UnsupportedPrefix,
    /// An existing symbolic link would redirect access outside the image.
    Symlink(PathBuf),
    /// Too many symbolic links were followed, usually indicating a cycle.
    SymlinkLoop,
    /// Inspecting a path component failed.
    Io(io::Error),
}

impl fmt::Display for ImagePathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParentTraversal => f.write_str("image paths must not contain '..'"),
            Self::UnsupportedPrefix => {
                f.write_str("image paths must not contain a platform prefix")
            }
            Self::Symlink(path) => {
                write!(f, "image path traverses symbolic link {}", path.display())
            }
            Self::SymlinkLoop => f.write_str("image path contains a symbolic-link loop"),
            Self::Io(source) => source.fmt(f),
        }
    }
}
impl std::error::Error for ImagePathError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            _ => None,
        }
    }
}

/// Normalize an image path to a relative path, accepting a leading `/`.
///
/// # Errors
/// Returns an error for parent traversal or a platform-specific prefix.
///
/// # Examples
/// ```
/// use std::path::Path;
/// assert_eq!(redox_installer::image_relative_path(Path::new("/etc/config"))?, Path::new("etc/config"));
/// # Ok::<(), redox_installer::ImagePathError>(())
/// ```
pub fn image_relative_path(path: &Path) -> Result<PathBuf, ImagePathError> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return Err(ImagePathError::ParentTraversal),
            Component::Prefix(_) => return Err(ImagePathError::UnsupportedPrefix),
        }
    }
    Ok(relative)
}

/// Resolve a path inside an image, allowing symbolic links contained in the root.
///
/// Relative link targets are resolved from their image directory. Absolute link
/// targets are resolved from the image root, never the host root. The root itself must be an existing directory, not a symlink.
/// The caller must exclusively own the image tree while checking and using the
/// result; this check does not protect against concurrent filesystem mutations.
///
/// # Errors
/// Returns traversal, escaping-link, link-loop or filesystem inspection errors.
pub fn image_path(root: &Path, path: &Path) -> Result<PathBuf, ImagePathError> {
    let relative = image_relative_path(path)?;
    let metadata = fs::symlink_metadata(root).map_err(ImagePathError::Io)?;
    if metadata.file_type().is_symlink() {
        return Err(ImagePathError::Symlink(root.to_path_buf()));
    }
    if !metadata.is_dir() {
        return Err(ImagePathError::Io(io::Error::new(
            io::ErrorKind::NotADirectory,
            "image root must be a directory",
        )));
    }
    let root = fs::canonicalize(root).map_err(ImagePathError::Io)?;
    let mut result = root.clone();
    let mut pending = path_components(&relative)?;
    let mut links = 0;
    while let Some(component) = pending.pop_front() {
        if component == ".." {
            if result == root {
                return Err(ImagePathError::ParentTraversal);
            }
            result.pop();
            continue;
        }
        result.push(component);
        match fs::symlink_metadata(&result) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                links += 1;
                if links > 40 {
                    return Err(ImagePathError::SymlinkLoop);
                }
                let target = fs::read_link(&result).map_err(ImagePathError::Io)?;
                let target = if target.is_absolute() {
                    let contained = target
                        .strip_prefix(Path::new("/"))
                        .map_err(|_| ImagePathError::UnsupportedPrefix)?
                        .to_path_buf();
                    result = root.clone();
                    contained
                } else {
                    result.pop();
                    target
                };
                let mut expanded = path_components(&target)?;
                expanded.append(&mut pending);
                pending = expanded;
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ImagePathError::Io(source)),
        }
    }
    Ok(result)
}

fn path_components(path: &Path) -> Result<VecDeque<std::ffi::OsString>, ImagePathError> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(Ok(part.to_os_string())),
            Component::ParentDir => Some(Ok("..".into())),
            Component::CurDir => None,
            Component::RootDir | Component::Prefix(_) => {
                Some(Err(ImagePathError::UnsupportedPrefix))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_paths_reject_parent_traversal() {
        for path in ["../outside", "/../../outside", "/etc/../outside"] {
            assert!(matches!(
                image_relative_path(Path::new(path)),
                Err(ImagePathError::ParentTraversal)
            ));
        }
        assert_eq!(
            image_relative_path(Path::new("/etc/./config")).unwrap(),
            Path::new("etc/config")
        );
        assert_eq!(image_relative_path(Path::new("/")).unwrap(), Path::new(""));
    }
}
