//! A registry's life, from the maintainer tool's edits to a client's
//! verification, including the attacks TUF's checks exist for.

use std::fs;
use std::path::Path;

use fox_core::trust::AuthorKey;
use fox_core::{Blake3Digest, KeyId, PackageName};
use fox_index::edit::{EditError, Keyring, Registry, SignOptions, single_key_roles};
use fox_index::layout::{PackageEntry, Release};
use fox_index::metadata::{RoleKeys, RoleName, Signed, Snapshot, Targets, Timestamp};
use fox_index::verify::{self, ClientState, VerifyError};
use pkgar_core::dryoc::classic::crypto_sign::crypto_sign_seed_keypair;
use semver::Version;
use time::{Duration, OffsetDateTime};

fn secret(seed: u8) -> [u8; 64] {
    crypto_sign_seed_keypair(&[seed; 32]).1
}

fn keyring(seeds: &[u8]) -> Keyring {
    let mut keyring = Keyring::new();
    for seed in seeds {
        keyring.add(secret(*seed));
    }
    keyring
}

fn key(seed: u8) -> KeyId {
    fox_core::signing::public_key_of(&secret(seed))
}

fn owner_key(seed: u8) -> AuthorKey {
    let pair = ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
    AuthorKey::new(ssh_key::PublicKey::from(pair.public)).unwrap()
}

const MAINTAINER: u8 = 1;
const OWNER: u8 = 7;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn ripgrep() -> PackageName {
    PackageName::new("ripgrep").unwrap()
}

fn entry() -> PackageEntry {
    let text = format!(
        "name = \"ripgrep\"\nkind = \"app\"\ngit = \"https://example.com/ripgrep\"\n\n\
         [[owners]]\nid = \"owner\"\nkey = \"{}\"\n",
        owner_key(OWNER).public_key().to_openssh().unwrap()
    );
    PackageEntry::from_toml(&text, "packages/ri/ripgrep.toml").unwrap()
}

fn release(major: u64) -> Release {
    Release {
        version: Version::new(major, 0, 0),
        tag: format!("v{major}.0.0"),
        commit: "3f1c0e5a3f1c0e5a3f1c0e5a3f1c0e5a3f1c0e5a".to_owned(),
        tree_blake3: Blake3Digest::of(&[major as u8]),
        yanked: false,
    }
}

/// A registry with ripgrep admitted, signed by the maintainer and the owner.
fn admitted(dir: &Path) -> Registry {
    let mut registry =
        Registry::init(dir, "test", &single_key_roles(key(MAINTAINER)), now()).unwrap();
    registry.admit(&entry()).unwrap();
    let report = registry
        .sign(&keyring(&[MAINTAINER]), now(), &SignOptions::default())
        .unwrap();
    // The delegation waits for its owner.
    assert!(!report.complete());
    let report = registry
        .sign(
            &keyring(&[MAINTAINER, OWNER]),
            now(),
            &SignOptions::default(),
        )
        .unwrap();
    assert!(report.complete(), "{report:?}");
    registry
}

fn client(registry: &Registry) -> ClientState {
    ClientState::trust_root(registry.root().clone()).unwrap()
}

fn read<T: for<'de> serde::Deserialize<'de>>(dir: &Path, file: &str) -> Signed<T> {
    Signed::from_json(&fs::read(dir.join(file)).unwrap()).unwrap()
}

#[test]
fn owners_publish_releases_without_the_targets_key() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let mut state = client(&registry);
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert_eq!(verified.package_names(), vec![ripgrep()]);
    assert!(
        verified
            .package(&ripgrep())
            .unwrap()
            .unwrap()
            .releases
            .is_empty()
    );
    let targets_version = read::<Targets>(dir.path(), "metadata/targets.json")
        .signed
        .version;

    // The owner records a release and signs only the delegation.
    registry.add_release(&ripgrep(), release(1)).unwrap();
    let report = registry
        .sign(&keyring(&[OWNER]), now(), &SignOptions::default())
        .unwrap();
    let delegation = report
        .files
        .iter()
        .find(|file| file.file == "delegations/ripgrep.json")
        .unwrap();
    assert!(delegation.complete() && delegation.bumped);
    assert!(!report.complete(), "snapshot and timestamp wait for CI");
    assert!(verify::update(dir.path(), &mut state.clone(), now()).is_err());

    // CI signs snapshot and timestamp after the merge.
    let mut registry = Registry::open(dir.path()).unwrap();
    let report = registry
        .sign(&keyring(&[MAINTAINER]), now(), &SignOptions::default())
        .unwrap();
    assert!(report.complete(), "{report:?}");
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    let entry = verified.package(&ripgrep()).unwrap().unwrap();
    assert_eq!(
        entry.newest_release().unwrap().version,
        Version::new(1, 0, 0)
    );
    assert_eq!(
        read::<Targets>(dir.path(), "metadata/targets.json")
            .signed
            .version,
        targets_version,
        "a release needs no maintainer signature"
    );

    // Yanking is a release edit like any other.
    registry
        .set_yanked(&ripgrep(), &Version::new(1, 0, 0), true)
        .unwrap();
    registry
        .sign(
            &keyring(&[MAINTAINER, OWNER]),
            now(),
            &SignOptions::default(),
        )
        .unwrap();
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    let entry = verified.package(&ripgrep()).unwrap().unwrap();
    assert!(entry.newest_release().is_none());
}

#[test]
fn signing_twice_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let before = fs::read(dir.path().join("metadata/timestamp.json")).unwrap();
    let report = registry
        .sign(
            &keyring(&[MAINTAINER, OWNER]),
            now(),
            &SignOptions::default(),
        )
        .unwrap();
    assert!(report.files.iter().all(|file| !file.bumped), "{report:?}");
    assert_eq!(
        fs::read(dir.path().join("metadata/timestamp.json")).unwrap(),
        before
    );
}

#[test]
fn scheduled_signing_renews_the_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let mut state = client(&registry);
    verify::update(dir.path(), &mut state, now()).unwrap();

    let later = now() + Duration::hours(30);
    assert!(matches!(
        verify::update(dir.path(), &mut state.clone(), later),
        Err(VerifyError::Expired { .. })
    ));
    let report = registry
        .sign(&keyring(&[MAINTAINER]), later, &SignOptions::default())
        .unwrap();
    let bumped: Vec<_> = report
        .files
        .iter()
        .filter(|file| file.bumped)
        .map(|file| file.file.as_str())
        .collect();
    assert_eq!(bumped, ["metadata/timestamp.json"]);
    verify::update(dir.path(), &mut state, later).unwrap();
}

#[test]
fn old_metadata_is_refused_after_newer_was_seen() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let old_timestamp = fs::read(dir.path().join("metadata/timestamp.json")).unwrap();
    let mut state = client(&registry);

    let options = SignOptions {
        bump: [RoleName::Timestamp].into(),
        ..SignOptions::default()
    };
    registry
        .sign(&keyring(&[MAINTAINER]), now(), &options)
        .unwrap();
    verify::update(dir.path(), &mut state, now()).unwrap();

    fs::write(dir.path().join("metadata/timestamp.json"), old_timestamp).unwrap();
    assert!(matches!(
        verify::update(dir.path(), &mut state, now()),
        Err(VerifyError::Rollback { .. })
    ));
}

#[test]
fn files_must_match_their_signed_hash() {
    let dir = tempfile::tempdir().unwrap();
    admitted(dir.path());
    let registry = Registry::open(dir.path()).unwrap();
    let mut state = client(&registry);
    let package = dir.path().join("packages/ri/ripgrep.toml");
    let text = fs::read_to_string(&package).unwrap();
    // Same length, so the hash check is what catches it.
    fs::write(&package, text.replace("kind = \"app\"", "kind = \"xyz\"")).unwrap();
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert!(matches!(
        verified.package(&ripgrep()),
        Err(VerifyError::HashMismatch { .. })
    ));
    // Unsigned files cannot be read at all.
    fs::write(dir.path().join("advisories/new.toml"), "x").unwrap();
    assert!(matches!(
        verified.read("advisories/new.toml"),
        Err(VerifyError::UnknownTarget { .. })
    ));
}

#[test]
fn a_delegation_cannot_sign_paths_it_was_not_given() {
    let dir = tempfile::tempdir().unwrap();
    let registry = admitted(dir.path());
    let mut state = client(&registry);

    // The owner signs a delegation that also claims registry.toml...
    let file = "metadata/delegations/ripgrep.json";
    let mut delegation: Signed<Targets> = read(dir.path(), file);
    let config = fs::read(dir.path().join("registry.toml")).unwrap();
    delegation.signed.targets.insert(
        "registry.toml".to_owned(),
        fox_index::metadata::FileMeta::of(&config),
    );
    delegation.signatures.clear();
    delegation.sign(&secret(OWNER)).unwrap();
    let bytes = delegation.to_json().unwrap();
    fs::write(dir.path().join(file), &bytes).unwrap();

    // ...and even a snapshot that lists it does not help.
    let mut snapshot: Signed<Snapshot> = read(dir.path(), "metadata/snapshot.json");
    let meta = snapshot
        .signed
        .meta
        .get_mut("delegations/ripgrep.json")
        .unwrap();
    meta.file = fox_index::metadata::FileMeta::of(&bytes);
    snapshot.signatures.clear();
    snapshot.sign(&secret(MAINTAINER)).unwrap();
    let bytes = snapshot.to_json().unwrap();
    fs::write(dir.path().join("metadata/snapshot.json"), &bytes).unwrap();
    let mut timestamp: Signed<Timestamp> = read(dir.path(), "metadata/timestamp.json");
    timestamp.signed.snapshot.file = fox_index::metadata::FileMeta::of(&bytes);
    timestamp.signatures.clear();
    timestamp.sign(&secret(MAINTAINER)).unwrap();
    fs::write(
        dir.path().join("metadata/timestamp.json"),
        timestamp.to_json().unwrap(),
    )
    .unwrap();

    assert!(matches!(
        verify::update(dir.path(), &mut state, now()),
        Err(VerifyError::DelegationPath { .. })
    ));
}

#[test]
fn root_rotation_needs_old_and_new_keys() {
    const NEW: u8 = 9;
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let mut state = client(&registry);
    verify::update(dir.path(), &mut state, now()).unwrap();

    let options = SignOptions::default();
    for role in [RoleName::Root, RoleName::Targets] {
        registry
            .set_role(
                role,
                RoleKeys::new([key(NEW)], 1),
                now(),
                &options.lifetimes,
            )
            .unwrap();
    }
    // Both changes went into one new root, signed by the new keys only:
    // the client cannot chain to it.
    registry.sign(&keyring(&[NEW]), now(), &options).unwrap();
    assert!(matches!(
        verify::update(dir.path(), &mut state.clone(), now()),
        Err(VerifyError::Metadata { .. })
    ));

    // With the old root key too, the client follows the rotation and from
    // then on accepts targets signed by the new key.
    let mut registry = Registry::open(dir.path()).unwrap();
    let report = registry
        .sign(&keyring(&[MAINTAINER, NEW]), now(), &options)
        .unwrap();
    assert!(report.complete(), "{report:?}");
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert_eq!(verified.root().version, 2);
    assert_eq!(state.root.signed.version, 2);
    let targets: Signed<Targets> = read(dir.path(), "metadata/targets.json");
    assert_eq!(
        targets
            .signatures
            .iter()
            .map(|signature| signature.keyid)
            .collect::<Vec<_>>(),
        [key(NEW)]
    );
}

#[test]
fn an_unsigned_root_is_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::init(
        dir.path(),
        "test",
        &single_key_roles(key(MAINTAINER)),
        now(),
    )
    .unwrap();
    assert!(ClientState::trust_root(registry.root().clone()).is_err());
}

#[test]
fn admitting_twice_or_releasing_twice_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    assert!(registry.admit(&entry()).is_err());
    registry.add_release(&ripgrep(), release(1)).unwrap();
    assert!(registry.add_release(&ripgrep(), release(1)).is_err());
    assert!(Registry::init(dir.path(), "again", &single_key_roles(key(1)), now()).is_err());
}

#[test]
fn packages_are_admitted_together_or_not_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let other = |name: &str| {
        let mut entry = entry();
        entry.name = PackageName::new(name).unwrap();
        entry
    };
    // One package already admitted refuses the whole batch.
    assert!(matches!(
        registry.admit_all(&[other("fd"), entry()]),
        Err(EditError::AlreadyAdmitted { .. })
    ));
    assert!(matches!(
        registry.admit_all(&[other("fd"), other("fd")]),
        Err(EditError::AlreadyAdmitted { .. })
    ));
    assert!(!dir.path().join("packages/fd/fd.toml").exists());

    registry.admit_all(&[other("fd"), other("bat")]).unwrap();
    // The delegations persist for another process to sign.
    let mut reopened = Registry::open(dir.path()).unwrap();
    let report = reopened
        .sign(
            &keyring(&[MAINTAINER, OWNER]),
            now(),
            &SignOptions::default(),
        )
        .unwrap();
    assert!(report.complete(), "{report:?}");
    let mut state = client(&reopened);
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert_eq!(verified.package_names().len(), 3);
}
