//! Signed release tags and fetching, on repositories made with the `git`
//! command. Tags are signed in Rust, since `ssh-keygen` may be missing; git
//! itself checks that the objects are well formed.

use fox_core::trust::AuthorKey;
use fox_git::{GitError, Repository, TagSigner, fetch};
use semver::Version;

mod common;
use common::{TAGGER, git, git_stdin, http_server, sample_repo, signing_key};

fn author(seed: u8) -> AuthorKey {
    AuthorKey::new(signing_key(seed).public_key().clone()).unwrap()
}

#[test]
fn signed_tags_verify_against_their_key_only() {
    let dir = tempfile::tempdir().unwrap();
    sample_repo(dir.path());
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.resolve("HEAD").unwrap();
    let tag = repo
        .create_signed_tag("v1.0.0", &head, TAGGER, "Release 1.0.0", &signing_key(1))
        .unwrap();
    assert_eq!(tag.commit(), &head);
    assert_eq!(tag.version(), Some(Version::new(1, 0, 0)));
    match tag.signer() {
        TagSigner::Ssh(key) => assert_eq!(key, author(1)),
        other => panic!("{other:?}"),
    }
    tag.verify(&author(1)).unwrap();
    assert!(matches!(
        tag.verify(&author(2)),
        Err(GitError::TagSignature { .. })
    ));

    let owners = [author(2), author(1)];
    assert_eq!(tag.verify_signed_by(&owners).unwrap(), &author(1));
    assert!(matches!(
        tag.verify_signed_by(&owners[..1]),
        Err(GitError::TagSignerNotAllowed { .. })
    ));

    // git accepts the object and sees the signature where it puts its own.
    let object = git(dir.path(), &["cat-file", "tag", "v1.0.0"]);
    assert!(object.starts_with(&format!("object {}\ntype commit\ntag v1.0.0\n", head.id())));
    assert!(object.contains("\n\nRelease 1.0.0\n-----BEGIN SSH SIGNATURE-----\n"));
    git(dir.path(), &["fsck", "--strict"]);

    // Read back through a fresh handle.
    let reread = Repository::open(dir.path()).unwrap().tag("v1.0.0").unwrap();
    reread.verify(&author(1)).unwrap();

    assert!(matches!(
        repo.create_signed_tag("v1.0.0", &head, TAGGER, "again", &signing_key(1)),
        Err(GitError::Write { .. })
    ));
}

#[test]
fn tampered_and_replayed_tags_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    sample_repo(dir.path());
    let repo = Repository::open(dir.path()).unwrap();
    let head = repo.resolve("HEAD").unwrap();
    repo.create_signed_tag("v1.0.0", &head, TAGGER, "Release", &signing_key(1))
        .unwrap();

    // The same signed object under another version's name.
    let object = git(dir.path(), &["rev-parse", "refs/tags/v1.0.0"]);
    git(dir.path(), &["update-ref", "refs/tags/v2.0.0", &object]);
    assert!(matches!(repo.tag("v2.0.0"), Err(GitError::TagName { .. })));

    // A changed message keeps the signature block but breaks it.
    let text = git(dir.path(), &["cat-file", "tag", "v1.0.0"]) + "\n";
    let forged = text.replace("Release\n", "Release (patched)\n");
    let id = git_stdin(
        dir.path(),
        &["mktag"],
        forged.replace("v1.0.0", "v1.0.1").as_bytes(),
    );
    git(dir.path(), &["update-ref", "refs/tags/v1.0.1", &id]);
    let tag = Repository::open(dir.path()).unwrap().tag("v1.0.1").unwrap();
    assert!(matches!(
        tag.verify(&author(1)),
        Err(GitError::TagSignature { .. })
    ));
}

#[test]
fn unsigned_tags_and_version_order() {
    let dir = tempfile::tempdir().unwrap();
    sample_repo(dir.path());
    git(dir.path(), &["tag", "v0.9.0"]);
    git(
        dir.path(),
        &["tag", "-a", "1.10.0", "-m", "annotated, unsigned"],
    );
    git(dir.path(), &["tag", "v1.2.0"]);
    git(dir.path(), &["tag", "v2.0.0-rc.1"]);
    git(dir.path(), &["tag", "not-a-version"]);
    let repo = Repository::open(dir.path()).unwrap();
    for name in ["v0.9.0", "1.10.0"] {
        let tag = repo.tag(name).unwrap();
        assert!(matches!(tag.signer(), TagSigner::Unsigned), "{name}");
        assert!(matches!(
            tag.verify(&author(1)),
            Err(GitError::TagUnsigned { .. })
        ));
    }
    let names: Vec<_> = repo
        .version_tags()
        .unwrap()
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(names, ["v0.9.0", "v1.2.0", "1.10.0", "v2.0.0-rc.1"]);
    assert_eq!(
        repo.newest_release_tag().unwrap().as_deref(),
        Some("1.10.0")
    );
    assert!(matches!(repo.tag("v9.9.9"), Err(GitError::Resolve { .. })));
}

#[test]
fn fetches_into_a_bare_cache_and_updates_it() {
    let upstream = tempfile::tempdir().unwrap();
    let url = format!("file://{}", upstream.path().display());
    fetch_and_update(upstream.path(), &url);

    assert!(matches!(
        fetch(
            &format!("file://{}/missing", upstream.path().display()),
            &tempfile::tempdir().unwrap().path().join("x")
        ),
        Err(GitError::Fetch { .. })
    ));
}

#[test]
fn fetches_over_http() {
    let root = tempfile::tempdir().unwrap();
    let upstream = root.path().join("hello");
    std::fs::create_dir(&upstream).unwrap();
    let base = http_server::serve_git(root.path());
    fetch_and_update(&upstream, &format!("{base}/hello"));

    let error = fetch(
        &format!("{base}/missing"),
        &tempfile::tempdir().unwrap().path().join("x"),
    )
    .unwrap_err();
    assert!(matches!(error, GitError::Fetch { .. }), "{error:?}");
}

#[test]
fn git_redirects_are_not_followed() {
    let target = tempfile::tempdir().unwrap();
    let moved = http_server::serve(move |_| {
        let mut response = http_server::Response::new("301 Moved Permanently", "");
        response
            .headers
            .push(("Location".into(), "https://example.com/new.git".into()));
        response
    });
    let error = fetch(&format!("{moved}/old.git"), &target.path().join("x")).unwrap_err();
    let chain = error_chain(&error);
    assert!(
        chain.contains("redirects to Some(\"https://example.com/new.git\")"),
        "{chain}"
    );
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        text.push_str(": ");
        text.push_str(&error.to_string());
        source = error.source();
    }
    text
}

/// Clones `url`, which serves the repository at `upstream`, then fetches
/// new commits, tags and a changed default branch.
fn fetch_and_update(upstream: &std::path::Path, url: &str) {
    sample_repo(upstream);
    let origin = Repository::open(upstream).unwrap();
    let first = origin.resolve("HEAD").unwrap();
    origin
        .create_signed_tag("v1.0.0", &first, TAGGER, "one", &signing_key(1))
        .unwrap();

    let caches = tempfile::tempdir().unwrap();
    let cache = caches.path().join("hello");
    let cached = fetch(url, &cache).unwrap();
    assert!(cached.workdir().is_none());
    assert_eq!(cached.resolve("HEAD").unwrap(), first);
    assert_eq!(cached.origin_url().as_deref(), Some(url));
    cached.tag("v1.0.0").unwrap().verify(&author(1)).unwrap();
    assert!(!caches.path().join("hello.partial").exists());

    git(upstream, &["commit", "-q", "--allow-empty", "-m", "two"]);
    let origin = Repository::open(upstream).unwrap();
    let second = origin.resolve("HEAD").unwrap();
    origin
        .create_signed_tag("v1.1.0", &second, TAGGER, "two", &signing_key(1))
        .unwrap();
    let cached = fetch(url, &cache).unwrap();
    assert_eq!(cached.resolve("main").unwrap(), second);
    assert_eq!(
        cached.newest_release_tag().unwrap().as_deref(),
        Some("v1.1.0")
    );
    assert_eq!(
        cached.tree_blake3(&second).unwrap(),
        origin.tree_blake3(&second).unwrap()
    );

    // The remote switches its default branch; the cache's HEAD follows.
    git(upstream, &["checkout", "-q", "-b", "trunk"]);
    git(upstream, &["commit", "-q", "--allow-empty", "-m", "three"]);
    let third = Repository::open(upstream).unwrap().resolve("HEAD").unwrap();
    let cached = fetch(url, &cache).unwrap();
    assert_eq!(cached.resolve("HEAD").unwrap(), third);
}
