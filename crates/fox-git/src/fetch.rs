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
//! `file://`, `ssh://` (through the `ssh` program) and `git://` URLs use
//! gix's transports. `https://` and `http://` use fox's own HTTP backend
//! ([`crate::http`]), on every target including Redox.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use gix::protocol::transport::Protocol;
use gix::protocol::transport::client::blocking_io::{Transport, http};
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use gix::remote::{Connection, Direction, fetch::Outcome};

use crate::http::GitHttp;
use crate::{GitError, Repository};

/// The refspecs a cache mirrors.
const REFSPECS: [&str; 2] = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];

/// Also asked for, so the remote advertises which branch its `HEAD` names.
/// It writes no local ref.
const HEAD_REFSPEC: &str = "HEAD";

/// The name the source's URL is saved under in a new cache.
const ORIGIN: &str = "origin";

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
    let fetch_error = |source| GitError::Fetch {
        url: url.to_owned(),
        source: Box::new(source),
    };
    if cache.exists() {
        let repo = Repository::open(cache)?;
        update(&repo, url).map_err(fetch_error)?;
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
    let repo = gix::init_bare(&partial).map_err(fetch_error)?;
    save_origin(&repo, url).map_err(|source| match source {
        Origin::Git(source) => fetch_error(source),
        Origin::Io(source) => GitError::Export {
            path: partial.clone(),
            source,
        },
    })?;
    update(&Repository { inner: repo }, url).map_err(fetch_error)?;
    fs::rename(&partial, cache).map_err(|source| GitError::Export {
        path: cache.to_path_buf(),
        source,
    })?;
    Repository::open(cache)
}

enum Origin {
    Git(gix::Error),
    Io(io::Error),
}

/// Records `url` as the cache's `origin`, as a clone does.
fn save_origin(repo: &gix::Repository, url: &str) -> Result<(), Origin> {
    let path = repo.path().join("config");
    let mut config =
        gix::config::File::from_path_no_includes(path.clone(), gix::config::Source::Local)
            .map_err(Origin::Git)?;
    repo.remote_at(url)
        .and_then(|mut remote| remote.save_as_to(ORIGIN, &mut config))
        .map_err(Origin::Git)?;
    fs::write(&path, config.to_bstring()).map_err(Origin::Io)
}

/// Fetches the remote's branches and tags into `repo`, then drops the tags
/// the remote no longer has and follows its `HEAD`.
fn update(repo: &Repository, url: &str) -> gix::Result<()> {
    let remote = repo.inner.remote_at(url)?.with_refspecs(
        REFSPECS.iter().copied().chain([HEAD_REFSPEC]),
        Direction::Fetch,
    )?;
    let outcome = if url.starts_with("https://") || url.starts_with("http://") {
        let url = gix::url::parse(gix::bstr::BStr::new(url))?;
        let transport = http::connect_http(GitHttp::new(), url, Protocol::V2, false);
        receive(remote.to_connection_with_transport(transport))?
    } else {
        receive(remote.connect(Direction::Fetch)?)?
    };
    prune_tags(repo, &outcome.ref_map.remote_refs)?;
    follow_remote_head(repo, &outcome.ref_map.remote_refs)
}

fn receive<T: Transport>(connection: Connection<'_, '_, '_, T>) -> gix::Result<Outcome> {
    let interrupt = AtomicBool::new(false);
    connection
        .prepare_fetch(gix::progress::Discard, Default::default())?
        .receive(gix::progress::Discard, &interrupt)
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
