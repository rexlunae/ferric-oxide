//! Headers written by cryptsetup (see `fixtures/README`): parsing, unlocking, recovery from a
//! damaged copy, and robustness against corruption.

use std::io;

use ferric_luks::luks2::CopyError;
use ferric_luks::luks2::binary::{BinaryHeaderError, HeaderCopy};
use ferric_luks::{Header, Limits, ReadError, UnlockError, read};
use sha2::{Digest, Sha256};

const PASSPHRASE: &[u8] = b"correct horse";

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(path).expect("fixture")
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex"))
        .collect()
}

/// A read function over an in-memory image.
fn reader(image: &[u8]) -> impl FnMut(u64, &mut [u8]) -> io::Result<()> + '_ {
    move |offset, buf| {
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let bytes = start
            .checked_add(buf.len())
            .and_then(|end| image.get(start..end))
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
}

fn luks2(image: &[u8]) -> ferric_luks::luks2::Luks2 {
    match read(&mut reader(image)).expect("valid header") {
        Header::V2(h) => h,
        other => panic!("not LUKS2: {other:?}"),
    }
}

const PBKDF2_KEY: &str = "3acb76604a03afd6f2e90ee076bfaac4fc764a63fff324ec4c6c8498f9ab71a9";
const ARGON2_KEY: &str = "fd7d7831122e3b7f51a22889894fea021f563860721f2b94c335c8ce76023afa";
const LUKS1_KEY: &str = "d7e382230b219343699c1cef3410024b61ce830ee929a7512a8ef7a3e84e666b";

#[test]
fn luks2_pbkdf2_unlocks_with_either_passphrase() {
    let image = fixture("luks2-pbkdf2.img");
    let header = luks2(&image);
    assert_eq!(header.binary.uuid, "f9f7eb0e-5e79-4e7f-be02-74d5e98386ba");
    assert_eq!(header.binary.label, "ferric-test");
    assert_eq!(header.binary.hdr_size, 0x4000);
    assert_eq!(header.binary.copy, HeaderCopy::Primary);
    assert!(header.other_copy_error().is_none());
    assert_eq!(header.metadata.keyslots.len(), 2);
    assert_eq!(header.payload_offset(), Some(1 << 20));
    let limits = Limits::default();
    let mut read_at = reader(&image);
    let first = header
        .unlock(&mut read_at, PASSPHRASE, &limits)
        .expect("slot 0");
    assert_eq!(first.keyslot, 0);
    assert_eq!(first.key.as_bytes(), hex(PBKDF2_KEY));
    let second = header
        .unlock(&mut read_at, b"second slot", &limits)
        .expect("slot 1");
    assert_eq!(second.keyslot, 1);
    assert_eq!(second.key, first.key);
    assert!(matches!(
        header.unlock_keyslot(&mut read_at, 1, PASSPHRASE, &limits),
        Err(UnlockError::NoMatch)
    ));
    assert!(matches!(
        header.unlock(&mut read_at, b"wrong", &limits),
        Err(UnlockError::NoMatch)
    ));
    assert!(matches!(
        header.unlock_keyslot(&mut read_at, 5, PASSPHRASE, &limits),
        Err(UnlockError::NoSuchKeyslot(5))
    ));
}

#[test]
fn luks2_argon2id_unlocks() {
    let image = fixture("luks2-argon2id.img");
    let header = read(&mut reader(&image)).expect("valid");
    assert_eq!(header.version(), 2);
    assert_eq!(header.uuid(), "110752d7-201b-49a2-8f94-c113a5b08000");
    let unlocked = header
        .unlock(&mut reader(&image), PASSPHRASE, &Limits::default())
        .expect("unlocks");
    assert_eq!(unlocked.key.as_bytes(), hex(ARGON2_KEY));
    // The header asks for 32 KiB; a tighter memory limit refuses before deriving anything.
    let tight = Limits {
        max_argon2_memory_kib: 16,
        ..Limits::default()
    };
    assert!(matches!(
        header.unlock(&mut reader(&image), PASSPHRASE, &tight),
        Err(UnlockError::OverLimits)
    ));
}

#[test]
fn luks1_unlocks() {
    let image = fixture("luks1.img");
    let Header::V1(header) = read(&mut reader(&image)).expect("valid") else {
        panic!("LUKS1");
    };
    assert_eq!(header.uuid, "c273805f-6b27-4f70-bd2f-bd890f8675b0");
    assert_eq!(header.cipher_name, "aes");
    assert_eq!(header.cipher_mode, "xts-plain64");
    assert_eq!(header.payload_offset_bytes(), 4096 * 512);
    assert_eq!(header.keyslots.iter().filter(|s| s.enabled).count(), 1);
    let limits = Limits::default();
    let unlocked = header
        .unlock(&mut reader(&image), PASSPHRASE, &limits)
        .expect("unlocks");
    assert_eq!(unlocked.keyslot, 0);
    assert_eq!(unlocked.key.as_bytes(), hex(LUKS1_KEY));
    assert!(matches!(
        header.unlock(&mut reader(&image), b"wrong", &limits),
        Err(UnlockError::NoMatch)
    ));
}

/// Rewrites the checksum of the copy at `offset` after a test edits it.
fn reseal(image: &mut [u8], offset: usize) {
    let size = u64::from_be_bytes(image[offset + 8..offset + 16].try_into().unwrap()) as usize;
    let area = &mut image[offset..offset + size];
    area[0x1c0..0x200].fill(0);
    let sum = Sha256::digest(&area[..]);
    area[0x1c0..0x1e0].copy_from_slice(&sum);
}

#[test]
fn a_damaged_copy_falls_back_to_the_other() {
    let image = fixture("luks2-pbkdf2.img");
    let limits = Limits::default();
    let check = |image: &[u8], used: HeaderCopy| {
        let header = luks2(image);
        assert_eq!(header.binary.copy, used);
        let key = header
            .unlock(&mut reader(image), PASSPHRASE, &limits)
            .expect("unlocks");
        assert_eq!(key.key.as_bytes(), hex(PBKDF2_KEY));
        header
    };

    // A flipped JSON byte in the primary: its checksum fails, the secondary is used.
    let mut damaged = image.clone();
    damaged[0x1000 + 10] ^= 1;
    let header = check(&damaged, HeaderCopy::Secondary);
    assert!(matches!(
        header.other_copy_error(),
        Some(CopyError::Binary(BinaryHeaderError::Checksum))
    ));

    // A destroyed primary binary header: the secondary is found by scanning.
    let mut damaged = image.clone();
    damaged[..0x1000].fill(0);
    check(&damaged, HeaderCopy::Secondary);

    // A damaged secondary: the primary is used.
    let mut damaged = image.clone();
    damaged[0x4000 + 0x1000 + 5] ^= 0x40;
    let header = check(&damaged, HeaderCopy::Primary);
    assert!(header.other_copy_error().is_some());

    // A more recent secondary wins.
    let mut newer = image.clone();
    let seqid = u64::from_be_bytes(newer[0x4010..0x4018].try_into().unwrap());
    newer[0x4010..0x4018].copy_from_slice(&(seqid + 1).to_be_bytes());
    reseal(&mut newer, 0x4000);
    assert_eq!(luks2(&newer).binary.seqid, seqid + 1);
    check(&newer, HeaderCopy::Secondary);

    // Both damaged: both reasons are reported.
    let mut both = image.clone();
    both[0x1000 + 10] ^= 1;
    both[0x4000 + 0x1000 + 10] ^= 1;
    assert!(matches!(
        read(&mut reader(&both)),
        Err(ReadError::NoValidCopy {
            primary: CopyError::Binary(BinaryHeaderError::Checksum),
            secondary: CopyError::Binary(BinaryHeaderError::Checksum),
        })
    ));
}

#[test]
fn an_unreadable_secondary_does_not_block_a_valid_primary() {
    let image = fixture("luks2-pbkdf2.img");
    // Reads that touch the secondary copy (0x4000..0x8000) fail.
    let mut failing = |offset: u64, buf: &mut [u8]| -> io::Result<()> {
        let end = offset + buf.len() as u64;
        if offset < 0x8000 && end > 0x4000 {
            return Err(io::ErrorKind::Other.into());
        }
        reader(&image)(offset, buf)
    };
    let Header::V2(header) = read(&mut failing).expect("the primary is enough") else {
        panic!("LUKS2");
    };
    assert_eq!(header.binary.copy, HeaderCopy::Primary);
    assert!(matches!(
        header.other_copy_error(),
        Some(CopyError::Unreadable { offset: 0x4000 })
    ));
    let key = header
        .unlock(&mut failing, PASSPHRASE, &Limits::default())
        .expect("unlocks");
    assert_eq!(key.key.as_bytes(), hex(PBKDF2_KEY));

    // With the primary damaged too, nothing is usable; the failed read is what is reported.
    let mut damaged = image.clone();
    damaged[0x1000 + 10] ^= 1;
    let mut failing = |offset: u64, buf: &mut [u8]| -> io::Result<()> {
        if offset == 0x4000 {
            return Err(io::ErrorKind::Other.into());
        }
        reader(&damaged)(offset, buf)
    };
    assert!(matches!(
        read(&mut failing),
        Err(ReadError::NoValidCopy {
            primary: CopyError::Binary(BinaryHeaderError::Checksum),
            secondary: CopyError::Unreadable { offset: 0x4000 },
        })
    ));
}

#[test]
fn misplaced_and_mismatched_copies_are_refused() {
    let image = fixture("luks2-pbkdf2.img");
    // The secondary claims another offset.
    let mut moved = image.clone();
    moved[0x4000 + 256..0x4000 + 264].copy_from_slice(&0x8000u64.to_be_bytes());
    reseal(&mut moved, 0x4000);
    let header = luks2(&moved);
    assert!(matches!(
        header.other_copy_error(),
        Some(CopyError::Misplaced {
            expected: 0x4000,
            found: 0x8000
        })
    ));
    // The primary's slot holds a second secondary copy.
    let mut swapped = image.clone();
    swapped.copy_within(0x4000..0x4006, 0);
    reseal(&mut swapped, 0);
    let header = luks2(&swapped);
    assert_eq!(header.binary.copy, HeaderCopy::Secondary);
    assert!(matches!(
        header.other_copy_error(),
        Some(CopyError::WrongCopy {
            found: HeaderCopy::Secondary
        })
    ));
}

#[test]
fn non_luks_and_short_devices_are_reported() {
    let zeros = vec![0u8; 1 << 20];
    assert!(matches!(read(&mut reader(&zeros)), Err(ReadError::NotLuks)));
    assert!(matches!(
        read(&mut reader(&[0u8; 100])),
        Err(ReadError::Io { offset: 0, .. })
    ));
    // The header reads, but the key material is cut off.
    let image = fixture("luks2-pbkdf2.img");
    let header = luks2(&image);
    let short = &image[..0x9000];
    assert!(matches!(
        header.unlock(&mut reader(short), PASSPHRASE, &Limits::default()),
        Err(UnlockError::Io { offset: 0x8000, .. })
    ));
}

#[test]
fn requirements_block_unlocking() {
    let mut image = fixture("luks2-pbkdf2.img");
    for offset in [0, 0x4000] {
        let json = &mut image[offset + 0x1000..offset + 0x4000];
        let end = json.iter().position(|&b| b == 0).unwrap();
        let text = String::from_utf8(json[..end].to_vec()).unwrap();
        let edited = text.replacen(
            "\"config\":{",
            "\"config\":{\"requirements\":{\"mandatory\":[\"offline-reencrypt\"]},",
            1,
        );
        assert_ne!(edited, text, "the fixture's JSON has a config object");
        json.fill(0);
        json[..edited.len()].copy_from_slice(edited.as_bytes());
        reseal(&mut image, offset);
    }
    let header = luks2(&image);
    assert_eq!(header.metadata.config.requirements, ["offline-reencrypt"]);
    assert!(matches!(
        header.unlock(&mut reader(&image), PASSPHRASE, &Limits::default()),
        Err(UnlockError::Requirement(r)) if r == "offline-reencrypt"
    ));
}

/// Flips random bits in the headers: reading and unlocking may fail, but never panic, and an
/// unlock that succeeds always gives the right key. A single flip damages at most one LUKS2
/// copy, so with one flip a LUKS2 header must always still open.
#[test]
fn corruption_never_panics_or_gives_a_wrong_key() {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    // Flipped iteration counts could ask for millions of rounds; keep the test fast.
    let limits = Limits {
        max_pbkdf2_iterations: 100_000,
        ..Limits::default()
    };
    for (name, key, header_len) in [
        ("luks2-pbkdf2.img", PBKDF2_KEY, 0x8000),
        ("luks1.img", LUKS1_KEY, 592),
    ] {
        let image = fixture(name);
        let key = hex(key);
        for flips in [1, 4] {
            let mut opened = 0;
            for _ in 0..100 {
                let mut damaged = image.clone();
                for _ in 0..flips {
                    let at = (next() % header_len) as usize;
                    damaged[at] ^= 1 << (next() % 8);
                }
                let Ok(header) = read(&mut reader(&damaged)) else {
                    continue;
                };
                if let Ok(unlocked) = header.unlock(&mut reader(&damaged), PASSPHRASE, &limits) {
                    assert_eq!(unlocked.key.as_bytes(), key, "{name}");
                    opened += 1;
                }
            }
            if name.starts_with("luks2") && flips == 1 {
                assert_eq!(
                    opened, 100,
                    "{name}: a single flip must leave one good copy"
                );
            }
        }
    }
}
