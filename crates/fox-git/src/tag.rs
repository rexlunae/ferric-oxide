//! Release tags and their SSH signatures.
//!
//! A fox release is an annotated tag named after its version (`v1.2.3` or
//! `1.2.3`) whose object is signed the way `git tag -s` signs it with
//! `gpg.format = ssh`: an OpenSSH signature (the SSHSIG format of
//! OpenSSH's `PROTOCOL.sshsig`, namespace `git`) of the tag object's bytes
//! up to the signature, appended to the object as an armored
//! `-----BEGIN SSH SIGNATURE-----` block. Git's own split rule is used: the
//! signature starts at the last line that begins a signature block.
//!
//! [`Repository::tag`] also checks that the object's `tag` header names the
//! ref it was found under, so a signed tag for one version cannot be
//! replayed under another version's name, and that it points at a commit.
//! PGP and X.509 signatures are recognized but not verified.

use fox_core::trust::AuthorKey;
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::objs::TagRef;
use gix::refs::transaction::PreviousValue;
use semver::Version;
use ssh_key::{HashAlg, LineEnding, PrivateKey, SshSig};

use crate::{Commit, GitError, Repository};

/// The SSHSIG namespace git signs with.
pub const GIT_NAMESPACE: &str = "git";

/// First lines of the signature blocks git recognizes, and their formats.
const SIGNATURE_STARTS: [(&[u8], SignatureFormat); 4] = [
    (b"-----BEGIN SSH SIGNATURE-----", SignatureFormat::Ssh),
    (b"-----BEGIN PGP SIGNATURE-----", SignatureFormat::Pgp),
    (b"-----BEGIN PGP MESSAGE-----", SignatureFormat::Pgp),
    (b"-----BEGIN SIGNED MESSAGE-----", SignatureFormat::X509),
];

/// The format of a tag's signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SignatureFormat {
    /// An OpenSSH signature: the only format verified.
    Ssh,
    /// An OpenPGP signature.
    Pgp,
    /// An X.509 (S/MIME) signature.
    X509,
}

/// How a tag is signed.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TagSigner {
    /// A lightweight tag, or an annotated tag without a signature.
    Unsigned,
    /// An SSH signature by this Ed25519 key. The signature is not yet
    /// checked; see [`Tag::verify`].
    Ssh(AuthorKey),
    /// A signature in a format that is not verified.
    Unsupported(SignatureFormat),
}

/// A tag and the commit it names.
#[derive(Debug, Clone)]
pub struct Tag {
    name: String,
    commit: Commit,
    signed: Option<SignedTag>,
    unsupported: Option<SignatureFormat>,
}

/// The parts of an SSH-signed tag object.
#[derive(Debug, Clone)]
struct SignedTag {
    payload: Vec<u8>,
    signature: SshSig,
    key: AuthorKey,
}

impl Tag {
    /// The tag name, without `refs/tags/`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The commit it names.
    pub fn commit(&self) -> &Commit {
        &self.commit
    }

    /// The version the name gives (`v1.2.3` or `1.2.3`), if it is one.
    pub fn version(&self) -> Option<Version> {
        tag_version(&self.name)
    }

    /// Who signed the tag, according to the signature itself.
    pub fn signer(&self) -> TagSigner {
        match (&self.signed, self.unsupported) {
            (Some(signed), _) => TagSigner::Ssh(signed.key.clone()),
            (None, Some(format)) => TagSigner::Unsupported(format),
            (None, None) => TagSigner::Unsigned,
        }
    }

    /// Checks that `key` made the tag's SSH signature, in namespace
    /// [`GIT_NAMESPACE`], over the tag object.
    ///
    /// # Errors
    ///
    /// [`GitError::TagUnsigned`] if the tag has no SSH signature and
    /// [`GitError::TagSignature`] if it is not a valid signature by `key`.
    pub fn verify(&self, key: &AuthorKey) -> Result<(), GitError> {
        let signed = self.signed.as_ref().ok_or_else(|| GitError::TagUnsigned {
            tag: self.name.clone(),
        })?;
        key.public_key()
            .verify(GIT_NAMESPACE, &signed.payload, &signed.signature)
            .map_err(|source| GitError::TagSignature {
                tag: self.name.clone(),
                source,
            })
    }
}

/// The version a tag name gives: `1.2.3` or `v1.2.3`, in semver syntax.
pub fn tag_version(name: &str) -> Option<Version> {
    Version::parse(name.strip_prefix('v').unwrap_or(name)).ok()
}

/// Splits a tag object into the signed payload and the signature block,
/// at the last line that starts a signature.
fn split_signature(data: &[u8]) -> Option<(&[u8], &[u8], SignatureFormat)> {
    let mut found = None;
    let mut line_start = 0;
    for line in data.split_inclusive(|&b| b == b'\n') {
        if let Some((_, format)) = SIGNATURE_STARTS
            .iter()
            .find(|(start, _)| line.starts_with(start))
        {
            found = Some((line_start, *format));
        }
        line_start += line.len();
    }
    found.map(|(at, format)| (&data[..at], &data[at..], format))
}

impl Repository {
    /// Reads tag `name` (without `refs/tags/`) and the commit it names.
    ///
    /// # Errors
    ///
    /// [`GitError::Resolve`] if there is no such tag or it does not lead to
    /// a commit, [`GitError::Object`] if an object cannot be read,
    /// [`GitError::TagName`] if the tag object names a different tag,
    /// [`GitError::TagTarget`] if it does not point at a commit, and
    /// [`GitError::TagSignatureFormat`] or [`GitError::TagKey`] if its SSH
    /// signature block cannot be decoded or is not by an Ed25519 key.
    pub fn tag(&self, name: &str) -> Result<Tag, GitError> {
        let full = format!("refs/tags/{name}");
        let commit = self.resolve(&full)?;
        let resolve_error = |source| GitError::Resolve {
            rev: full.clone(),
            source: Box::new(source),
        };
        let reference = self
            .inner
            .find_reference(full.as_str())
            .map_err(resolve_error)?;
        let Some(id) = reference.target().try_id().map(ToOwned::to_owned) else {
            return Ok(unsigned(name, commit));
        };
        let object = self.find(id)?;
        if object.kind != gix::object::Kind::Tag {
            // A lightweight tag.
            return Ok(unsigned(name, commit));
        }
        let data = object.detach().data;
        self.check_tag_header(id, name, &data, &commit)?;
        let Some((payload, block, format)) = split_signature(&data) else {
            return Ok(unsigned(name, commit));
        };
        if format != SignatureFormat::Ssh {
            return Ok(Tag {
                name: name.to_owned(),
                commit,
                signed: None,
                unsupported: Some(format),
            });
        }
        let signature = SshSig::from_pem(block).map_err(|source| GitError::TagSignatureFormat {
            tag: name.to_owned(),
            source,
        })?;
        let key = ssh_key::PublicKey::from(signature.public_key().clone());
        let key = AuthorKey::new(key).map_err(|source| GitError::TagKey {
            tag: name.to_owned(),
            source,
        })?;
        Ok(Tag {
            name: name.to_owned(),
            commit,
            signed: Some(SignedTag {
                payload: payload.to_vec(),
                signature,
                key,
            }),
            unsupported: None,
        })
    }

    /// Checks that a tag object names `name` and points at `commit`
    /// directly.
    fn check_tag_header(
        &self,
        id: ObjectId,
        name: &str,
        data: &[u8],
        commit: &Commit,
    ) -> Result<(), GitError> {
        let tag = TagRef::from_bytes(data, self.inner.object_hash()).map_err(|source| {
            GitError::Object {
                id,
                source: Box::new(source),
            }
        })?;
        if tag.name != name.as_bytes() {
            return Err(GitError::TagName {
                tag: name.to_owned(),
                object_name: tag.name.to_str_lossy().into_owned(),
            });
        }
        if tag.target_kind != gix::object::Kind::Commit || tag.target() != commit.id {
            return Err(GitError::TagTarget {
                tag: name.to_owned(),
            });
        }
        Ok(())
    }

    /// Every tag whose name is a version (`1.2.3` or `v1.2.3`), oldest
    /// version first. Pre-releases are included.
    ///
    /// # Errors
    ///
    /// [`GitError::References`] if the tags cannot be listed.
    pub fn version_tags(&self) -> Result<Vec<(Version, String)>, GitError> {
        let refs_error = |source| GitError::References {
            source: Box::new(source),
        };
        let platform = self.inner.references().map_err(refs_error)?;
        let mut tags = Vec::new();
        for reference in platform.tags().map_err(refs_error)? {
            let reference = reference.map_err(refs_error)?;
            let name = reference.name().shorten().to_str_lossy().into_owned();
            if let Some(version) = tag_version(&name) {
                tags.push((version, name));
            }
        }
        tags.sort();
        Ok(tags)
    }

    /// The tag of the newest release version, ignoring pre-releases.
    ///
    /// # Errors
    ///
    /// As [`Repository::version_tags`].
    pub fn newest_release_tag(&self) -> Result<Option<String>, GitError> {
        Ok(self
            .version_tags()?
            .into_iter()
            .rev()
            .find(|(version, _)| version.pre.is_empty())
            .map(|(_, name)| name))
    }

    /// Creates annotated tag `name` on `commit`, signed with an OpenSSH
    /// Ed25519 private key the way `git tag -s` signs with
    /// `gpg.format = ssh`. `tagger` is a git identity line
    /// (`Name <email> <seconds> <+hhmm>`, as `git var GIT_COMMITTER_IDENT`
    /// prints it) and `message` the tag message.
    ///
    /// # Errors
    ///
    /// [`GitError::TagField`] if `tagger` or `message` cannot be written into
    /// a tag object, [`GitError::TagKey`] if the key is not Ed25519,
    /// [`GitError::Sign`] if signing fails, [`GitError::Write`] if the
    /// object or the ref cannot be written (including when the tag exists),
    /// and the errors of [`Repository::tag`] reading it back.
    pub fn create_signed_tag(
        &self,
        name: &str,
        commit: &Commit,
        tagger: &str,
        message: &str,
        key: &PrivateKey,
    ) -> Result<Tag, GitError> {
        if tagger.is_empty() || tagger.contains('\n') {
            return Err(GitError::TagField { field: "tagger" });
        }
        AuthorKey::new(key.public_key().clone()).map_err(|source| GitError::TagKey {
            tag: name.to_owned(),
            source,
        })?;
        let mut object = format!(
            "object {}\ntype commit\ntag {name}\ntagger {tagger}\n\n{message}",
            commit.id
        );
        if !object.ends_with('\n') {
            object.push('\n');
        }
        if split_signature(object.as_bytes()).is_some() {
            return Err(GitError::TagField { field: "message" });
        }
        let signature = key
            .sign(GIT_NAMESPACE, HashAlg::Sha512, object.as_bytes())
            .and_then(|signature| signature.to_pem(LineEnding::LF))
            .map_err(|source| GitError::Sign { source })?;
        object.push_str(&signature);
        if !object.ends_with('\n') {
            object.push('\n');
        }
        let write_error = |source| GitError::Write {
            what: format!("tag {name}"),
            source: Box::new(source),
        };
        let id = gix::objs::Write::write_buf(
            &self.inner.objects,
            gix::object::Kind::Tag,
            object.as_bytes(),
        )
        .map_err(write_error)?;
        self.inner
            .tag_reference(name, id, PreviousValue::MustNotExist)
            .map_err(write_error)?;
        self.tag(name)
    }
}

fn unsigned(name: &str, commit: Commit) -> Tag {
    Tag {
        name: name.to_owned(),
        commit,
        signed: None,
        unsupported: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_from_tag_names() {
        assert_eq!(tag_version("v1.2.3"), Some(Version::new(1, 2, 3)));
        assert_eq!(tag_version("1.2.3"), Some(Version::new(1, 2, 3)));
        assert!(tag_version("v1.2.3-rc.1").is_some());
        for name in ["v1.2", "release-1.2.3", "vv1.2.3", ""] {
            assert_eq!(tag_version(name), None, "{name}");
        }
    }

    #[test]
    fn split_at_last_signature_line() {
        let data = b"object x\n\nmsg\n-----BEGIN PGP SIGNATURE-----\nfake\n-----BEGIN SSH SIGNATURE-----\nsig\n";
        let (payload, block, format) = split_signature(data).unwrap();
        assert_eq!(format, SignatureFormat::Ssh);
        assert!(payload.ends_with(b"fake\n"));
        assert!(block.starts_with(b"-----BEGIN SSH SIGNATURE-----\n"));
        // A marker that does not start a line is not a signature.
        assert!(split_signature(b"msg -----BEGIN SSH SIGNATURE-----\n").is_none());
    }
}
