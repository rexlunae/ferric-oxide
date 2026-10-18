//! Fetching a repository into a local bare cache.
//!
//! [`fetch`] keeps one bare repository per source, mirroring the remote's
//! branches and tags (`+refs/heads/*:refs/heads/*` and
//! `+refs/tags/*:refs/tags/*`), so later builds of the same source only
//! fetch what changed. A moved tag is fetched like any other change; the
//! release-tag checks in [`crate::tag`] decide whether it is accepted. Tags
//! the remote no longer has are deleted from the cache, so a withdrawn
//! release is not picked as the newest one. The cache's `HEAD` follows the
//! branch the remote's `HEAD` names, so a changed default branch is picked
//! up by the next fetch.
//!
//! `file://`, `ssh://` (through the `ssh` program) and `git://` URLs work
//! everywhere. `https://` and `http://` need the HTTP transport, which is
//! built on every target except Redox.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use gix::remote::Direction;

use crate::{GitError, Repository};

/// The refspecs a cache mirrors.
const REFSPECS: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];

/// Also asked for when updating a cache, so the remote advertises which
/// branch its `HEAD` names. It writes no local ref.
const HEAD_REFSPEC: &str = "HEAD";

/// Fetches `url` into the bare repository `cache`, cloning it there first
/// if it does not exist, and returns the cache.
///
/// # Errors
///
/// [`GitError::Fetch`] if the clone or fetch fails (including for an
/// unsupported scheme), [`GitError::Open`] if `cache` exists but is not a
/// repository, and [`GitError::Export`] if the cache directory cannot be
/// prepared.
pub fn fetch(url: &str, cache: &Path) -> Result<Repository, GitError> {
    install_tls_provider();
    let fetch_error = |source| GitError::Fetch {
        url: url.to_owned(),
        source: Box::new(source),
    };
    let interrupt = AtomicBool::new(false);
    if cache.exists() {
        let repo = Repository::open(cache)?;
        let remote = repo
            .inner
            .remote_at(url)
            .and_then(|remote| {
                remote.with_refspecs(
                    REFSPECS.iter().copied().chain([HEAD_REFSPEC]),
                    Direction::Fetch,
                )
            })
            .map_err(fetch_error)?;
        let outcome = remote
            .connect(Direction::Fetch)
            .and_then(|connection| {
                connection.prepare_fetch(gix::progress::Discard, Default::default())
            })
            .and_then(|prepare| prepare.receive(gix::progress::Discard, &interrupt))
            .map_err(fetch_error)?;
        prune_tags(&repo, &outcome.ref_map.remote_refs).map_err(fetch_error)?;
        follow_remote_head(&repo, &outcome.ref_map.remote_refs).map_err(fetch_error)?;
        return Ok(repo);
    }

    // Clone into a sibling directory and rename it into place, so an
    // interrupted clone never leaves a half-filled cache behind.
    let partial = cache.with_extension("partial");
    remove_partial(&partial)?;
    if let Some(parent) = partial.parent() {
        fs::create_dir_all(parent).map_err(|source| GitError::Export {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let (repo, _) = gix::prepare_clone_bare(url, &partial)
        .map(|prepare| {
            prepare.configure_remote(|remote| {
                remote.with_refspecs(REFSPECS[1..].iter().copied(), Direction::Fetch)
            })
        })
        .and_then(|mut prepare| prepare.fetch_only(gix::progress::Discard, &interrupt))
        .map_err(fetch_error)?;
    drop(repo);
    fs::rename(&partial, cache).map_err(|source| GitError::Export {
        path: cache.to_path_buf(),
        source,
    })?;
    Repository::open(cache)
}

/// Deletes the cache's tags that the remote did not advertise.
fn prune_tags(repo: &Repository, remote_refs: &[gix::protocol::handshake::Ref]) -> gix::Result<()> {
    let advertised: Vec<&gix::bstr::BStr> = remote_refs.iter().map(|r| r.unpack().0).collect();
    let mut stale = Vec::new();
    for reference in repo.inner.references()?.tags()? {
        let name = reference?.name().to_owned();
        if !advertised.contains(&name.as_bstr()) {
            stale.push(name);
        }
    }
    for name in stale {
        repo.inner.edit_reference(RefEdit {
            change: Change::Delete {
                expected: PreviousValue::Any,
                log: RefLog::AndReference,
            },
            name,
            deref: false,
        })?;
    }
    Ok(())
}

/// Points the cache's `HEAD` at the branch the remote's `HEAD` names, if
/// the remote said and the cache has that branch.
fn follow_remote_head(
    repo: &Repository,
    remote_refs: &[gix::protocol::handshake::Ref],
) -> gix::Result<()> {
    use gix::protocol::handshake::Ref;
    let target = remote_refs.iter().find_map(|reference| match reference {
        Ref::Symbolic {
            full_ref_name,
            target,
            ..
        }
        | Ref::Unborn {
            full_ref_name,
            target,
        } if full_ref_name == "HEAD" => Some(target.clone()),
        _ => None,
    });
    let Some(target) = target else {
        return Ok(());
    };
    let Ok(target) = gix::refs::FullName::try_from(target) else {
        return Ok(());
    };
    if repo.inner.try_find_reference(target.as_ref())?.is_none() {
        return Ok(());
    }
    repo.inner.edit_reference(RefEdit {
        change: Change::Update {
            log: gix::refs::transaction::LogChange::default(),
            expected: PreviousValue::Any,
            new: gix::refs::Target::Symbolic(target),
        },
        name: "HEAD".try_into().expect("HEAD is a valid reference name"),
        deref: false,
    })?;
    Ok(())
}

fn remove_partial(partial: &Path) -> Result<(), GitError> {
    match fs::remove_dir_all(partial) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(GitError::Export {
            path: partial.to_path_buf(),
            source: error,
        }),
        _ => Ok(()),
    }
}

/// rustls is built without a default crypto provider (see Cargo.toml), so
/// the HTTP transport needs ring installed as the process default.
#[cfg(not(target_os = "redox"))]
pub(crate) fn install_tls_provider() {
    // An error means a provider is already installed, which is all this
    // needs.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(target_os = "redox")]
pub(crate) fn install_tls_provider() {}
