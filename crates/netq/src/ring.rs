//! The four rings of a queue socket, on `ferric-ring`'s queues.
//!
//! The rings share one region of the application's memory, granted to the driver. Its first
//! page holds each ring's indices: the producer's tail and the consumer's head and flags, on
//! separate cache lines so that the two sides do not write the same line. Each ring's slots
//! follow in whole pages, in the order Fill, RX, TX, Completion ([`RingLayout`]).
//!
//! The index protocol, its checks against a hostile peer, and need-wakeup are `ferric-ring`'s
//! [`Producer`] and [`Consumer`], over two entry types:
//!
//! - [`FrameSlot`], one word: a Fill or Completion entry ([`desc::encode_frame`]);
//! - [`PacketSlot`], two words: an RX or TX entry ([`Packet::to_bytes`]).
//!
//! A word holds eight bytes of the entry's little-endian encoding, so the ring memory has the
//! byte layout `desc` describes. Every word pattern decodes into a slot. The consumer checks a
//! slot against its own [`Umem`] when it takes it ([`FrameSlot::frame`], [`PacketSlot::packet`]),
//! after the copy out of shared memory, so the producer cannot change the entry between the
//! check and the use.
//!
//! [`AppRings`] and [`DriverRings`] are the two sides' handles: each side produces on two rings
//! and consumes the other two.

use ferric_ring::abi::{PAGE_SIZE, QueueLayout};
use ferric_ring::{Consumer, Entry, Producer};

use crate::call::RingSize;
use crate::desc::{self, DescError, FRAME_ENTRY_BYTES, PACKET_ENTRY_BYTES, Packet};
use crate::umem::{Frame, FrameError, Umem};

/// A Fill or Completion entry as it sits in the ring: a frame offset, not yet checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FrameSlot([u64; 1]);

impl FrameSlot {
    /// The entry for `frame`.
    pub const fn new(frame: Frame) -> Self {
        Self([u64::from_le_bytes(desc::encode_frame(frame))])
    }

    /// The frame the entry names.
    ///
    /// # Errors
    ///
    /// [`FrameError`] when the offset is not one of `umem`'s frames.
    pub const fn frame(self, umem: &Umem) -> Result<Frame, FrameError> {
        desc::decode_frame(self.0[0].to_le_bytes(), umem)
    }
}

impl Entry for FrameSlot {
    type Words = [u64; 1];
    const WORDS: usize = 1;

    fn to_words(&self) -> Self::Words {
        self.0
    }

    fn from_words(words: &Self::Words) -> Self {
        Self(*words)
    }
}

/// An RX or TX entry as it sits in the ring: a packet descriptor, not yet checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PacketSlot([u64; 2]);

impl PacketSlot {
    /// The entry for `packet`.
    pub fn new(packet: &Packet) -> Self {
        let bytes = packet.to_bytes();
        let mut low = [0; 8];
        let mut high = [0; 8];
        low.copy_from_slice(&bytes[..8]);
        high.copy_from_slice(&bytes[8..]);
        Self([u64::from_le_bytes(low), u64::from_le_bytes(high)])
    }

    /// The packet the entry describes.
    ///
    /// # Errors
    ///
    /// [`DescError`] when the offset, length or flags are not valid for `umem`.
    pub fn packet(self, umem: &Umem) -> Result<Packet, DescError> {
        let mut bytes = [0; PACKET_ENTRY_BYTES];
        bytes[..8].copy_from_slice(&self.0[0].to_le_bytes());
        bytes[8..].copy_from_slice(&self.0[1].to_le_bytes());
        Packet::from_bytes(bytes, umem)
    }
}

impl Entry for PacketSlot {
    type Words = [u64; 2];
    const WORDS: usize = 2;

    fn to_words(&self) -> Self::Words {
        self.0
    }

    fn from_words(words: &Self::Words) -> Self {
        Self(*words)
    }
}

/// Words per cache line, which separates one side's index words from the other's.
const LINE_WORDS: usize = 8;

/// The ring region's layout, computed from the four ring sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RingLayout {
    fill: RingSize,
    rx: RingSize,
    tx: RingSize,
    completion: RingSize,
}

/// The ring with this position in the region.
#[derive(Clone, Copy)]
enum Ring {
    Fill = 0,
    Rx = 1,
    Tx = 2,
    Completion = 3,
}

/// `bytes` rounded up to whole pages; ring sizes are bounded, so this cannot overflow.
const fn page_round(bytes: usize) -> usize {
    bytes.div_ceil(PAGE_SIZE) * PAGE_SIZE
}

impl RingLayout {
    /// The layout of rings with these sizes.
    pub const fn new(fill: RingSize, rx: RingSize, tx: RingSize, completion: RingSize) -> Self {
        Self {
            fill,
            rx,
            tx,
            completion,
        }
    }

    /// The bytes of each ring's slots, in region order.
    const fn slot_bytes(&self) -> [usize; 4] {
        [
            self.fill.get() as usize * FRAME_ENTRY_BYTES,
            self.rx.get() as usize * PACKET_ENTRY_BYTES,
            self.tx.get() as usize * PACKET_ENTRY_BYTES,
            self.completion.get() as usize * FRAME_ENTRY_BYTES,
        ]
    }

    /// The byte offset of `ring`'s first slot.
    const fn offset(&self, ring: Ring) -> usize {
        let bytes = self.slot_bytes();
        let mut offset = PAGE_SIZE;
        let mut i = 0;
        while i < ring as usize {
            offset += page_round(bytes[i]);
            i += 1;
        }
        offset
    }

    const fn queue(&self, ring: Ring, entries: RingSize) -> QueueLayout {
        // Two lines per ring in the header page: the producer's tail on the first, the
        // consumer's head and flags on the second.
        let line = ring as usize * 2 * LINE_WORDS;
        QueueLayout {
            tail: line,
            head: line + LINE_WORDS,
            flags: line + LINE_WORDS + 1,
            base: self.offset(ring) / 8,
            entries: entries.get(),
        }
    }

    /// The Fill ring (application to driver).
    pub const fn fill(&self) -> QueueLayout {
        self.queue(Ring::Fill, self.fill)
    }

    /// The RX ring (driver to application).
    pub const fn rx(&self) -> QueueLayout {
        self.queue(Ring::Rx, self.rx)
    }

    /// The TX ring (application to driver).
    pub const fn tx(&self) -> QueueLayout {
        self.queue(Ring::Tx, self.tx)
    }

    /// The Completion ring (driver to application).
    pub const fn completion(&self) -> QueueLayout {
        self.queue(Ring::Completion, self.completion)
    }

    /// The region's length in bytes, a whole number of pages.
    pub const fn map_len(&self) -> usize {
        self.offset(Ring::Completion) + page_round(self.slot_bytes()[3])
    }

    /// The region's length in 64-bit words.
    pub const fn words(&self) -> usize {
        self.map_len() / 8
    }

    /// The application's side of the rings.
    pub const fn app(&self) -> AppRings {
        AppRings {
            fill: Producer::new(self.fill()),
            rx: Consumer::new(self.rx()),
            tx: Producer::new(self.tx()),
            completion: Consumer::new(self.completion()),
        }
    }

    /// The driver's side of the rings.
    pub const fn driver(&self) -> DriverRings {
        DriverRings {
            fill: Consumer::new(self.fill()),
            rx: Producer::new(self.rx()),
            tx: Consumer::new(self.tx()),
            completion: Producer::new(self.completion()),
        }
    }
}

/// The application's side: it gives frames to receive into and packets to send, and takes
/// received packets and sent frames back.
#[derive(Debug)]
pub struct AppRings {
    /// Empty frames for the driver to receive into.
    pub fill: Producer<FrameSlot>,
    /// Received packets.
    pub rx: Consumer<PacketSlot>,
    /// Packets to send.
    pub tx: Producer<PacketSlot>,
    /// Frames the driver has sent, free to reuse.
    pub completion: Consumer<FrameSlot>,
}

/// The driver's side, the mirror of [`AppRings`].
#[derive(Debug)]
pub struct DriverRings {
    /// Frames to receive into.
    pub fill: Consumer<FrameSlot>,
    /// Packets received for the application.
    pub rx: Producer<PacketSlot>,
    /// Packets the application sends.
    pub tx: Consumer<PacketSlot>,
    /// Frames sent, handed back.
    pub completion: Producer<FrameSlot>,
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::vec::Vec;

    use ferric_ring::notify::{NEED_WAKEUP, Sleep};
    use ferric_ring::queue::RingError;

    use super::*;
    use crate::umem::FrameSize;

    fn size(entries: u32) -> RingSize {
        RingSize::new(entries).expect("a valid ring size")
    }

    fn layout() -> RingLayout {
        RingLayout::new(size(64), size(32), size(16), size(128))
    }

    fn memory(layout: &RingLayout) -> Vec<AtomicU64> {
        (0..layout.words()).map(|_| AtomicU64::new(0)).collect()
    }

    fn umem() -> Umem {
        Umem::new(64 * 2048, FrameSize::Bytes2048).expect("a valid UMEM")
    }

    /// Checks that `layout`'s rings sit apart, after the header page, inside the region.
    fn check_layout(layout: &RingLayout) {
        let queues = [
            (layout.fill(), 1),
            (layout.rx(), 2),
            (layout.tx(), 2),
            (layout.completion(), 1),
        ];
        let mut header = Vec::new();
        let mut slots = Vec::new();
        for (queue, words) in queues {
            header.extend([queue.tail, queue.head, queue.flags]);
            let end = queue.base + queue.entries as usize * words;
            assert!(
                queue.base * 8 >= PAGE_SIZE,
                "slots start after the header page"
            );
            assert_eq!(queue.base * 8 % PAGE_SIZE, 0, "slots start on a page");
            assert!(end <= layout.words(), "slots fit the region");
            slots.push(queue.base..end);
        }
        assert!(header.iter().all(|&word| word < PAGE_SIZE / 8));
        let mut sorted = header.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), header.len(), "every index word is distinct");
        for (i, a) in slots.iter().enumerate() {
            for b in &slots[i + 1..] {
                assert!(a.end <= b.start || b.end <= a.start, "{a:?} and {b:?}");
            }
        }
        assert_eq!(layout.map_len() % PAGE_SIZE, 0);
        // The producer's tail and the consumer's head are on different cache lines.
        for (queue, _) in queues {
            assert_ne!(queue.tail / LINE_WORDS, queue.head / LINE_WORDS);
        }
    }

    #[test]
    fn rings_do_not_overlap_and_fit_the_region() {
        check_layout(&layout());
    }

    #[test]
    fn the_largest_rings_lay_out_as_specified() {
        let max = size(RingSize::MAX);
        let layout = RingLayout::new(max, max, max, max);
        check_layout(&layout);
        // 65,536 slots: 512 KiB of 8-byte frame entries, 1 MiB of 16-byte packet descriptors.
        let frame_ring = (RingSize::MAX as usize) * 8;
        let packet_ring = (RingSize::MAX as usize) * 16;
        let bases = [
            (layout.fill(), PAGE_SIZE),
            (layout.rx(), PAGE_SIZE + frame_ring),
            (layout.tx(), PAGE_SIZE + frame_ring + packet_ring),
            (
                layout.completion(),
                PAGE_SIZE + frame_ring + 2 * packet_ring,
            ),
        ];
        for (queue, base) in bases {
            assert_eq!(queue.base * 8, base);
            assert_eq!(queue.entries, RingSize::MAX);
        }
        let len = PAGE_SIZE + 2 * frame_ring + 2 * packet_ring;
        assert_eq!(layout.map_len(), len);
        assert_eq!(layout.map_len(), 4096 + (3 << 20));
        assert_eq!(layout.words(), len / 8);
        // Filling each ring to the last slot stays inside a region of exactly that size (the
        // slice would refuse a word past it).
        let memory = memory(&layout);
        let mut app = layout.app();
        let mut driver = layout.driver();
        let umem = Umem::new(4 * 2048, FrameSize::Bytes2048).expect("a valid UMEM");
        let frame = umem.frame(3).expect("frame 3");
        for _ in 0..RingSize::MAX {
            app.fill
                .push(&memory[..], &FrameSlot::new(frame))
                .expect("room");
        }
        assert_eq!(app.fill.free(&memory[..]), Ok(0));
        let mut taken = 0;
        while let Some(slot) = driver.fill.pop(&memory[..]).expect("valid") {
            assert_eq!(slot.frame(&umem), Ok(frame));
            taken += 1;
        }
        assert_eq!(taken, RingSize::MAX);
        let packet = Packet::new(&umem, frame, 60).expect("fits");
        for _ in 0..RingSize::MAX {
            driver
                .completion
                .push(&memory[..], &FrameSlot::new(frame))
                .expect("room");
            app.tx
                .push(&memory[..], &PacketSlot::new(&packet))
                .expect("room");
        }
        assert_eq!(app.tx.free(&memory[..]), Ok(0));
        assert_eq!(driver.completion.free(&memory[..]), Ok(0));
    }

    #[test]
    fn frames_and_packets_go_round_the_four_rings() {
        let (layout, umem) = (layout(), umem());
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut app = layout.app();
        let mut driver = layout.driver();

        // The application gives frame 5 to receive into; the driver receives 60 bytes there.
        let frame = umem.frame(5).expect("frame 5");
        app.fill.push(mem, &FrameSlot::new(frame)).expect("room");
        let given = driver.fill.pop(mem).expect("valid").expect("an entry");
        let given = given.frame(&umem).expect("a UMEM frame");
        let rx = Packet::new(&umem, given, 60).expect("fits");
        driver.rx.push(mem, &PacketSlot::new(&rx)).expect("room");
        let got = app.rx.pop(mem).expect("valid").expect("an entry");
        assert_eq!(got.packet(&umem), Ok(rx));

        // The application sends from frame 9; the driver hands frame 9 back.
        let tx = Packet::new(&umem, umem.frame(9).expect("frame 9"), 1500).expect("fits");
        app.tx.push(mem, &PacketSlot::new(&tx)).expect("room");
        let sent = driver.tx.pop(mem).expect("valid").expect("an entry");
        let sent = sent.packet(&umem).expect("valid");
        driver
            .completion
            .push(mem, &FrameSlot::new(sent.frame()))
            .expect("room");
        let done = app.completion.pop(mem).expect("valid").expect("an entry");
        assert_eq!(done.frame(&umem), Ok(tx.frame()));

        // The words in memory are the `desc` byte encoding.
        let slot = layout.rx().base;
        let low = mem[slot].load(Ordering::Relaxed).to_le_bytes();
        let high = mem[slot + 1].load(Ordering::Relaxed).to_le_bytes();
        assert_eq!([low, high].concat(), rx.to_bytes());
    }

    #[test]
    fn a_hostile_application_cannot_get_past_the_driver() {
        let (layout, umem) = (layout(), umem());
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut driver = layout.driver();
        let fill = layout.fill();
        let tx = layout.tx();

        // Entries outside the UMEM, misaligned, too long, or with reserved flags are refused
        // after the copy, one by one, and the ring carries on.
        let raw_fill = [64 * 2048, 2048 + 1, u64::MAX];
        for (i, raw) in raw_fill.into_iter().enumerate() {
            mem[fill.base + i].store(raw, Ordering::Relaxed);
        }
        mem[fill.tail].store(raw_fill.len() as u64, Ordering::Release);
        for _ in raw_fill {
            let slot = driver
                .fill
                .pop(mem)
                .expect("valid indices")
                .expect("an entry");
            assert!(slot.frame(&umem).is_err());
        }
        let raw_tx = [[0, 2049], [0, 0], [0, 64 | (1 << 32)], [4096 * 1024, 64]];
        for (i, [offset, rest]) in raw_tx.into_iter().enumerate() {
            mem[tx.base + 2 * i].store(offset, Ordering::Relaxed);
            mem[tx.base + 2 * i + 1].store(rest, Ordering::Relaxed);
        }
        mem[tx.tail].store(raw_tx.len() as u64, Ordering::Release);
        for _ in raw_tx {
            let slot = driver
                .tx
                .pop(mem)
                .expect("valid indices")
                .expect("an entry");
            assert!(slot.packet(&umem).is_err());
        }

        // A tail that runs more than a ring ahead, or goes backwards, is corruption.
        mem[fill.tail].store(raw_fill.len() as u64 + 65, Ordering::Release);
        assert!(matches!(
            driver.fill.pop(mem),
            Err(RingError::Corrupt { .. })
        ));
        mem[tx.tail].store(0, Ordering::Release);
        assert!(matches!(driver.tx.pop(mem), Err(RingError::Corrupt { .. })));
    }

    #[test]
    fn hostile_ring_contents_never_panic() {
        // Random words everywhere, indices included: every decode either fails cleanly or
        // yields an entry inside the UMEM.
        let (layout, umem) = (layout(), umem());
        let mem = memory(&layout);
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            for word in &mem {
                // Mostly small values, so that some indices and offsets are in range.
                let value = next();
                word.store(
                    if value & 1 == 0 { value % 4096 } else { value },
                    Ordering::Relaxed,
                );
            }
            let mut driver = layout.driver();
            let mut app = layout.app();
            for _ in 0..8 {
                if let Ok(Some(slot)) = driver.fill.pop(&mem[..])
                    && let Ok(frame) = slot.frame(&umem)
                {
                    assert!(frame.offset() < umem.bytes());
                }
                if let Ok(Some(slot)) = driver.tx.pop(&mem[..])
                    && let Ok(packet) = slot.packet(&umem)
                {
                    assert!(packet.frame().offset() < umem.bytes());
                    assert!(packet.len() <= umem.frame_size().bytes());
                }
                let _ = app.rx.pop(&mem[..]);
                let _ = app.completion.pop(&mem[..]);
            }
        }
    }

    #[test]
    fn the_driver_asks_to_be_kicked_before_it_sleeps() {
        let (layout, umem) = (layout(), umem());
        let mem = memory(&layout);
        let mem = &mem[..];
        let mut app = layout.app();
        let driver = layout.driver();
        assert_eq!(driver.tx.prepare_sleep(mem), Ok(Sleep::Sleep));
        assert_ne!(
            mem[layout.tx().flags].load(Ordering::Relaxed) & NEED_WAKEUP,
            0
        );
        let tx = Packet::new(&umem, umem.frame(0).expect("frame 0"), 64).expect("fits");
        app.tx.push(mem, &PacketSlot::new(&tx)).expect("room");
        assert_eq!(
            app.tx.needs_wakeup(mem),
            Ok(true),
            "the application sends Kick"
        );
    }

    #[test]
    fn two_threads_move_every_frame_exactly_once() {
        // The application cycles 64 frames through Fill and RX; the driver receives into
        // each with a length that names the round, from another thread.
        const ROUNDS: u32 = 20_000;
        let layout = RingLayout::new(size(64), size(64), size(8), size(8));
        let umem = umem();
        let mem: Arc<Vec<AtomicU64>> = Arc::new(memory(&layout));
        let driver_mem = Arc::clone(&mem);
        let driver = std::thread::spawn(move || {
            let mut rings = layout.driver();
            let mem = &driver_mem[..];
            let mut round = 0;
            while round < ROUNDS {
                let Some(slot) = rings.fill.pop(mem).expect("valid") else {
                    std::hint::spin_loop();
                    continue;
                };
                let frame = slot
                    .frame(&umem)
                    .expect("the application gives UMEM frames");
                let packet = Packet::new(&umem, frame, round % 2048 + 1).expect("fits");
                while rings.rx.push(mem, &PacketSlot::new(&packet)).is_err() {
                    std::hint::spin_loop();
                }
                round += 1;
            }
        });

        let mut app = layout.app();
        let mem = &mem[..];
        for frame in umem.iter().take(64) {
            app.fill.push(mem, &FrameSlot::new(frame)).expect("room");
        }
        let mut seen = 0;
        while seen < ROUNDS {
            let Some(slot) = app.rx.pop(mem).expect("valid") else {
                std::hint::spin_loop();
                continue;
            };
            let packet = slot.packet(&umem).expect("the driver sends valid packets");
            assert_eq!(
                packet.len(),
                seen % 2048 + 1,
                "in order, none lost or repeated"
            );
            seen += 1;
            if seen + 64 <= ROUNDS {
                app.fill
                    .push(mem, &FrameSlot::new(packet.frame()))
                    .expect("room: the frame just came back");
            }
        }
        driver.join().expect("the driver thread");
        assert_eq!(app.rx.pop(mem), Ok(None));
    }
}
