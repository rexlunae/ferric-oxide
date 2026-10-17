//! Validate paths within an exclusively owned image tree before host filesystem writes.

#![forbid(unsafe_code)]

use std::{
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

/// Join a validated path to an image root, rejecting existing symbolic links.
///
/// The caller must exclusively own the image tree while checking and using the
/// result; this check does not protect against concurrent filesystem mutations.
///
/// # Errors
/// Returns traversal, symbolic-link or filesystem inspection errors.
pub fn image_path(root: &Path, path: &Path) -> Result<PathBuf, ImagePathError> {
    let mut result = root.to_path_buf();
    for component in image_relative_path(path)?.components() {
        result.push(component);
        match fs::symlink_metadata(&result) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ImagePathError::Symlink(result));
            }
            Ok(_) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(ImagePathError::Io(source)),
        }
    }
    Ok(result)
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
