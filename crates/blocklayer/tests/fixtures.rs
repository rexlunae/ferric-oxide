//! Stacks over images made by cryptsetup, lvm2 and OpenSSL (see `fixtures/make-fixtures.sh`):
//! every volume holds a known pattern, so these tests check decrypted and mapped bytes against
//! data no Ferric Oxide code wrote. Nothing here depends on the random keys, salts or UUIDs of
//! one run of the script, so the tests pass on freshly made images too.

use std::error::Error;

use ferric_blocklayer::{
    BlockDevice, Crypt, CryptOpenError, Limits, Lvm, LvmOpenError, Memory, OutOfRange, Slice,
    Stack, StackError,
};
use ferric_luks::UnlockError;

const PASSPHRASE: &[u8] = b"correct horse";
/// How many bytes of each encrypted volume the script kept.
const KEPT: usize = 8192;
const KIB: u64 = 1024;

/// An image from `tests/fixtures/`, or from `$BLOCKLAYER_FIXTURES` when it is set (CI points it
/// at images `make-fixtures.sh` has just made, to check against the tools as they are now).
fn fixture(name: &str) -> Vec<u8> {
    let dir = std::env::var("BLOCKLAYER_FIXTURES")
        .unwrap_or_else(|_| format!("{}/tests/fixtures", env!("CARGO_MANIFEST_DIR")));
    std::fs::read(format!("{dir}/{name}")).expect("fixture")
}

/// Bytes `start..start + len` of a volume filled with the pattern for `seed`.
fn pattern(seed: u64, start: u64, len: usize) -> Vec<u8> {
    (start..start + len as u64)
        .map(|i| ((i * 7 + i / 4096 + seed) % 251) as u8)
        .collect()
}

fn read<D: BlockDevice>(device: &D, offset: u64, len: usize) -> Vec<u8> {
    let mut buf = vec![0; len];
    device.read_at(offset, &mut buf).expect("read");
    buf
}

/// Every source in an error's chain, top first, as text.
fn chain(error: &dyn Error) -> Vec<String> {
    let mut out = vec![error.to_string()];
    let mut next = error.source();
    while let Some(e) = next {
        out.push(e.to_string());
        next = e.source();
    }
    out
}

type Image = Stack<Memory<Vec<u8>>>;

fn image(name: &str) -> Image {
    Stack::Device(Memory::new(fixture(name)))
}

#[test]
fn a_logical_volume_reads_across_its_segments() {
    let lv = Lvm::open(vec![Memory::new(fixture("lvm.img"))], "data").expect("opens");
    assert_eq!(lv.group().name, "blkvg");
    assert_eq!(lv.volume().segments.len(), 2);
    // PE 0-1, then PE 7-8: four 64 KiB extents.
    assert_eq!(lv.size(), 256 * KIB);
    assert_eq!(read(&lv, 0, 256 * 1024), pattern(4, 0, 256 * 1024));
    // Across the jump from PE 1 to PE 7.
    let at = 128 * KIB - 100;
    assert_eq!(read(&lv, at, 300), pattern(4, at, 300));
    assert!(lv.read_at(256 * KIB - 1, &mut [0; 2]).is_err());
}

/// LUKS2 with 512-byte sectors, inside a logical volume.
#[test]
fn luks_on_lvm_decrypts() {
    let lv = Stack::lvm(vec![image("lvm.img")], "sealed").expect("lv");
    assert_eq!(lv.size(), 5 * 64 * KIB);
    let volume = lv.crypt(PASSPHRASE, &Limits::default()).expect("unlocks");
    // The LUKS2 data starts 256 KiB into the 320 KiB volume.
    assert_eq!(volume.size(), 64 * KIB);
    assert_eq!(read(&volume, 0, KEPT), pattern(5, 0, KEPT));
    // Partial sectors at both ends.
    assert_eq!(read(&volume, 100, 1000), pattern(5, 100, 1000));
    assert_eq!(read(&volume, 511, 2), pattern(5, 511, 2));
}

/// LVM inside LUKS2 with 4096-byte sectors: the IVs of large sectors count 512-byte units.
#[test]
fn lvm_on_luks_with_4k_sectors_decrypts() {
    let luks = Crypt::open(
        Memory::new(fixture("lvm-on-luks.img")),
        PASSPHRASE,
        &Limits::default(),
    )
    .expect("unlocks");
    assert_eq!(luks.segment().sector_size, 4096);
    assert_eq!(luks.segment().offset, 256 * KIB);
    assert_eq!(luks.keyslot(), 0);
    assert_eq!(luks.size(), 768 * KIB);
    let lv = Lvm::open(vec![&luks], "inner").expect("lv");
    assert_eq!(lv.size(), 64 * KIB);
    // The LV is PE 1, 64 KiB past the start of the PV's data area, so its first 8 KiB are
    // 4 KiB sectors 32 and 33 of the segment: IVs 256 and 264. Counting IVs in 4 KiB units
    // (32 and 33) would decrypt to noise.
    assert_eq!(read(&lv, 0, KEPT), pattern(6, 0, KEPT));
    assert_eq!(read(&lv, 4000, 200), pattern(6, 4000, 200));
    assert_eq!(read(&lv, 4095, 1), pattern(6, 4095, 1));
}

/// LUKS1 data was encrypted by OpenSSL, not cryptsetup, with the header's volume key: a
/// second, independent check of plain64 from IV 0.
#[test]
fn luks1_decrypts() {
    // The header is ferric-luks's committed fixture; the data was encrypted with its key.
    let header = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../luks/tests/fixtures/luks1.img"
    );
    let mut disk = std::fs::read(header).expect("LUKS1 header");
    // The header puts the payload at sector 4096.
    disk.resize(4096 * 512, 0);
    disk.extend(fixture("luks1-data.bin"));
    let volume = Crypt::open(
        Slice::new(Memory::new(&disk), 0, disk.len() as u64).expect("whole disk"),
        PASSPHRASE,
        &Limits::default(),
    )
    .expect("unlocks");
    assert_eq!(volume.segment().sector_size, 512);
    assert_eq!(volume.size(), KEPT as u64);
    assert_eq!(read(&volume, 0, KEPT), pattern(3, 0, KEPT));
    assert_eq!(read(&volume, 1000, 3000), pattern(3, 1000, 3000));
}

#[test]
fn a_wrong_passphrase_is_refused() {
    let error = Crypt::open(
        Memory::new(fixture("lvm-on-luks.img")),
        b"wrong",
        &Limits::default(),
    )
    .expect_err("refused");
    assert!(matches!(
        error,
        CryptOpenError::Unlock(UnlockError::NoMatch)
    ));
    assert_eq!(
        chain(&error),
        [
            "the LUKS volume key cannot be recovered",
            "the passphrase does not open any LUKS keyslot"
        ]
    );
}

#[test]
fn a_device_that_is_not_luks_is_refused() {
    let error = Crypt::open(
        Memory::new(fixture("lvm.img")),
        PASSPHRASE,
        &Limits::default(),
    )
    .expect_err("not LUKS");
    assert!(matches!(error, CryptOpenError::Header(_)));
}

#[test]
fn a_cut_short_device_is_refused_or_fails_cleanly() {
    let mut image = fixture("lvm-on-luks.img");
    // The header says the segment runs to the end of the device, so a shorter device is a
    // shorter volume; cut the LV's data off and the LV no longer fits.
    image.truncate(256 * 1024 + 64 * 1024 + 4096);
    let luks = Crypt::open(Memory::new(image), PASSPHRASE, &Limits::default()).expect("opens");
    assert_eq!(luks.size(), 68 * KIB);
    let lv = Lvm::open(vec![luks], "inner").expect("lv metadata is still there");
    let error = lv.read_at(0, &mut [0; 16]).expect_err("past the device");
    assert_eq!(
        chain(&error),
        [
            "reading physical volume 0 failed",
            "16 bytes at 0x20000 are not inside the 69632-byte device"
        ]
    );
}

#[test]
fn logical_volumes_need_their_whole_group() {
    let pv = || Memory::new(fixture("lvm.img"));
    assert!(matches!(
        Lvm::open(vec![pv()], "nonesuch"),
        Err(LvmOpenError::NoVolume)
    ));
    assert!(matches!(
        Lvm::open(vec![pv(), pv()], "data"),
        Err(LvmOpenError::DuplicatePv { .. })
    ));
    assert!(matches!(
        Lvm::open(Vec::<Memory<Vec<u8>>>::new(), "data"),
        Err(LvmOpenError::NoMetadata)
    ));
    let not_a_pv = Memory::new(vec![0u8; 4096]);
    assert!(matches!(
        Lvm::open(vec![Memory::new(fixture("lvm.img")), not_a_pv], "data"),
        Err(LvmOpenError::Pv { index: 1, .. })
    ));
}

#[test]
fn physical_volumes_of_another_group_are_refused() {
    let blkvg = Stack::Device(Memory::new(fixture("lvm.img")));
    let innervg = Stack::Crypt(Box::new(
        Crypt::open(
            Stack::Device(Memory::new(fixture("lvm-on-luks.img"))),
            PASSPHRASE,
            &Limits::default(),
        )
        .expect("unlocks"),
    ));
    assert!(matches!(
        Stack::lvm(vec![blkvg, innervg], "data"),
        Err(LvmOpenError::SeveralGroups)
    ));
}

#[test]
fn stack_errors_keep_every_layer() {
    let lv = Stack::lvm(vec![image("lvm.img")], "sealed").expect("lv");
    let volume = lv
        .crypt(PASSPHRASE, &Limits::default())
        .expect("unlocks")
        .slice(4096, 4096)
        .expect("fits");
    assert_eq!(read(&volume, 0, 4096), pattern(5, 4096, 4096));
    let error: StackError<OutOfRange> = volume.read_at(4000, &mut [0; 200]).unwrap_err();
    assert!(error.is_out_of_range());
    assert_eq!(
        chain(&error),
        ["200 bytes at 0xfa0 are not inside the 4096-byte device"]
    );
}
