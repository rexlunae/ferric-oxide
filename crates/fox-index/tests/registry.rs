//! A registry's life, from the maintainer tool's edits to a client's
//! verification, including the attacks TUF's checks exist for.

use std::fs;
use std::path::Path;

use fox_core::trust::AuthorKey;
use fox_core::{Blake3Digest, KeyId, PackageName};
use fox_index::edit::{EditError, Keyring, Registry, SignOptions, single_key_roles};
use fox_index::layout::{Owner, PackageEntry, Release};
use fox_index::metadata::{
    FileMeta, RoleKeys, RoleName, Root, Signed, Snapshot, Targets, Timestamp,
};
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

/// Re-signs the snapshot and timestamp around a hand-edited `targets.json`,
/// as a maintainer would, keeping their versions.
fn reseal(dir: &Path, seed: u8) {
    let targets = fs::read(dir.join("metadata/targets.json")).unwrap();
    let mut snapshot: Signed<Snapshot> = read(dir, "metadata/snapshot.json");
    snapshot.signed.meta.get_mut("targets.json").unwrap().file = FileMeta::of(&targets);
    snapshot.signatures.clear();
    snapshot.sign(&secret(seed)).unwrap();
    let snapshot_bytes = snapshot.to_json().unwrap();
    fs::write(dir.join("metadata/snapshot.json"), &snapshot_bytes).unwrap();

    let mut timestamp: Signed<Timestamp> = read(dir, "metadata/timestamp.json");
    timestamp.signed.snapshot.file = FileMeta::of(&snapshot_bytes);
    timestamp.signatures.clear();
    timestamp.sign(&secret(seed)).unwrap();
    fs::write(
        dir.join("metadata/timestamp.json"),
        timestamp.to_json().unwrap(),
    )
    .unwrap();
}

#[test]
fn a_delegation_name_cannot_leave_the_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let registry = admitted(dir.path());
    let mut state = client(&registry);

    let mut targets: Signed<Targets> = read(dir.path(), "metadata/targets.json");
    targets.signed.delegations.first_mut().unwrap().name = "../escape".to_owned();
    targets.signatures.clear();
    targets.sign(&secret(MAINTAINER)).unwrap();
    fs::write(
        dir.path().join("metadata/targets.json"),
        targets.to_json().unwrap(),
    )
    .unwrap();
    reseal(dir.path(), MAINTAINER);

    assert!(matches!(
        verify::update(dir.path(), &mut state, now()),
        Err(VerifyError::DelegationName { .. })
    ));
}

#[test]
fn the_config_name_must_match_the_root() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let mut state = client(&registry);
    let mut config = registry.config().unwrap();
    config.name = "spoofed".to_owned();
    registry.set_config(&config).unwrap();
    registry
        .sign(
            &keyring(&[MAINTAINER, OWNER]),
            now(),
            &SignOptions::default(),
        )
        .unwrap();
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert!(matches!(
        verified.config(),
        Err(VerifyError::RegistryName { .. })
    ));
}

#[cfg(unix)]
#[test]
fn a_symlink_cannot_leave_the_checkout() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let registry = admitted(dir.path());
    let mut state = client(&registry);

    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret.toml");
    fs::write(&target, b"not the package").unwrap();
    let package = dir.path().join("packages/ri/ripgrep.toml");
    fs::remove_file(&package).unwrap();
    symlink(&target, &package).unwrap();

    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert!(matches!(
        verified.read("packages/ri/ripgrep.toml"),
        Err(VerifyError::UnsafePath { .. })
    ));
}

#[test]
fn a_scheduled_run_cannot_renew_a_role_it_cannot_finish() {
    const T1: u8 = 4;
    const T2: u8 = 5;
    let dir = tempfile::tempdir().unwrap();
    let mut roles = single_key_roles(key(MAINTAINER));
    roles.insert(RoleName::Timestamp, RoleKeys::new([key(T1), key(T2)], 2));
    let mut registry = Registry::init(dir.path(), "test", &roles, now()).unwrap();
    registry.admit(&entry()).unwrap();
    let report = registry
        .sign(
            &keyring(&[MAINTAINER, OWNER, T1, T2]),
            now(),
            &SignOptions::default(),
        )
        .unwrap();
    assert!(report.complete(), "{report:?}");
    let mut state = client(&registry);
    verify::update(dir.path(), &mut state, now()).unwrap();
    let version = read::<Timestamp>(dir.path(), "metadata/timestamp.json")
        .signed
        .version;

    // Past half of the one-day timestamp lifetime, one of its two keys
    // must not renew it alone, or clients would reject the one-signature
    // replacement until the other key signed.
    let later = now() + Duration::hours(13);
    let report = registry
        .sign(&keyring(&[T1]), later, &SignOptions::default())
        .unwrap();
    let timestamp = report
        .files
        .iter()
        .find(|file| file.file == "metadata/timestamp.json")
        .unwrap();
    assert!(!timestamp.bumped, "{timestamp:?}");
    assert!(timestamp.complete());
    assert_eq!(
        read::<Timestamp>(dir.path(), "metadata/timestamp.json")
            .signed
            .version,
        version
    );
    verify::update(dir.path(), &mut state, later).unwrap();

    // Both keys renew it.
    let report = registry
        .sign(&keyring(&[T1, T2]), later, &SignOptions::default())
        .unwrap();
    let timestamp = report
        .files
        .iter()
        .find(|file| file.file == "metadata/timestamp.json")
        .unwrap();
    assert!(timestamp.bumped, "{timestamp:?}");
    verify::update(dir.path(), &mut state, later).unwrap();
}

#[test]
fn an_incomplete_rotation_is_reported_and_amended() {
    const NEW: u8 = 11;
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let options = SignOptions::default();
    registry
        .set_role(
            RoleName::Root,
            RoleKeys::new([key(NEW)], 1),
            now(),
            &options.lifetimes,
        )
        .unwrap();
    // A first signature from the new key alone cannot complete the rotation.
    let report = registry.sign(&keyring(&[NEW]), now(), &options).unwrap();
    let root = report
        .files
        .iter()
        .find(|file| file.file == "metadata/root/2.json")
        .unwrap();
    assert!(!root.complete(), "{root:?}");

    // A later edit amends the pending root instead of stranding it.
    registry
        .set_role(
            RoleName::Targets,
            RoleKeys::new([key(NEW)], 1),
            now(),
            &options.lifetimes,
        )
        .unwrap();
    assert!(!dir.path().join("metadata/root/3.json").exists());
    let report = registry
        .sign(&keyring(&[MAINTAINER, NEW]), now(), &options)
        .unwrap();
    assert!(report.complete(), "{report:?}");
    assert!(!dir.path().join("metadata/root/3.json").exists());
}

#[test]
fn a_root_without_every_role_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    admitted(dir.path());
    let mut root: Signed<Root> = read(dir.path(), "metadata/root/1.json");
    root.signed.roles.remove(&RoleName::Targets);
    fs::write(
        dir.path().join("metadata/root/1.json"),
        root.to_json().unwrap(),
    )
    .unwrap();
    assert!(matches!(
        Registry::open(dir.path()),
        Err(EditError::MissingRole { role: "targets" })
    ));
}

#[test]
fn package_owners_must_match_the_delegation_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let mut state = client(&registry);

    // A package edit adds an owner the maintainer's delegation did not
    // authorize; a client must not treat its tags as trusted.
    let mut entry = registry.package(&ripgrep()).unwrap().unwrap();
    entry.owners.push(Owner {
        id: "other".to_owned(),
        key: owner_key(8),
    });
    registry.write_package(&entry).unwrap();
    registry
        .sign(
            &keyring(&[MAINTAINER, OWNER]),
            now(),
            &SignOptions::default(),
        )
        .unwrap();

    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert!(matches!(
        verified.package(&ripgrep()),
        Err(VerifyError::OwnerMismatch { .. })
    ));
}

#[test]
fn a_same_version_timestamp_cannot_change() {
    let dir = tempfile::tempdir().unwrap();
    let registry = admitted(dir.path());
    let mut state = client(&registry);
    verify::update(dir.path(), &mut state, now()).unwrap();

    // Re-signing the timestamp at the same version with different content
    // is an equivocation the client must detect.
    let mut timestamp: Signed<Timestamp> = read(dir.path(), "metadata/timestamp.json");
    timestamp.signed.snapshot.file = FileMeta::of(b"different");
    timestamp.signatures.clear();
    timestamp.sign(&secret(MAINTAINER)).unwrap();
    fs::write(
        dir.path().join("metadata/timestamp.json"),
        timestamp.to_json().unwrap(),
    )
    .unwrap();

    assert!(matches!(
        verify::update(dir.path(), &mut state, now()),
        Err(VerifyError::Equivocation { .. })
    ));
}

#[test]
fn a_timestamp_key_rotation_keeps_clients_updating() {
    const NEW: u8 = 12;
    let dir = tempfile::tempdir().unwrap();
    let mut registry = admitted(dir.path());
    let mut state = client(&registry);
    verify::update(dir.path(), &mut state, now()).unwrap();
    let trusted = fs::read(dir.path().join("metadata/timestamp.json")).unwrap();

    // Rotating the timestamp role re-signs the same payload at the same
    // version with the new key; a client must follow the rotation instead
    // of reading the re-signed bytes as an equivocation.
    let options = SignOptions::default();
    registry
        .set_role(
            RoleName::Timestamp,
            RoleKeys::new([key(NEW)], 1),
            now(),
            &options.lifetimes,
        )
        .unwrap();
    let report = registry
        .sign(&keyring(&[MAINTAINER, NEW]), now(), &options)
        .unwrap();
    assert!(report.complete(), "{report:?}");
    let row = report
        .files
        .iter()
        .find(|file| file.file == "metadata/timestamp.json")
        .unwrap();
    assert!(!row.bumped, "{row:?}");
    let timestamp: Signed<Timestamp> = read(dir.path(), "metadata/timestamp.json");
    assert_eq!(timestamp.signed.version, state.timestamp);
    assert_eq!(
        timestamp
            .signatures
            .iter()
            .map(|signature| signature.keyid)
            .collect::<Vec<_>>(),
        [key(NEW)]
    );
    assert_ne!(
        fs::read(dir.path().join("metadata/timestamp.json")).unwrap(),
        trusted
    );

    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    assert!(verified.package(&ripgrep()).unwrap().is_some());
}

#[test]
fn a_top_level_package_ignores_an_overlapping_delegation() {
    let dir = tempfile::tempdir().unwrap();
    let registry = admitted(dir.path());
    let mut state = client(&registry);

    // The package file gains an owner its delegation does not grant...
    let mut entry = registry.package(&ripgrep()).unwrap().unwrap();
    entry.owners.push(Owner {
        id: "other".to_owned(),
        key: owner_key(8),
    });
    registry.write_package(&entry).unwrap();

    // ...but the maintainer signs those exact bytes into the top-level
    // targets, which take precedence over the delegation for that path.
    let mut targets: Signed<Targets> = read(dir.path(), "metadata/targets.json");
    let bytes = fs::read(dir.path().join("packages/ri/ripgrep.toml")).unwrap();
    targets
        .signed
        .targets
        .insert("packages/ri/ripgrep.toml".to_owned(), FileMeta::of(&bytes));
    targets.signatures.clear();
    targets.sign(&secret(MAINTAINER)).unwrap();
    fs::write(
        dir.path().join("metadata/targets.json"),
        targets.to_json().unwrap(),
    )
    .unwrap();
    reseal(dir.path(), MAINTAINER);

    // The package's bytes are the top-level-signed ones, so the delegation
    // does not constrain its owners.
    let verified = verify::update(dir.path(), &mut state, now()).unwrap();
    let found = verified.package(&ripgrep()).unwrap().unwrap();
    assert_eq!(found.owners, entry.owners);
}

/// Sets the root role's keys and signs with `seeds`; returns whether the
/// report calls the new root complete, and checks that a client agrees.
fn rotate_root(dir: &Path, keys: RoleKeys, seeds: &[u8]) -> bool {
    let options = SignOptions::default();
    let mut registry = Registry::open(dir).unwrap();
    // A client that has only ever trusted the first root.
    let mut state = ClientState::trust_root(read(dir, "metadata/root/1.json")).unwrap();
    registry
        .set_role(RoleName::Root, keys, now(), &options.lifetimes)
        .unwrap();
    let report = registry.sign(&keyring(seeds), now(), &options).unwrap();
    let root = report.files[0].complete();
    assert_eq!(
        root,
        verify::update(dir, &mut state, now()).is_ok(),
        "{report:?}"
    );
    root
}

#[test]
fn raising_the_root_threshold_needs_each_key_set_s_own_threshold() {
    const SECOND: u8 = 12;
    let dir = tempfile::tempdir().unwrap();
    admitted(dir.path());
    let raised = RoleKeys::new([key(MAINTAINER), key(SECOND)], 2);
    // The old threshold (1 of the maintainer key) is met, the new one is not.
    assert!(!rotate_root(dir.path(), raised.clone(), &[MAINTAINER]));
    // Both met: 1 of 1 old, 2 of 2 new.
    assert!(rotate_root(dir.path(), raised, &[MAINTAINER, SECOND]));
}

#[test]
fn lowering_the_root_threshold_still_needs_the_old_threshold() {
    const SECOND: u8 = 12;
    let dir = tempfile::tempdir().unwrap();
    admitted(dir.path());
    let both = RoleKeys::new([key(MAINTAINER), key(SECOND)], 2);
    assert!(rotate_root(dir.path(), both, &[MAINTAINER, SECOND]));
    let lowered = RoleKeys::new([key(MAINTAINER)], 1);
    // The new threshold (1) is met, the old one (2 of 2) is not.
    assert!(!rotate_root(dir.path(), lowered.clone(), &[MAINTAINER]));
    assert!(rotate_root(dir.path(), lowered, &[MAINTAINER, SECOND]));
}

#[test]
fn editing_a_pending_root_drops_every_signature_on_it() {
    const SECOND: u8 = 12;
    const TARGETS: u8 = 13;
    let dir = tempfile::tempdir().unwrap();
    admitted(dir.path());
    let options = SignOptions::default();
    let both = RoleKeys::new([key(MAINTAINER), key(SECOND)], 2);
    // Only the new key approves the pending root: 1 of 2 new, 0 of 1 old.
    assert!(!rotate_root(dir.path(), both, &[SECOND]));
    let pending: Signed<Root> = read(dir.path(), "metadata/root/2.json");
    assert_eq!(pending.signatures.len(), 1);

    // Any edit to the pending root changes what was approved, so neither
    // key set's signatures survive it.
    let mut registry = Registry::open(dir.path()).unwrap();
    registry
        .set_role(
            RoleName::Targets,
            RoleKeys::new([key(TARGETS)], 1),
            now(),
            &options.lifetimes,
        )
        .unwrap();
    let edited: Signed<Root> = read(dir.path(), "metadata/root/2.json");
    assert!(edited.signatures.is_empty());

    // The old key, also a new root key, cannot complete the edited root on
    // the strength of the new key's earlier approval.
    let report = registry
        .sign(&keyring(&[MAINTAINER, TARGETS]), now(), &options)
        .unwrap();
    let root = &report.files[0];
    assert_eq!((root.valid, root.threshold), (1, 2));
    assert!(!root.complete());
    let mut state = ClientState::trust_root(read(dir.path(), "metadata/root/1.json")).unwrap();
    assert!(verify::update(dir.path(), &mut state, now()).is_err());

    // The new key has to approve the edited content itself.
    let report = registry
        .sign(&keyring(&[SECOND, TARGETS]), now(), &options)
        .unwrap();
    assert!(report.files[0].complete(), "{report:?}");
}

#[test]
fn a_delegation_name_is_one_path_component() {
    let dir = tempfile::tempdir().unwrap();
    let registry = admitted(dir.path());
    let mut state = client(&registry);

    // A nested name, with its metadata moved to match and described by a
    // re-signed snapshot, is still refused.
    let mut targets: Signed<Targets> = read(dir.path(), "metadata/targets.json");
    targets.signed.delegations.first_mut().unwrap().name = "ri/ripgrep".to_owned();
    targets.signatures.clear();
    targets.sign(&secret(MAINTAINER)).unwrap();
    fs::write(
        dir.path().join("metadata/targets.json"),
        targets.to_json().unwrap(),
    )
    .unwrap();
    let delegations = dir.path().join("metadata/delegations");
    fs::create_dir_all(delegations.join("ri")).unwrap();
    fs::rename(
        delegations.join("ripgrep.json"),
        delegations.join("ri/ripgrep.json"),
    )
    .unwrap();
    let mut snapshot: Signed<Snapshot> = read(dir.path(), "metadata/snapshot.json");
    let meta = snapshot
        .signed
        .meta
        .remove("delegations/ripgrep.json")
        .unwrap();
    snapshot
        .signed
        .meta
        .insert("delegations/ri/ripgrep.json".to_owned(), meta);
    fs::write(
        dir.path().join("metadata/snapshot.json"),
        snapshot.to_json().unwrap(),
    )
    .unwrap();
    reseal(dir.path(), MAINTAINER);

    assert!(matches!(
        verify::update(dir.path(), &mut state, now()),
        Err(VerifyError::DelegationName { name }) if name == "ri/ripgrep"
    ));
}
