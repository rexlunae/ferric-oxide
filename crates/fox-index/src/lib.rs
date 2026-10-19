//! The fox package registry.
//!
//! A fox registry is a git repository of TOML files (packages, artifact
//! lists, advisories) signed with a subset of [The Update
//! Framework](https://theupdateframework.github.io/specification/v1.0.33/)
//! called `fox-tuf/1`. This crate holds everything both sides need:
//!
//! - [`canonical`]: the OLPC canonical JSON that metadata signatures cover;
//! - [`metadata`]: the root, targets, snapshot and timestamp roles, and
//!   [`Signed`](metadata::Signed) envelopes that sign and verify them;
//! - [`layout`]: `registry.toml`, package files, artifact lists and the
//!   paths they live at;
//! - [`advisory`]: security advisories, in the RustSec format, from the
//!   registry or the RustSec database;
//! - [`verify`]: the client workflow that checks a checkout and reads only
//!   files the metadata signs;
//! - [`edit`]: the maintainer side, which creates a registry, admits
//!   packages, records releases and re-signs the metadata.
//!
//! See `docs/src/ferric-packaging.md` for how the registry is run, and
//! `packaging-design.md` section 6 for the design.
//!
//! # Examples
//!
//! Creating and signing an empty registry, then verifying it as a client
//! would:
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use fox_index::edit::{Keyring, Registry, SignOptions, generate_secret, single_key_roles};
//! use fox_index::verify::{self, ClientState};
//! use time::OffsetDateTime;
//!
//! let dir = tempfile::tempdir()?;
//! let now = OffsetDateTime::now_utc();
//! let mut keyring = Keyring::new();
//! let key = keyring.add(generate_secret());
//! let mut registry = Registry::init(dir.path(), "example", &single_key_roles(key), now)?;
//! let report = registry.sign(&keyring, now, &SignOptions::default())?;
//! assert!(report.complete());
//!
//! let mut state = ClientState::trust_root(registry.root().clone())?;
//! let verified = verify::update(dir.path(), &mut state, now)?;
//! assert_eq!(verified.config()?.name, "example");
//! assert!(verified.package_names().is_empty());
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod advisory;
pub mod canonical;
pub mod edit;
pub mod layout;
pub mod metadata;
pub mod verify;
