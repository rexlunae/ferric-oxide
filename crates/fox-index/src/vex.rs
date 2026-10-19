//! VEX documents a registry publishes, at `vex/<id>.cdx.json`.
//!
//! Each file is a CycloneDX VEX document (see [`fox_sbom::vex`]) saying
//! whether vulnerabilities affect packages of the registry. The targets
//! role signs the directory like every other top-level file, so clients
//! read the statements only through the verified checkout. The id is the
//! file name: ASCII letters, digits and `-`, as for advisories.

use fox_sbom::vex::{VexError, VexStatement, read_vex};

use crate::advisory::valid_id;
use crate::layout::VEX_DIR;

/// Why a VEX file cannot be stored or read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VexFileError {
    /// The id has characters other than ASCII letters, digits and `-`.
    #[error("VEX id {id:?} may only hold letters, digits and '-'")]
    Id {
        /// The id.
        id: String,
    },
    /// The document is not usable VEX.
    #[error("invalid VEX document")]
    Vex(#[source] VexError),
}

/// One VEX file of a registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VexDocument {
    /// Its registry path.
    pub path: String,
    /// What it says.
    pub statements: Vec<VexStatement>,
}

/// The registry path of VEX document `id`.
///
/// # Errors
///
/// [`VexFileError::Id`] if `id` holds anything but ASCII letters, digits
/// and `-`.
pub fn vex_path(id: &str) -> Result<String, VexFileError> {
    if !valid_id(id) {
        return Err(VexFileError::Id { id: id.to_owned() });
    }
    Ok(format!("{VEX_DIR}/{id}.cdx.json"))
}

/// Reads a VEX document's statements.
///
/// # Errors
///
/// [`VexFileError::Vex`] if it is not usable VEX (see
/// [`read_vex`]).
pub fn parse_vex(json: &[u8]) -> Result<Vec<VexStatement>, VexFileError> {
    read_vex(json).map_err(VexFileError::Vex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vex_ids_are_file_names() {
        assert_eq!(vex_path("tiny-2026").unwrap(), "vex/tiny-2026.cdx.json");
        for bad in ["", "../x", "a.b", "a/b"] {
            assert!(
                matches!(vex_path(bad), Err(VexFileError::Id { .. })),
                "{bad}"
            );
        }
    }
}
