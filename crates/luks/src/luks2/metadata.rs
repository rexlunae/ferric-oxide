//! The LUKS2 JSON metadata (LUKS2 On-Disk Format Specification 1.1.4, sections 2.2 and 3).
//!
//! The JSON area holds a NUL-terminated JSON object with five mandatory members: `keyslots`,
//! `digests`, `segments`, `tokens` and `config`. Every object inside the first four is named by
//! a decimal number and has a `type`. Binary values are Base64; 64-bit integers are decimal
//! strings ("string-uint64").
//!
//! [`Metadata::parse`] turns the area into typed objects and checks what the specification
//! requires of them: areas inside the keyslots area, references between objects that resolve,
//! sizes that match the binary header. Objects of a type this crate does not know are kept by
//! type name, as the specification asks, so a header that has them still parses.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use base64ct::{Base64, Encoding};
use serde_json::{Map, Value};
use thiserror::Error;

use super::binary::BINARY_HEADER_LEN;
use crate::cipher::KEY_MATERIAL_SECTOR;
use crate::hash::HashAlg;

/// The only anti-forensic stripe count the specification supports (section 3.2.4).
pub const AF_STRIPES: u32 = 4000;
/// The largest key, in bytes, this crate accepts in a keyslot (a 4096-bit key).
pub const MAX_KEY_SIZE: u32 = 512;
/// The sector sizes a crypt segment may use (section 3.3.2).
pub const SECTOR_SIZES: [u32; 4] = [512, 1024, 2048, 4096];

type Object = Map<String, Value>;

/// The parsed JSON metadata of one header copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// The keyslots, by number.
    pub keyslots: BTreeMap<u32, Keyslot>,
    /// The digests, by number.
    pub digests: BTreeMap<u32, Digest>,
    /// The segments, by number.
    pub segments: BTreeMap<u32, Segment>,
    /// The tokens, by number.
    pub tokens: BTreeMap<u32, Token>,
    /// The `config` object.
    pub config: Config,
}

/// A keyslot (section 3.2).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Keyslot {
    /// A `luks2` keyslot: a key stored as in LUKS1, with per-keyslot algorithms.
    Luks2(Luks2Keyslot),
    /// A `reencrypt` keyslot, which holds recovery data for online reencryption, not a key.
    Reencrypt {
        /// Its binary area.
        area: Area,
    },
    /// A keyslot of a type this crate does not know.
    Other {
        /// The keyslot's `type`.
        kind: String,
    },
}

/// A `luks2` keyslot (section 3.2.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Luks2Keyslot {
    /// The size of the stored key, in bytes.
    pub key_size: u32,
    /// When to try the keyslot.
    pub priority: Priority,
    /// Where the encrypted key material is, and how it is encrypted.
    pub area: RawArea,
    /// How the passphrase becomes the area key.
    pub kdf: Kdf,
    /// The anti-forensic splitter.
    pub af: Af,
}

/// A keyslot priority (section 3.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    /// 0: used only when named explicitly.
    Ignore,
    /// 1, the default.
    Normal,
    /// 2: tried before normal keyslots.
    High,
}

/// A keyslot's place in the keyslots area (section 3.2.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Area {
    /// The area's `type`.
    pub kind: String,
    /// Its offset from the device start, in bytes.
    pub offset: u64,
    /// Its size, in bytes.
    pub size: u64,
}

/// A `raw` area: encrypted key material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawArea {
    /// The offset from the device start, in bytes.
    pub offset: u64,
    /// The size, in bytes.
    pub size: u64,
    /// The encryption, in dm-crypt notation such as `aes-xts-plain64`.
    pub encryption: String,
    /// The size of the area key, in bytes.
    pub key_size: u32,
}

/// A key derivation function (section 3.2.5).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kdf {
    /// PBKDF2 with HMAC.
    Pbkdf2 {
        /// The hash.
        hash: HashAlg,
        /// The iteration count.
        iterations: u32,
        /// The salt.
        salt: Vec<u8>,
    },
    /// Argon2 (RFC 9106), version 0x13.
    Argon2 {
        /// `argon2i` or `argon2id`.
        variant: Argon2Variant,
        /// The time cost (passes).
        time: u32,
        /// The memory cost, in KiB.
        memory_kib: u32,
        /// The parallelism (lanes).
        cpus: u32,
        /// The salt.
        salt: Vec<u8>,
    },
    /// A KDF this crate does not know, by its `type`.
    Other(String),
}

/// An Argon2 variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Argon2Variant {
    /// Argon2i.
    I,
    /// Argon2id.
    Id,
}

/// The anti-forensic splitter of a keyslot (section 3.2.4, type `luks1`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Af {
    /// The stripe count (always [`AF_STRIPES`]).
    pub stripes: u32,
    /// The diffusion hash.
    pub hash: HashAlg,
}

/// A digest that recognises the right key (section 3.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    /// The keyslots whose keys it checks.
    pub keyslots: Vec<u32>,
    /// The segments those keys encrypt (empty for an unbound key).
    pub segments: Vec<u32>,
    /// The digest itself.
    pub kind: DigestKind,
}

/// The kinds of digest.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DigestKind {
    /// PBKDF2 of the key, as in LUKS1.
    Pbkdf2 {
        /// The hash.
        hash: HashAlg,
        /// The iteration count.
        iterations: u32,
        /// The salt.
        salt: Vec<u8>,
        /// The expected output.
        digest: Vec<u8>,
    },
    /// A digest type this crate does not know.
    Other(String),
}

/// A data segment (section 3.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// The offset from the device start, in bytes.
    pub offset: u64,
    /// The size.
    pub size: SegmentSize,
    /// The flags, possibly empty.
    pub flags: Vec<String>,
    /// What the segment holds.
    pub kind: SegmentKind,
}

/// A segment's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentSize {
    /// To the end of the device.
    Dynamic,
    /// A fixed size, in bytes.
    Bytes(u64),
}

/// The kinds of segment.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SegmentKind {
    /// Encrypted data.
    Crypt {
        /// The IV of the segment's first sector.
        iv_tweak: u64,
        /// The encryption, in dm-crypt notation.
        encryption: String,
        /// The encryption sector size, in bytes.
        sector_size: u32,
        /// The integrity type, if the segment is integrity-protected.
        integrity: Option<String>,
    },
    /// Plaintext (only during reencryption).
    Linear,
    /// A segment type this crate does not know.
    Other(String),
}

/// A token (section 3.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The token's `type`.
    pub kind: String,
    /// The keyslots it can open.
    pub keyslots: Vec<u32>,
}

/// The `config` object (section 3.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// The JSON area size, in bytes.
    pub json_size: u64,
    /// The keyslots area size, in bytes.
    pub keyslots_size: u64,
    /// Persistent activation flags.
    pub flags: Vec<String>,
    /// Mandatory requirements. A device with one this reader does not implement must not be
    /// activated (section 3.5); this crate implements none.
    pub requirements: Vec<String>,
}

/// Where in the metadata an error is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Location {
    /// The top-level object.
    Root,
    /// A keyslot.
    Keyslot(u32),
    /// A digest.
    Digest(u32),
    /// A segment.
    Segment(u32),
    /// A token.
    Token(u32),
    /// The `config` object.
    Config,
}

impl Metadata {
    /// Parses and checks the JSON area of a copy whose binary header gives `hdr_size`.
    ///
    /// # Errors
    ///
    /// A [`MetadataError`] for JSON that does not parse, a missing or mistyped member, or a
    /// value the specification does not allow.
    pub fn parse(json_area: &[u8], hdr_size: u64) -> Result<Self, MetadataError> {
        let end = json_area
            .iter()
            .position(|&b| b == 0)
            .ok_or(MetadataError::NotTerminated)?;
        let root: Value = serde_json::from_slice(&json_area[..end]).map_err(MetadataError::Json)?;
        let root = as_object(&root, Location::Root, "metadata")?;
        let config = parse_config(object(root, Location::Root, "config")?)?;
        let metadata = Self {
            keyslots: named(root, "keyslots", Location::Keyslot, parse_keyslot)?,
            digests: named(root, "digests", Location::Digest, parse_digest)?,
            segments: named(root, "segments", Location::Segment, parse_segment)?,
            tokens: named(root, "tokens", Location::Token, parse_token)?,
            config,
        };
        metadata.check(hdr_size)?;
        Ok(metadata)
    }

    /// The digest that checks `keyslot`'s key, if any. When more than one digest names the
    /// keyslot, one bound to a segment is preferred, so an extra unbound digest cannot hide a
    /// keyslot from [`Luks2::unlock`](crate::luks2::Luks2::unlock).
    pub fn digest_of(&self, keyslot: u32) -> Option<&Digest> {
        let mut naming = self
            .digests
            .values()
            .filter(|d| d.keyslots.contains(&keyslot));
        let first = naming.next()?;
        if !first.segments.is_empty() {
            return Some(first);
        }
        Some(naming.find(|d| !d.segments.is_empty()).unwrap_or(first))
    }

    fn check(&self, hdr_size: u64) -> Result<(), MetadataError> {
        let json_size = hdr_size
            .checked_sub(BINARY_HEADER_LEN as u64)
            .ok_or(MetadataError::HeaderSize(hdr_size))?;
        if self.config.json_size != json_size {
            return Err(MetadataError::JsonSize {
                header: json_size,
                config: self.config.json_size,
            });
        }
        if !self.config.keyslots_size.is_multiple_of(4096) {
            return Err(MetadataError::KeyslotsSize(self.config.keyslots_size));
        }
        // The keyslots area follows both header copies (section 2, figure 1).
        let start = 2 * hdr_size;
        let end = start
            .checked_add(self.config.keyslots_size)
            .ok_or(MetadataError::KeyslotsSize(self.config.keyslots_size))?;
        for (&id, keyslot) in &self.keyslots {
            let (offset, size) = match keyslot {
                Keyslot::Luks2(k) => (k.area.offset, k.area.size),
                Keyslot::Reencrypt { area } => (area.offset, area.size),
                Keyslot::Other { .. } => continue,
            };
            if offset < start || offset.checked_add(size).is_none_or(|e| e > end) {
                return Err(MetadataError::AreaOutside { keyslot: id });
            }
            if let Keyslot::Luks2(k) = keyslot {
                if self.digest_of(id).is_none() {
                    return Err(MetadataError::NoDigest { keyslot: id });
                }
                // Key material is read and decrypted in whole 512-byte sectors.
                let material = (u64::from(k.key_size) * u64::from(k.af.stripes))
                    .next_multiple_of(KEY_MATERIAL_SECTOR as u64);
                if material > size {
                    return Err(MetadataError::AreaTooSmall { keyslot: id });
                }
            }
        }
        for (&id, digest) in &self.digests {
            if let Some(&keyslot) = digest
                .keyslots
                .iter()
                .find(|k| !self.keyslots.contains_key(k))
            {
                return Err(MetadataError::UnknownKeyslot {
                    at: Location::Digest(id),
                    keyslot,
                });
            }
            if let Some(&segment) = digest
                .segments
                .iter()
                .find(|s| !self.segments.contains_key(s))
            {
                return Err(MetadataError::UnknownSegment {
                    at: Location::Digest(id),
                    segment,
                });
            }
        }
        for (&id, token) in &self.tokens {
            if let Some(&keyslot) = token
                .keyslots
                .iter()
                .find(|k| !self.keyslots.contains_key(k))
            {
                return Err(MetadataError::UnknownKeyslot {
                    at: Location::Token(id),
                    keyslot,
                });
            }
        }
        Ok(())
    }
}

fn named<T>(
    root: &Object,
    field: &'static str,
    at: fn(u32) -> Location,
    parse: fn(&Object, u32) -> Result<T, MetadataError>,
) -> Result<BTreeMap<u32, T>, MetadataError> {
    let mut out = BTreeMap::new();
    for (name, value) in object(root, Location::Root, field)? {
        let id = decimal(name).ok_or(MetadataError::ObjectId { field })?;
        let parsed = parse(as_object(value, at(id), field)?, id)?;
        if out.insert(id, parsed).is_some() {
            return Err(MetadataError::ObjectId { field });
        }
    }
    Ok(out)
}

fn parse_keyslot(o: &Object, id: u32) -> Result<Keyslot, MetadataError> {
    let at = Location::Keyslot(id);
    match string(o, at, "type")? {
        "luks2" => parse_luks2_keyslot(o, id).map(Keyslot::Luks2),
        "reencrypt" => {
            let area = object(o, at, "area")?;
            Ok(Keyslot::Reencrypt {
                area: Area {
                    kind: string(area, at, "type")?.into(),
                    offset: uint64_string(area, at, "offset")?,
                    size: uint64_string(area, at, "size")?,
                },
            })
        }
        other => Ok(Keyslot::Other { kind: other.into() }),
    }
}

fn parse_luks2_keyslot(o: &Object, id: u32) -> Result<Luks2Keyslot, MetadataError> {
    let at = Location::Keyslot(id);
    let key_size = key_size_of(o, at, id)?;
    let priority = match o.get("priority") {
        None => Priority::Normal,
        Some(_) => match uint(o, at, "priority")? {
            0 => Priority::Ignore,
            1 => Priority::Normal,
            2 => Priority::High,
            _ => {
                return Err(MetadataError::Integer {
                    at,
                    field: "priority",
                });
            }
        },
    };
    let area = object(o, at, "area")?;
    if string(area, at, "type")? != "raw" {
        return Err(MetadataError::AreaType { keyslot: id });
    }
    let area = RawArea {
        offset: uint64_string(area, at, "offset")?,
        size: uint64_string(area, at, "size")?,
        encryption: string(area, at, "encryption")?.into(),
        key_size: key_size_of(area, at, id)?,
    };
    let af = object(o, at, "af")?;
    if string(af, at, "type")? != "luks1" {
        return Err(MetadataError::AfType { keyslot: id });
    }
    let stripes = uint(af, at, "stripes")?;
    if stripes != AF_STRIPES {
        return Err(MetadataError::Stripes {
            keyslot: id,
            stripes,
        });
    }
    let af = Af {
        stripes,
        hash: HashAlg::from_name(string(af, at, "hash")?),
    };
    Ok(Luks2Keyslot {
        key_size,
        priority,
        area,
        kdf: parse_kdf(object(o, at, "kdf")?, at)?,
        af,
    })
}

fn key_size_of(o: &Object, at: Location, id: u32) -> Result<u32, MetadataError> {
    match uint(o, at, "key_size")? {
        0 => Err(MetadataError::KeySize { keyslot: id }),
        size if size > MAX_KEY_SIZE => Err(MetadataError::KeySize { keyslot: id }),
        size => Ok(size),
    }
}

fn parse_kdf(o: &Object, at: Location) -> Result<Kdf, MetadataError> {
    let variant = match string(o, at, "type")? {
        "pbkdf2" => {
            return Ok(Kdf::Pbkdf2 {
                hash: HashAlg::from_name(string(o, at, "hash")?),
                iterations: nonzero(o, at, "iterations")?,
                salt: base64(o, at, "salt")?,
            });
        }
        "argon2i" => Argon2Variant::I,
        "argon2id" => Argon2Variant::Id,
        other => return Ok(Kdf::Other(other.into())),
    };
    Ok(Kdf::Argon2 {
        variant,
        time: nonzero(o, at, "time")?,
        memory_kib: nonzero(o, at, "memory")?,
        cpus: nonzero(o, at, "cpus")?,
        salt: base64(o, at, "salt")?,
    })
}

fn parse_digest(o: &Object, id: u32) -> Result<Digest, MetadataError> {
    let at = Location::Digest(id);
    let kind = match string(o, at, "type")? {
        "pbkdf2" => {
            let hash = HashAlg::from_name(string(o, at, "hash")?);
            let digest = base64(o, at, "digest")?;
            // PBKDF2's work grows with its output length, one block of `iterations` per
            // hash-output-sized chunk, so a long digest would multiply the work past what
            // `Limits` allow. Allow at most one block; LUKS1-style 20-byte digests stay valid.
            if digest.is_empty() || hash.output_len().is_some_and(|max| digest.len() > max) {
                return Err(MetadataError::DigestLength { digest: id });
            }
            DigestKind::Pbkdf2 {
                hash,
                iterations: nonzero(o, at, "iterations")?,
                salt: base64(o, at, "salt")?,
                digest,
            }
        }
        other => DigestKind::Other(other.into()),
    };
    Ok(Digest {
        keyslots: ids(o, at, "keyslots")?,
        segments: ids(o, at, "segments")?,
        kind,
    })
}

fn parse_segment(o: &Object, id: u32) -> Result<Segment, MetadataError> {
    let at = Location::Segment(id);
    let size = match field(o, at, "size")? {
        Value::String(s) if s == "dynamic" => SegmentSize::Dynamic,
        _ => SegmentSize::Bytes(uint64_string(o, at, "size")?),
    };
    let kind = match string(o, at, "type")? {
        "crypt" => {
            let sector_size = uint(o, at, "sector_size")?;
            if !SECTOR_SIZES.contains(&sector_size) {
                return Err(MetadataError::SectorSize {
                    segment: id,
                    size: sector_size,
                });
            }
            let integrity = match o.get("integrity") {
                None => None,
                Some(_) => Some(string(object(o, at, "integrity")?, at, "type")?.into()),
            };
            SegmentKind::Crypt {
                iv_tweak: uint64_string(o, at, "iv_tweak")?,
                encryption: string(o, at, "encryption")?.into(),
                sector_size,
                integrity,
            }
        }
        "linear" => SegmentKind::Linear,
        other => SegmentKind::Other(other.into()),
    };
    Ok(Segment {
        offset: uint64_string(o, at, "offset")?,
        size,
        flags: optional_strings(o, at, "flags")?,
        kind,
    })
}

fn parse_token(o: &Object, id: u32) -> Result<Token, MetadataError> {
    let at = Location::Token(id);
    Ok(Token {
        kind: string(o, at, "type")?.into(),
        keyslots: ids(o, at, "keyslots")?,
    })
}

fn parse_config(o: &Object) -> Result<Config, MetadataError> {
    let at = Location::Config;
    // The specification describes `requirements` as an array; headers written by the reference
    // implementation hold it as an object with a `mandatory` array. Both are accepted.
    let requirements = match o.get("requirements") {
        Some(Value::Object(r)) => optional_strings(r, at, "mandatory")?,
        _ => optional_strings(o, at, "requirements")?,
    };
    Ok(Config {
        json_size: uint64_string(o, at, "json_size")?,
        keyslots_size: uint64_string(o, at, "keyslots_size")?,
        flags: optional_strings(o, at, "flags")?,
        requirements,
    })
}

/// A decimal object name or reference in canonical form: digits only, and no leading zero
/// unless the number is 0, so that two different names never mean the same object.
fn decimal(text: &str) -> Option<u32> {
    if text.is_empty()
        || !text.bytes().all(|b| b.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

fn field<'a>(o: &'a Object, at: Location, name: &'static str) -> Result<&'a Value, MetadataError> {
    o.get(name)
        .ok_or(MetadataError::Missing { at, field: name })
}

fn as_object<'a>(
    v: &'a Value,
    at: Location,
    name: &'static str,
) -> Result<&'a Object, MetadataError> {
    v.as_object().ok_or(MetadataError::Type { at, field: name })
}

fn object<'a>(
    o: &'a Object,
    at: Location,
    name: &'static str,
) -> Result<&'a Object, MetadataError> {
    as_object(field(o, at, name)?, at, name)
}

fn string<'a>(o: &'a Object, at: Location, name: &'static str) -> Result<&'a str, MetadataError> {
    field(o, at, name)?
        .as_str()
        .ok_or(MetadataError::Type { at, field: name })
}

fn uint(o: &Object, at: Location, name: &'static str) -> Result<u32, MetadataError> {
    let value = field(o, at, name)?
        .as_u64()
        .ok_or(MetadataError::Type { at, field: name })?;
    u32::try_from(value).map_err(|_| MetadataError::Integer { at, field: name })
}

fn nonzero(o: &Object, at: Location, name: &'static str) -> Result<u32, MetadataError> {
    match uint(o, at, name)? {
        0 => Err(MetadataError::Integer { at, field: name }),
        value => Ok(value),
    }
}

fn uint64_string(o: &Object, at: Location, name: &'static str) -> Result<u64, MetadataError> {
    let text = string(o, at, name)?;
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(MetadataError::Integer { at, field: name });
    }
    text.parse()
        .map_err(|_| MetadataError::Integer { at, field: name })
}

fn base64(o: &Object, at: Location, name: &'static str) -> Result<Vec<u8>, MetadataError> {
    Base64::decode_vec(string(o, at, name)?).map_err(|_| MetadataError::Base64 { at, field: name })
}

fn strings<'a>(
    o: &'a Object,
    at: Location,
    name: &'static str,
) -> Result<impl Iterator<Item = Result<&'a str, MetadataError>>, MetadataError> {
    let array = field(o, at, name)?
        .as_array()
        .ok_or(MetadataError::Type { at, field: name })?;
    Ok(array
        .iter()
        .map(move |v| v.as_str().ok_or(MetadataError::Type { at, field: name })))
}

fn optional_strings(
    o: &Object,
    at: Location,
    name: &'static str,
) -> Result<Vec<String>, MetadataError> {
    if !o.contains_key(name) {
        return Ok(Vec::new());
    }
    strings(o, at, name)?.map(|s| s.map(String::from)).collect()
}

fn ids(o: &Object, at: Location, name: &'static str) -> Result<Vec<u32>, MetadataError> {
    strings(o, at, name)?
        .map(|s| decimal(s?).ok_or(MetadataError::Integer { at, field: name }))
        .collect()
}

/// LUKS2 JSON metadata that is not valid.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MetadataError {
    /// The JSON area has no terminating NUL.
    #[error("the LUKS2 JSON area is not NUL-terminated")]
    NotTerminated,
    /// The JSON does not parse.
    #[error("the LUKS2 JSON area does not parse")]
    Json(#[source] serde_json::Error),
    /// A mandatory member is missing.
    #[error("LUKS2 metadata {at:?} has no {field}")]
    Missing {
        /// The object.
        at: Location,
        /// The member.
        field: &'static str,
    },
    /// A member has the wrong JSON type.
    #[error("LUKS2 metadata {at:?} member {field} has the wrong type")]
    Type {
        /// The object.
        at: Location,
        /// The member.
        field: &'static str,
    },
    /// An integer is out of range, or a string-uint64 or object reference is not a decimal
    /// number.
    #[error("LUKS2 metadata {at:?} member {field} is not a valid number")]
    Integer {
        /// The object.
        at: Location,
        /// The member.
        field: &'static str,
    },
    /// A Base64 value does not decode.
    #[error("LUKS2 metadata {at:?} member {field} is not valid Base64")]
    Base64 {
        /// The object.
        at: Location,
        /// The member.
        field: &'static str,
    },
    /// An object inside `keyslots`, `digests`, `segments` or `tokens` is not named by a decimal
    /// number.
    #[error("an object in LUKS2 {field} is not named by a decimal number")]
    ObjectId {
        /// The top-level member holding it.
        field: &'static str,
    },
    /// `config.json_size` does not match the binary header.
    #[error("LUKS2 config json_size {config} does not match the header's {header}")]
    JsonSize {
        /// The size from the binary header.
        header: u64,
        /// The size in `config`.
        config: u64,
    },
    /// `config.keyslots_size` is not a multiple of 4096.
    #[error("LUKS2 keyslots_size {0} is not a multiple of 4096")]
    KeyslotsSize(u64),
    /// A `luks2` keyslot's area is not of type `raw`.
    #[error("LUKS2 keyslot {keyslot} does not have a raw area")]
    AreaType {
        /// The keyslot.
        keyslot: u32,
    },
    /// A `luks2` keyslot's anti-forensic splitter is not of type `luks1`.
    #[error("LUKS2 keyslot {keyslot} does not use the luks1 AF splitter")]
    AfType {
        /// The keyslot.
        keyslot: u32,
    },
    /// A keyslot's stripe count is not [`AF_STRIPES`].
    #[error("LUKS2 keyslot {keyslot} has {stripes} AF stripes, not 4000")]
    Stripes {
        /// The keyslot.
        keyslot: u32,
        /// The stripe count.
        stripes: u32,
    },
    /// A keyslot's key size, or its area's key size, is zero or above [`MAX_KEY_SIZE`].
    #[error("LUKS2 keyslot {keyslot} has an unsupported key size")]
    KeySize {
        /// The keyslot.
        keyslot: u32,
    },
    /// A keyslot's area is not inside the keyslots area.
    #[error("LUKS2 keyslot {keyslot}'s area is outside the keyslots area")]
    AreaOutside {
        /// The keyslot.
        keyslot: u32,
    },
    /// A keyslot's area is smaller than its split key material.
    #[error("LUKS2 keyslot {keyslot}'s area is too small for its key material")]
    AreaTooSmall {
        /// The keyslot.
        keyslot: u32,
    },
    /// A `luks2` keyslot has no digest.
    #[error("LUKS2 keyslot {keyslot} has no digest")]
    NoDigest {
        /// The keyslot.
        keyslot: u32,
    },
    /// An object names a keyslot that does not exist.
    #[error("LUKS2 metadata {at:?} names keyslot {keyslot}, which does not exist")]
    UnknownKeyslot {
        /// The naming object.
        at: Location,
        /// The keyslot named.
        keyslot: u32,
    },
    /// A digest names a segment that does not exist.
    #[error("LUKS2 metadata {at:?} names segment {segment}, which does not exist")]
    UnknownSegment {
        /// The naming object.
        at: Location,
        /// The segment named.
        segment: u32,
    },
    /// The `hdr_size` given is smaller than the binary header.
    #[error("LUKS2 hdr_size {0} is smaller than the binary header")]
    HeaderSize(u64),
    /// A `pbkdf2` digest is empty or longer than its hash's output.
    #[error("LUKS2 digest {digest} is empty or longer than its hash output")]
    DigestLength {
        /// The digest.
        digest: u32,
    },
    /// A crypt segment's sector size is not 512, 1024, 2048 or 4096.
    #[error("LUKS2 segment {segment} has sector size {size}")]
    SectorSize {
        /// The segment.
        segment: u32,
        /// The sector size.
        size: u32,
    },
}

#[cfg(test)]
pub(crate) mod tests {
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;

    use super::*;

    const HDR: u64 = 0x4000;

    /// The JSON of a valid single-keyslot header with a 16 KiB header size, as `cryptsetup`
    /// writes it, for tests to edit.
    pub(crate) fn json() -> Value {
        serde_json::json!({
            "keyslots": {"0": {
                "type": "luks2", "key_size": 32,
                "af": {"type": "luks1", "stripes": 4000, "hash": "sha256"},
                "area": {"type": "raw", "offset": "32768", "size": "131072",
                         "encryption": "aes-xts-plain64", "key_size": 32},
                "kdf": {"type": "argon2id", "time": 4, "memory": 32, "cpus": 1,
                        "salt": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}},
            "tokens": {},
            "segments": {"0": {"type": "crypt", "offset": "1048576", "size": "dynamic",
                               "iv_tweak": "0", "encryption": "aes-xts-plain64",
                               "sector_size": 4096}},
            "digests": {"0": {"type": "pbkdf2", "keyslots": ["0"], "segments": ["0"],
                              "hash": "sha256", "iterations": 1000,
                              "salt": "AAAA", "digest": "AAAA"}},
            "config": {"json_size": "12288", "keyslots_size": "262144"}
        })
    }

    pub(crate) fn area(json: &Value) -> Vec<u8> {
        let mut bytes = json.to_string().into_bytes();
        bytes.push(0);
        bytes
    }

    fn parse(json: &Value) -> Result<Metadata, MetadataError> {
        Metadata::parse(&area(json), HDR)
    }

    #[test]
    fn a_cryptsetup_style_header_parses() {
        let m = parse(&json()).expect("valid");
        let Keyslot::Luks2(k) = &m.keyslots[&0] else {
            panic!("luks2 keyslot");
        };
        assert_eq!(k.key_size, 32);
        assert_eq!(k.priority, Priority::Normal);
        assert_eq!(k.area.offset, 32768);
        assert_eq!(k.area.encryption, "aes-xts-plain64");
        assert_eq!(
            k.af,
            Af {
                stripes: 4000,
                hash: HashAlg::Sha256
            }
        );
        assert!(matches!(
            k.kdf,
            Kdf::Argon2 {
                variant: Argon2Variant::Id,
                time: 4,
                memory_kib: 32,
                cpus: 1,
                ..
            }
        ));
        assert_eq!(m.segments[&0].size, SegmentSize::Dynamic);
        assert!(matches!(
            m.segments[&0].kind,
            SegmentKind::Crypt {
                sector_size: 4096,
                iv_tweak: 0,
                integrity: None,
                ..
            }
        ));
        assert_eq!(m.digest_of(0).map(|d| d.segments.clone()), Some(vec![0]));
        assert_eq!(m.config.json_size, 12288);
        assert!(m.config.requirements.is_empty());
    }

    #[test]
    fn unknown_types_are_kept() {
        let mut j = json();
        j["keyslots"]["1"] = serde_json::json!({"type": "future"});
        j["segments"]["1"] = serde_json::json!({"type": "hw-opal", "offset": "0", "size": "4096"});
        j["tokens"]["0"] = serde_json::json!({"type": "luks2-keyring", "keyslots": ["0"],
                                               "key_description": "x"});
        j["config"]["requirements"] = serde_json::json!({"mandatory": ["online-reencrypt-v2"]});
        let m = parse(&j).expect("valid");
        assert_eq!(
            m.keyslots[&1],
            Keyslot::Other {
                kind: "future".into()
            }
        );
        assert_eq!(m.segments[&1].kind, SegmentKind::Other("hw-opal".into()));
        assert_eq!(m.tokens[&0].keyslots, vec![0]);
        assert_eq!(
            m.config.requirements,
            vec![String::from("online-reencrypt-v2")]
        );
    }

    #[test]
    fn invalid_metadata_is_named() {
        type Edit = fn(&mut Value);
        let cases: [(Edit, &str); 23] = [
            (
                |j| {
                    j.as_object_mut().unwrap().remove("tokens");
                },
                "Missing",
            ),
            (|j| j["keyslots"]["0"]["key_size"] = "32".into(), "Type"),
            (
                |j| j["keyslots"]["0"]["area"]["offset"] = "+32768".into(),
                "Integer",
            ),
            (
                |j| j["keyslots"]["0"]["kdf"]["salt"] = "!!".into(),
                "Base64",
            ),
            (|j| j["keyslots"]["x"] = serde_json::json!({}), "ObjectId"),
            (|j| j["config"]["json_size"] = "4096".into(), "JsonSize"),
            (
                |j| j["config"]["keyslots_size"] = "1000".into(),
                "KeyslotsSize",
            ),
            (
                |j| j["keyslots"]["0"]["area"]["type"] = "none".into(),
                "AreaType",
            ),
            (
                |j| j["keyslots"]["0"]["af"]["type"] = "luks9".into(),
                "AfType",
            ),
            (
                |j| j["keyslots"]["0"]["af"]["stripes"] = 1.into(),
                "Stripes",
            ),
            (|j| j["keyslots"]["0"]["key_size"] = 0.into(), "KeySize"),
            (
                |j| j["keyslots"]["0"]["area"]["offset"] = "16384".into(),
                "AreaOutside",
            ),
            (
                |j| j["keyslots"]["0"]["area"]["size"] = "4096".into(),
                "AreaTooSmall",
            ),
            (
                |j| j["digests"]["0"]["keyslots"] = serde_json::json!([]),
                "NoDigest",
            ),
            (
                |j| j["digests"]["0"]["keyslots"] = serde_json::json!(["0", "7"]),
                "UnknownKeyslot",
            ),
            (
                |j| j["digests"]["0"]["segments"] = serde_json::json!(["3"]),
                "UnknownSegment",
            ),
            (
                |j| j["segments"]["0"]["sector_size"] = 768.into(),
                "SectorSize",
            ),
            (|j| j["keyslots"]["0"]["kdf"]["time"] = 0.into(), "Integer"),
            (
                |j| j["keyslots"]["00"] = j["keyslots"]["0"].clone(),
                "ObjectId",
            ),
            (
                |j| j["digests"]["0"]["keyslots"] = serde_json::json!(["00"]),
                "Integer",
            ),
            (
                // 33 bytes for SHA-256: more than one PBKDF2 block.
                |j| j["digests"]["0"]["digest"] = "A".repeat(44).into(),
                "DigestLength",
            ),
            (|j| j["digests"]["0"]["digest"] = "".into(), "DigestLength"),
            (
                // Key size 1: 4000 bytes of material, read as 4096.
                |j| {
                    j["keyslots"]["0"]["key_size"] = 1.into();
                    j["keyslots"]["0"]["area"]["size"] = "4000".into();
                },
                "AreaTooSmall",
            ),
        ];
        for (edit, variant) in cases {
            let mut j = json();
            edit(&mut j);
            let error = parse(&j).expect_err(variant);
            assert!(
                format!("{error:?}").starts_with(variant),
                "{variant}: {error:?}"
            );
        }
        assert!(matches!(
            Metadata::parse(b"{}", HDR),
            Err(MetadataError::NotTerminated)
        ));
        assert!(matches!(
            Metadata::parse(&area(&json()), 100),
            Err(MetadataError::HeaderSize(100))
        ));
        assert!(matches!(
            Metadata::parse(b"{\0", HDR),
            Err(MetadataError::Json(_))
        ));
    }

    #[test]
    fn a_bound_digest_is_preferred() {
        let mut j = json();
        j["digests"]["1"] = j["digests"]["0"].clone();
        j["digests"]["0"]["segments"] = serde_json::json!([]);
        let m = parse(&j).expect("valid");
        assert_eq!(m.digest_of(0).map(|d| d.segments.clone()), Some(vec![0]));
        j["digests"]["1"]["segments"] = serde_json::json!([]);
        let m = parse(&j).expect("valid");
        assert_eq!(m.digest_of(0).map(|d| d.segments.len()), Some(0));
    }

    #[test]
    fn errors_describe_themselves() {
        let error = MetadataError::AreaOutside { keyslot: 3 };
        assert_eq!(
            error.to_string(),
            "LUKS2 keyslot 3's area is outside the keyslots area"
        );
    }
}
