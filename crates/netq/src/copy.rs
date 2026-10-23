//! Copy mode: the driver's side of a queue socket served in software.
//!
//! A NIC with no spare hardware queue, or no filter the socket's filters fit in, still serves
//! queue sockets (design section 3, "Copy mode"). The driver matches each received frame
//! against the sockets' filters before netstack sees it ([`CopyQueue::wants`]), copies a
//! matching frame into a frame the application put on the Fill ring and publishes it on RX
//! ([`CopyQueue::receive`]), and sends what the application puts on TX by copying it out of
//! the UMEM, handing each frame back on Completion ([`CopyQueue::transmit`]).
//!
//! The UMEM and the rings are the application's memory, which the driver maps and the
//! application may write at any time. Both are seen here as `[AtomicU64]` ([`Shared`]), so
//! every access is atomic and a hostile peer can change data but never cause undefined
//! behaviour. Frames start on a frame boundary (a multiple of 2 KiB), so a frame's bytes are
//! whole words. Every entry the application writes is checked against the UMEM after it is
//! copied out of the ring ([`crate::ring`]), and a bad one is dropped and counted
//! ([`Stats::invalid_descriptors`]).
//!
//! In copy mode the driver never sleeps on a ring: it looks at Fill when a frame arrives, and
//! at TX when the application sends `Kick`. So it leaves `NEED_WAKEUP` set on TX
//! ([`CopyQueue::transmit`] sets it before it stops), and the application kicks whenever it
//! has queued packets.

use core::sync::atomic::{AtomicU64, Ordering};

use ferric_ring::notify::Sleep;
use ferric_ring::queue::{PushError, RingError};
use thiserror::Error;

use crate::call::{ActiveMode, Bind, BindFlags, FilterId, Stats};
use crate::desc::Packet;
use crate::filter::Filter;
use crate::frame::{Headers, MacAddr};
use crate::own::{FilterOwnError, OwnError, OwnedPort, Ownership, TxReject};
use crate::ring::{DriverRings, FrameSlot, PacketSlot, RingLayout};
use crate::umem::{Frame, Umem, UmemError};

/// The largest frame size, which bounds the bytes [`CopyQueue::transmit`] copies at once.
const MAX_FRAME_BYTES: usize = 4096;

/// The application's memory, as the driver maps it.
#[derive(Clone, Copy, Debug)]
pub struct Shared<'a> {
    /// The ring grant: [`RingLayout::words`] words or more.
    pub rings: &'a [AtomicU64],
    /// The UMEM grant: the UMEM's length in words or more.
    pub umem: &'a [AtomicU64],
}

/// Why a socket cannot be bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum BindError {
    /// The UMEM grant is not a valid UMEM for the frame size.
    #[error("the UMEM grant does not fit the frame size")]
    Umem(#[from] UmemError),
    /// The ring grant is smaller than the rings need.
    #[error("the ring grant holds {have} bytes, the rings need {need}")]
    RingsTooSmall {
        /// The bytes the rings need ([`RingLayout::map_len`]).
        need: usize,
        /// The bytes the grant holds.
        have: usize,
    },
}

/// Why a filter cannot be added or removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FilterTableError {
    /// The socket already has [`CopyQueue::MAX_FILTERS`] filters.
    #[error("the socket has no room for another filter")]
    Full,
    /// No filter of the socket has this ID.
    #[error("the socket has no such filter")]
    Unknown,
    /// The filter names a destination port the socket does not own.
    #[error("the socket does not own the filter's port")]
    NotOwned(#[from] FilterOwnError),
}

/// What became of a received frame [`CopyQueue::receive`] was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Received {
    /// Copied into a Fill frame and published on RX.
    Delivered,
    /// Dropped: the Fill ring was empty ([`Stats::fill_empty_drops`]).
    FillEmpty,
    /// Dropped: the RX ring was full, or the frame was longer than a UMEM frame
    /// ([`Stats::overflow_drops`]).
    Overflow,
    /// Dropped: the Fill entry was not a UMEM frame ([`Stats::invalid_descriptors`]).
    BadFill,
}

/// One queue socket in copy mode: its geometry, its side of the rings, its filters and its
/// counters.
#[derive(Debug)]
pub struct CopyQueue {
    umem: Umem,
    layout: RingLayout,
    rings: DriverRings,
    filters: [Option<(FilterId, Filter)>; Self::MAX_FILTERS],
    next_filter: u64,
    stats: Stats,
    /// The NIC's MAC address, which TX validation requires as the source.
    mac: MacAddr,
    /// Whether TX frames go out unchecked ([`BindFlags::RAW_TX`]).
    raw_tx: bool,
    owned: Ownership,
}

impl CopyQueue {
    /// The most filters a socket may have.
    pub const MAX_FILTERS: usize = 16;

    /// A socket bound with `bind` on the NIC with MAC address `mac`, to a UMEM grant of
    /// `umem_bytes` bytes and a ring grant of `ring_bytes` bytes. It starts with no filters, so
    /// it receives nothing until [`add_filter`](Self::add_filter), and owns no ports, so
    /// without [`BindFlags::RAW_TX`] it sends nothing until [`own`](Self::own).
    ///
    /// # Errors
    ///
    /// [`BindError::Umem`] when the UMEM grant is not a whole number of frames (or holds too
    /// many), and [`BindError::RingsTooSmall`] when the ring grant cannot hold the rings.
    pub fn bind(
        bind: &Bind,
        mac: MacAddr,
        umem_bytes: u64,
        ring_bytes: usize,
    ) -> Result<Self, BindError> {
        let umem = Umem::new(umem_bytes, bind.frame_size)?;
        let layout = RingLayout::new(bind.fill, bind.rx, bind.tx, bind.completion);
        if layout.map_len() > ring_bytes {
            return Err(BindError::RingsTooSmall {
                need: layout.map_len(),
                have: ring_bytes,
            });
        }
        Ok(Self {
            umem,
            layout,
            rings: layout.driver(),
            filters: [None; Self::MAX_FILTERS],
            next_filter: 1,
            stats: Stats {
                mode: ActiveMode::Copy,
                rx_packets: 0,
                tx_packets: 0,
                fill_empty_drops: 0,
                overflow_drops: 0,
                invalid_descriptors: 0,
                tx_rejected: 0,
            },
            mac,
            raw_tx: bind.flags.contains(BindFlags::RAW_TX),
            owned: Ownership::new(),
        })
    }

    /// The UMEM's geometry.
    pub const fn umem(&self) -> &Umem {
        &self.umem
    }

    /// The rings' layout in the ring grant.
    pub const fn layout(&self) -> &RingLayout {
        &self.layout
    }

    /// The socket's counters.
    pub const fn stats(&self) -> Stats {
        self.stats
    }

    /// Records that the socket owns `port` ([`crate::own`]), and returns whether it was new
    /// ([`Ownership::own`]).
    ///
    /// # Errors
    ///
    /// [`OwnError::Full`] when it already owns [`Ownership::MAX_PORTS`] ports.
    pub fn own(&mut self, port: OwnedPort) -> Result<bool, OwnError> {
        self.owned.own(port)
    }

    /// Adds `filter` and returns its ID. IDs are never reused on one socket.
    ///
    /// # Errors
    ///
    /// [`FilterTableError::NotOwned`] for a destination port the socket does not own
    /// ([`Ownership::check_filter`]), and [`FilterTableError::Full`] when the socket has
    /// [`MAX_FILTERS`](Self::MAX_FILTERS).
    pub fn add_filter(&mut self, filter: Filter) -> Result<FilterId, FilterTableError> {
        self.owned.check_filter(&filter)?;
        let slot = self
            .filters
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(FilterTableError::Full)?;
        let id = FilterId::new(self.next_filter);
        self.next_filter += 1;
        *slot = Some((id, filter));
        Ok(id)
    }

    /// Removes filter `id`.
    ///
    /// # Errors
    ///
    /// [`FilterTableError::Unknown`] when the socket has no filter `id`.
    pub fn remove_filter(&mut self, id: FilterId) -> Result<(), FilterTableError> {
        let slot = self
            .filters
            .iter_mut()
            .find(|slot| matches!(slot, Some((have, _)) if *have == id))
            .ok_or(FilterTableError::Unknown)?;
        *slot = None;
        Ok(())
    }

    /// Whether one of the socket's filters matches a frame with `headers`.
    pub fn wants(&self, headers: &Headers) -> bool {
        self.filters
            .iter()
            .flatten()
            .any(|(_, filter)| filter.matches(headers))
    }

    /// Delivers the received `frame`: copies it into the next Fill frame and publishes it on
    /// RX. A frame that cannot be delivered is dropped and counted.
    ///
    /// # Errors
    ///
    /// [`RingError::Corrupt`] when the application has written an impossible index; the
    /// driver should then stop serving the socket.
    pub fn receive(&mut self, mem: Shared<'_>, frame: &[u8]) -> Result<Received, RingError> {
        let fits =
            u32::try_from(frame.len()).is_ok_and(|len| len <= self.umem.frame_size().bytes());
        if !fits || self.rings.rx.free(mem.rings)? == 0 {
            self.stats.overflow_drops += 1;
            return Ok(Received::Overflow);
        }
        let Some(slot) = self.rings.fill.pop(mem.rings)? else {
            self.stats.fill_empty_drops += 1;
            return Ok(Received::FillEmpty);
        };
        let Ok(target) = slot.frame(&self.umem) else {
            self.stats.invalid_descriptors += 1;
            return Ok(Received::BadFill);
        };
        write_frame(mem.umem, target, frame);
        // `fits` checked the length against the frame size, so the packet is valid.
        let len = frame.len() as u32;
        let Ok(packet) = Packet::new(&self.umem, target, len) else {
            self.stats.overflow_drops += 1;
            return Ok(Received::Overflow);
        };
        match self.rings.rx.push(mem.rings, &PacketSlot::new(&packet)) {
            Ok(()) => {
                self.stats.rx_packets += 1;
                Ok(Received::Delivered)
            }
            Err(PushError::Ring(err)) => Err(err),
            // Full: only this side produces on RX, and it had a free slot above.
            Err(_) => {
                self.stats.overflow_drops += 1;
                Ok(Received::Overflow)
            }
        }
    }

    /// Sends up to `budget` packets from the TX ring, calling `send` with each frame's bytes,
    /// and hands each frame back on Completion. It takes a TX entry only when Completion has
    /// room for its frame, so no frame is lost; an entry that is not a valid packet is dropped
    /// and counted. The driver sends only when kicked, so whenever it stops it leaves
    /// `NEED_WAKEUP` set on TX: when TX ran empty, so the application kicks for the next
    /// packets, and also when it stopped with entries left (the budget was used up, or
    /// Completion was full until the application reaps it), so they are not left waiting for
    /// a kick that never comes. A call clears the flag first. Returns the packets `send` took.
    ///
    /// Unless the socket was bound with [`BindFlags::RAW_TX`], each frame passes TX
    /// validation first ([`Ownership::check_tx`]); one that fails is not sent, is counted in
    /// [`Stats::tx_rejected`], and goes back on Completion like a sent one.
    ///
    /// `send` returns whether the NIC took the frame; a frame it refused still goes back on
    /// Completion, unsent.
    ///
    /// # Errors
    ///
    /// [`RingError::Corrupt`] when the application has written an impossible index.
    pub fn transmit(
        &mut self,
        mem: Shared<'_>,
        budget: usize,
        mut send: impl FnMut(&[u8]) -> bool,
    ) -> Result<usize, RingError> {
        let mut sent = 0;
        let mut buf = [0u8; MAX_FRAME_BYTES];
        self.rings.tx.woken(mem.rings)?;
        for _ in 0..budget {
            if self.rings.completion.free(mem.rings)? == 0 {
                self.rings.tx.request_wakeup(mem.rings)?;
                return Ok(sent);
            }
            let Some(slot) = self.next_tx(mem)? else {
                return Ok(sent);
            };
            let Ok(packet) = slot.packet(&self.umem) else {
                self.stats.invalid_descriptors += 1;
                continue;
            };
            let bytes = &mut buf[..packet.len() as usize];
            read_frame(mem.umem, packet.frame(), bytes);
            if self.validate(bytes).is_err() {
                self.stats.tx_rejected += 1;
            } else if send(bytes) {
                sent += 1;
                self.stats.tx_packets += 1;
            }
            // Full cannot happen: Completion had a free slot above, and only this side
            // produces on it.
            if let Err(PushError::Ring(err)) = self
                .rings
                .completion
                .push(mem.rings, &FrameSlot::new(packet.frame()))
            {
                return Err(err);
            }
        }
        // The budget is used up: ask for a kick, whether entries are left or not, so none
        // waits for a kick the application would not send.
        self.rings.tx.request_wakeup(mem.rings)?;
        Ok(sent)
    }

    /// TX validation of `frame`, unless the socket sends raw frames.
    fn validate(&self, frame: &[u8]) -> Result<(), TxReject> {
        if self.raw_tx {
            return Ok(());
        }
        let headers = Headers::parse(frame).map_err(|_| TxReject::Malformed)?;
        self.owned.check_tx(&headers, self.mac)
    }

    /// The next TX entry, or `None` once TX is empty and `NEED_WAKEUP` is set on it.
    fn next_tx(&mut self, mem: Shared<'_>) -> Result<Option<PacketSlot>, RingError> {
        loop {
            if let Some(slot) = self.rings.tx.pop(mem.rings)? {
                return Ok(Some(slot));
            }
            match self.rings.tx.prepare_sleep(mem.rings)? {
                Sleep::Sleep => return Ok(None),
                Sleep::Retry => {}
            }
        }
    }
}

/// A socket a driver offers received frames to, in [`offer`]: in practice a bound
/// [`CopyQueue`] with its shared memory.
pub trait Receiver {
    /// Whether the socket takes frames now. A socket stopped earlier in the same pass (its
    /// rings turned out corrupt) must answer `false`, so it gets none of the later frames.
    fn active(&self) -> bool;
    /// Whether the socket's filters match `headers` ([`CopyQueue::wants`]).
    fn wants(&self, headers: &Headers) -> bool;
    /// Delivers `frame` ([`CopyQueue::receive`]): what became of it, or `None` when a ring
    /// error stopped the socket.
    fn deliver(&mut self, frame: &[u8]) -> Option<Received>;
}

/// Where [`offer`] put a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offered {
    /// No active socket wants it (or it does not parse): it is netstack's.
    Unclaimed,
    /// The socket at `index` wanted it, and [`Receiver::deliver`] said what became of it.
    /// Only [`Received::Delivered`] put anything on RX, so only that is worth a wakeup.
    Taken {
        /// The socket's index in the slice.
        index: usize,
        /// What the delivery did, or `None` when it stopped the socket.
        received: Option<Received>,
    },
}

impl Offered {
    /// The index of the socket to wake: one that got the frame on its RX ring.
    pub const fn woken(self) -> Option<usize> {
        match self {
            Self::Taken {
                index,
                received: Some(Received::Delivered),
            } => Some(index),
            _ => None,
        }
    }
}

/// Gives `frame` to the first active socket of `sockets` whose filters match it.
pub fn offer<R: Receiver>(sockets: &mut [R], frame: &[u8]) -> Offered {
    let Ok(headers) = Headers::parse(frame) else {
        return Offered::Unclaimed;
    };
    let Some(index) = sockets
        .iter()
        .position(|socket| socket.active() && socket.wants(&headers))
    else {
        return Offered::Unclaimed;
    };
    let received = sockets
        .get_mut(index)
        .and_then(|socket| socket.deliver(frame));
    Offered::Taken { index, received }
}

/// Copies `bytes` into `frame` of the UMEM `words`, with atomic word stores. The tail of the
/// last word is zeroed; it lies inside the frame, since frames are whole words. Both sides use
/// it: the driver for received frames, the application for frames it sends.
///
/// Words past the end of `words` are skipped, so a short slice cannot make it panic; callers
/// check `bytes.len()` against the frame size first.
///
/// # Examples
///
/// ```
/// use core::sync::atomic::AtomicU64;
/// use ferric_netq::copy::{read_frame, write_frame};
/// use ferric_netq::umem::{FrameSize, Umem};
///
/// let umem = Umem::new(4 * 2048, FrameSize::Bytes2048).expect("a valid UMEM");
/// let words: Vec<AtomicU64> = (0..umem.bytes() / 8).map(|_| AtomicU64::new(0)).collect();
/// let frame = umem.frame(2).expect("frame 2");
/// write_frame(&words, frame, b"hello, queue");
/// let mut back = [0; 12];
/// read_frame(&words, frame, &mut back);
/// assert_eq!(&back, b"hello, queue");
/// ```
pub fn write_frame(words: &[AtomicU64], frame: Frame, bytes: &[u8]) {
    let first = word_index(frame);
    for (i, chunk) in bytes.chunks(8).enumerate() {
        let mut le = [0u8; 8];
        le[..chunk.len()].copy_from_slice(chunk);
        if let Some(word) = words.get(first + i) {
            word.store(u64::from_le_bytes(le), Ordering::Relaxed);
        }
    }
}

/// Copies the first `bytes.len()` bytes of `frame` of the UMEM `words` into `bytes`, with
/// atomic word loads; words past the end of `words` read as zero. See [`write_frame`].
pub fn read_frame(words: &[AtomicU64], frame: Frame, bytes: &mut [u8]) {
    let first = word_index(frame);
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
        let word = words
            .get(first + i)
            .map_or(0, |word| word.load(Ordering::Relaxed));
        chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
    }
}

/// The index of `frame`'s first word.
fn word_index(frame: Frame) -> usize {
    // A UMEM holds at most 2^20 frames of at most 4 KiB, so the offset fits in a usize.
    (frame.offset() / 8) as usize
}

#[cfg(test)]
mod tests {
    extern crate std;

    use core::net::IpAddr;
    use std::vec::Vec;

    use super::*;
    use crate::call::{Mode, RingSize};
    use crate::filter::{FilterSpec, MatchSpec};
    use crate::frame::tests::{PEER, ethernet, ipv4, udp};
    use crate::frame::{ETHERTYPE_IPV4, IP_PROTOCOL_UDP};
    use crate::own::{OwnedPort, Transport};
    use crate::ring::AppRings;
    use crate::umem::FrameSize;
    use ferric_ring::Entry;

    const UMEM_BYTES: u64 = 64 * 2048;

    fn size(entries: u32) -> RingSize {
        RingSize::new(entries).expect("a valid ring size")
    }

    fn bind() -> Bind {
        Bind {
            frame_size: FrameSize::Bytes2048,
            fill: size(8),
            rx: size(8),
            tx: size(8),
            completion: size(8),
            mode: Mode::Copy,
            cpu: 0,
            flags: BindFlags::empty(),
        }
    }

    /// The NIC's MAC: the source of the frames the tests build, so they pass TX validation.
    const NIC: MacAddr = MacAddr(PEER);

    /// The source port of [`udp_frame`]s.
    const SRC_PORT: u16 = 5000;

    /// The address [`udp_frame`]s come from: the socket's own address.
    const LOCAL: [u8; 4] = [10, 0, 0, 9];

    /// The address [`udp_frame`]s go to, which receive filters name.
    const DEST: [u8; 4] = [10, 0, 0, 1];

    fn udp_port(addr: [u8; 4], port: u16) -> OwnedPort {
        OwnedPort {
            transport: Transport::Udp,
            addr: IpAddr::from(addr),
            port,
        }
    }

    struct App {
        queue: CopyQueue,
        app: AppRings,
        rings: Vec<AtomicU64>,
        umem: Vec<AtomicU64>,
    }

    impl App {
        /// A socket owning the UDP ports the tests send from and filter on.
        fn new() -> Self {
            let mut t = Self::unowned(bind());
            t.queue.own(udp_port(LOCAL, SRC_PORT)).expect("room");
            for port in [7, 8, 9] {
                t.queue.own(udp_port(DEST, port)).expect("room");
            }
            t
        }

        fn unowned(bind: Bind) -> Self {
            let layout = RingLayout::new(bind.fill, bind.rx, bind.tx, bind.completion);
            let queue = CopyQueue::bind(&bind, NIC, UMEM_BYTES, layout.map_len()).expect("binds");
            Self {
                queue,
                app: layout.app(),
                rings: (0..layout.words()).map(|_| AtomicU64::new(0)).collect(),
                umem: (0..UMEM_BYTES / 8).map(|_| AtomicU64::new(0)).collect(),
            }
        }

        fn receive(&mut self, frame: &[u8]) -> Received {
            let mem = Shared {
                rings: &self.rings,
                umem: &self.umem,
            };
            self.queue.receive(mem, frame).expect("valid rings")
        }

        fn fill(&mut self, frames: impl IntoIterator<Item = Frame>) {
            for frame in frames {
                self.app
                    .fill
                    .push(&self.rings[..], &FrameSlot::new(frame))
                    .expect("room");
            }
        }

        fn frames(&self) -> Vec<Frame> {
            self.queue.umem().iter().collect()
        }
    }

    fn udp_frame(port: u16) -> Vec<u8> {
        let ip = ipv4(IP_PROTOCOL_UDP, LOCAL, DEST, 0, &udp(SRC_PORT, port));
        ethernet(&[], ETHERTYPE_IPV4, &ip)
    }

    fn port_filter(port: u16) -> Filter {
        Filter::new(FilterSpec::Match(MatchSpec {
            ip_protocol: Some(IP_PROTOCOL_UDP),
            dst_port: Some(port),
            dst_addr: Some(IpAddr::from(DEST)),
            ..MatchSpec::default()
        }))
        .expect("valid")
    }

    #[test]
    fn binding_checks_the_grants() {
        let layout = RingLayout::new(size(8), size(8), size(8), size(8));
        assert_eq!(
            CopyQueue::bind(&bind(), NIC, 3000, layout.map_len()).map(|_| ()),
            Err(BindError::Umem(UmemError::Unaligned { bytes: 3000 }))
        );
        assert_eq!(
            CopyQueue::bind(&bind(), NIC, UMEM_BYTES, layout.map_len() - 1).map(|_| ()),
            Err(BindError::RingsTooSmall {
                need: layout.map_len(),
                have: layout.map_len() - 1
            })
        );
    }

    #[test]
    fn filters_select_frames() {
        let mut t = App::new();
        let echo = udp_frame(7);
        let other = udp_frame(8);
        let parse = |f: &[u8]| Headers::parse(f).expect("well-formed");
        assert!(!t.queue.wants(&parse(&echo)), "no filters, no frames");
        let id = t.queue.add_filter(port_filter(7)).expect("room");
        assert!(t.queue.wants(&parse(&echo)));
        assert!(!t.queue.wants(&parse(&other)));
        t.queue.remove_filter(id).expect("added");
        assert!(!t.queue.wants(&parse(&echo)));
        assert_eq!(t.queue.remove_filter(id), Err(FilterTableError::Unknown));
        for _ in 0..CopyQueue::MAX_FILTERS {
            let again = t.queue.add_filter(port_filter(7)).expect("room");
            assert_ne!(again, id, "IDs are not reused");
        }
        assert_eq!(
            t.queue.add_filter(port_filter(7)),
            Err(FilterTableError::Full)
        );
    }

    #[test]
    fn received_frames_reach_the_application_byte_for_byte() {
        let mut t = App::new();
        let frames = t.frames();
        t.fill(frames[..2].iter().copied());
        // Lengths that end mid-word, and the largest that fits.
        for frame in [udp_frame(7), Vec::from([0xa5u8; 2048])] {
            assert_eq!(t.receive(&frame), Received::Delivered);
            let slot = t
                .app
                .rx
                .pop(&t.rings[..])
                .expect("valid")
                .expect("an entry");
            let packet = slot.packet(t.queue.umem()).expect("valid");
            let mut got = std::vec![0u8; packet.len() as usize];
            read_frame(&t.umem, packet.frame(), &mut got);
            assert_eq!(got, frame);
        }
        assert_eq!(t.queue.stats().rx_packets, 2);
    }

    #[test]
    fn undeliverable_frames_are_dropped_and_counted() {
        let mut t = App::new();
        let frame = udp_frame(7);
        assert_eq!(t.receive(&frame), Received::FillEmpty);
        // Too long for a frame: dropped without taking a Fill entry.
        let frames = t.frames();
        t.fill([frames[0]]);
        assert_eq!(t.receive(&[0; 2049]), Received::Overflow);
        assert_eq!(t.receive(&frame), Received::Delivered);
        // A Fill entry inside the UMEM but not on a frame boundary.
        t.app
            .fill
            .push(&t.rings[..], &FrameSlot::from_words(&[100]))
            .expect("room");
        assert_eq!(t.receive(&frame), Received::BadFill);
        // RX full: eight undelivered packets fill it, and the Fill entry stays put.
        let more: Vec<Frame> = frames[1..9].to_vec();
        t.fill(more[..7].iter().copied());
        for _ in 0..7 {
            assert_eq!(t.receive(&frame), Received::Delivered);
        }
        t.fill([more[7]]);
        assert_eq!(t.receive(&frame), Received::Overflow);
        assert_eq!(t.app.fill.free(&t.rings[..]).expect("valid"), 7);
        let stats = t.queue.stats();
        assert_eq!(
            (
                stats.rx_packets,
                stats.fill_empty_drops,
                stats.overflow_drops,
                stats.invalid_descriptors
            ),
            (8, 1, 2, 1)
        );
        assert_eq!(stats.mode, ActiveMode::Copy);
    }

    #[test]
    fn transmit_sends_and_hands_frames_back() {
        let mut t = App::new();
        let frames = t.frames();
        let payloads = [udp_frame(7), udp_frame(9)];
        for (frame, bytes) in frames.iter().zip(&payloads) {
            write_frame(&t.umem, *frame, bytes);
            let packet = Packet::new(t.queue.umem(), *frame, bytes.len() as u32).expect("fits");
            t.app
                .tx
                .push(&t.rings[..], &PacketSlot::new(&packet))
                .expect("room");
        }
        // A bad entry between them is dropped.
        t.app
            .tx
            .push(&t.rings[..], &PacketSlot::from_words(&[UMEM_BYTES, 64]))
            .expect("room");
        let mut wire = Vec::new();
        let mem = Shared {
            rings: &t.rings,
            umem: &t.umem,
        };
        let sent = t
            .queue
            .transmit(mem, 16, |bytes| {
                wire.push(bytes.to_vec());
                true
            })
            .expect("valid rings");
        assert_eq!(sent, 2);
        assert_eq!(wire, payloads);
        let back: Vec<Frame> = (0..2)
            .map(|_| {
                t.app
                    .completion
                    .pop(&t.rings[..])
                    .expect("valid")
                    .expect("an entry")
                    .frame(t.queue.umem())
                    .expect("valid")
            })
            .collect();
        assert_eq!(back, frames[..2]);
        assert_eq!(t.queue.stats().invalid_descriptors, 1);
        // TX ran empty, so the driver asks to be kicked.
        assert!(t.app.tx.needs_wakeup(&t.rings[..]).expect("valid"));
    }

    #[test]
    fn filters_need_the_port_owned() {
        let mut t = App::unowned(bind());
        assert_eq!(
            t.queue.add_filter(port_filter(7)),
            Err(FilterTableError::NotOwned(FilterOwnError::NotOwned))
        );
        t.queue.own(udp_port(DEST, 7)).expect("room");
        assert!(t.queue.add_filter(port_filter(7)).is_ok());
    }

    /// Puts `frames` on TX, transmits, and returns what reached the wire and how many frames
    /// came back on Completion.
    fn send_all(t: &mut App, frames: &[Vec<u8>]) -> (Vec<Vec<u8>>, usize) {
        for (slot, bytes) in t.frames().into_iter().zip(frames) {
            write_frame(&t.umem, slot, bytes);
            let packet = Packet::new(t.queue.umem(), slot, bytes.len() as u32).expect("fits");
            t.app
                .tx
                .push(&t.rings[..], &PacketSlot::new(&packet))
                .expect("room");
        }
        let mut wire = Vec::new();
        let mem = Shared {
            rings: &t.rings,
            umem: &t.umem,
        };
        t.queue
            .transmit(mem, 16, |bytes| {
                wire.push(bytes.to_vec());
                true
            })
            .expect("valid rings");
        let mut back = 0;
        while t.app.completion.pop(&t.rings[..]).expect("valid").is_some() {
            back += 1;
        }
        (wire, back)
    }

    #[test]
    fn tx_validation_drops_what_the_socket_may_not_send() {
        let good = udp_frame(7);
        let mut spoofed_mac = good.clone();
        spoofed_mac[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 0x66]);
        let ip = ipv4(IP_PROTOCOL_UDP, LOCAL, DEST, 0, &udp(6000, 7));
        let unowned_port = ethernet(&[], ETHERTYPE_IPV4, &ip);
        // The right port from an address the socket does not own.
        let ip = ipv4(IP_PROTOCOL_UDP, [10, 0, 0, 99], DEST, 0, &udp(SRC_PORT, 7));
        let spoofed_ip = ethernet(&[], ETHERTYPE_IPV4, &ip);
        let arp = ethernet(&[], 0x0806, &[0; 28]);
        let runt = [0u8; 10].to_vec();
        let frames = [
            good.clone(),
            spoofed_mac,
            unowned_port,
            spoofed_ip,
            arp,
            runt,
        ];

        let mut t = App::new();
        let (wire, back) = send_all(&mut t, &frames);
        assert_eq!(wire, [good]);
        assert_eq!(back, frames.len(), "rejected frames still come back");
        assert_eq!(t.queue.stats().tx_packets, 1);
        assert_eq!(t.queue.stats().tx_rejected, 5);

        let mut raw = App::unowned(Bind {
            flags: BindFlags::RAW_TX,
            ..bind()
        });
        let (wire, back) = send_all(&mut raw, &frames);
        assert_eq!(wire, frames, "a raw socket sends everything");
        assert_eq!(back, frames.len());
        assert_eq!(raw.queue.stats().tx_rejected, 0);
    }

    #[test]
    fn transmit_respects_the_budget_and_a_full_completion_ring() {
        // The frames are zeros, not valid traffic, so the socket sends raw frames.
        let mut t = App::unowned(Bind {
            flags: BindFlags::RAW_TX,
            ..bind()
        });
        let frames = t.frames();
        for frame in &frames[..8] {
            let packet = Packet::new(t.queue.umem(), *frame, 60).expect("fits");
            t.app
                .tx
                .push(&t.rings[..], &PacketSlot::new(&packet))
                .expect("room");
        }
        let mem = Shared {
            rings: &t.rings,
            umem: &t.umem,
        };
        assert_eq!(t.queue.transmit(mem, 3, |_| true), Ok(3));
        // The budget ran out with entries left: the driver still asks to be kicked, or they
        // would wait for good.
        assert!(t.app.tx.needs_wakeup(&t.rings[..]).expect("valid"));
        // A refused frame still comes back, unsent.
        assert_eq!(t.queue.transmit(mem, 1, |_| false), Ok(0));
        assert_eq!(t.queue.transmit(mem, 16, |_| true), Ok(4));
        // Completion is full (8 entries the application has not taken): nothing more is
        // taken from TX, so no frame is lost.
        let packet = Packet::new(t.queue.umem(), frames[8], 60).expect("fits");
        t.app
            .tx
            .push(&t.rings[..], &PacketSlot::new(&packet))
            .expect("room");
        assert_eq!(t.queue.transmit(mem, 16, |_| true), Ok(0));
        assert_eq!(t.app.tx.free(&t.rings[..]).expect("valid"), 7);
        assert_eq!(t.queue.stats().tx_packets, 7);
        // It stopped with an entry left: the application is asked to kick again, and once it
        // reaps Completion the next kick sends the entry.
        assert!(t.app.tx.needs_wakeup(&t.rings[..]).expect("valid"));
        while t.app.completion.pop(&t.rings[..]).expect("valid").is_some() {}
        assert_eq!(t.queue.transmit(mem, 16, |_| true), Ok(1));
        assert_eq!(t.queue.stats().tx_packets, 8);
        assert!(t.app.tx.needs_wakeup(&t.rings[..]).expect("valid"));
    }

    /// A socket for [`offer`]: wants one port, and stops on the frame `stop_on`.
    struct FakeSocket {
        port: u16,
        stopped: bool,
        full: bool,
        stop_on: Option<usize>,
        got: Vec<u16>,
        seen: usize,
    }

    impl FakeSocket {
        fn new(port: u16) -> Self {
            Self {
                port,
                stopped: false,
                full: false,
                stop_on: None,
                got: Vec::new(),
                seen: 0,
            }
        }
    }

    impl Receiver for FakeSocket {
        fn active(&self) -> bool {
            !self.stopped
        }
        fn wants(&self, headers: &Headers) -> bool {
            port_filter(self.port).matches(headers)
        }
        fn deliver(&mut self, _frame: &[u8]) -> Option<Received> {
            self.seen += 1;
            if self.stop_on == Some(self.seen) {
                self.stopped = true;
                return None;
            }
            if self.full {
                return Some(Received::FillEmpty);
            }
            self.got.push(self.port);
            Some(Received::Delivered)
        }
    }

    #[test]
    fn offer_skips_a_socket_stopped_earlier_in_the_pass() {
        // Two sockets want port 7; the first stops on its second frame.
        let mut first = FakeSocket::new(7);
        first.stop_on = Some(2);
        let mut sockets = [first, FakeSocket::new(7), FakeSocket::new(9)];
        let frames = [
            udp_frame(7),
            udp_frame(7),
            udp_frame(7),
            udp_frame(9),
            udp_frame(8),
        ];
        let offered: Vec<Offered> = frames
            .iter()
            .map(|frame| offer(&mut sockets, frame))
            .collect();
        assert_eq!(offered[0].woken(), Some(0));
        // The frame that stopped socket 0 woke nobody.
        assert_eq!(
            offered[1],
            Offered::Taken {
                index: 0,
                received: None
            }
        );
        assert_eq!(offered[1].woken(), None);
        // Later frames of the same pass go past the stopped socket.
        assert_eq!(offered[2].woken(), Some(1));
        assert_eq!(offered[3].woken(), Some(2));
        assert_eq!(offered[4], Offered::Unclaimed);
        assert_eq!(sockets[0].seen, 2);
        assert_eq!(sockets[1].got, [7]);
        assert_eq!(offer(&mut sockets, b"not a frame"), Offered::Unclaimed);
    }

    #[test]
    fn a_dropped_frame_wakes_nobody() {
        let mut socket = FakeSocket::new(7);
        socket.full = true;
        let mut sockets = [socket];
        let offered = offer(&mut sockets, &udp_frame(7));
        assert_eq!(
            offered,
            Offered::Taken {
                index: 0,
                received: Some(Received::FillEmpty)
            }
        );
        assert_eq!(offered.woken(), None);
    }

    #[test]
    fn a_corrupt_ring_is_reported() {
        let mut t = App::new();
        let fill = t.queue.layout().fill();
        t.rings[fill.tail].store(1 << 40, Ordering::Release);
        let mem = Shared {
            rings: &t.rings,
            umem: &t.umem,
        };
        assert!(matches!(
            t.queue.receive(mem, &udp_frame(7)),
            Err(RingError::Corrupt { .. })
        ));
    }

    #[test]
    fn hostile_memory_never_panics() {
        let t = App::new();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let frame = udp_frame(7);
        for _ in 0..500 {
            for word in &t.rings {
                let value = next();
                word.store(
                    if value & 1 == 0 { value % 64 } else { value },
                    Ordering::Relaxed,
                );
            }
            let mut queue =
                CopyQueue::bind(&bind(), NIC, UMEM_BYTES, t.rings.len() * 8).expect("binds");
            let mem = Shared {
                rings: &t.rings,
                umem: &t.umem,
            };
            let _ = queue.receive(mem, &frame);
            let _ = queue.transmit(mem, 8, |bytes| bytes.len() <= 2048);
        }
    }
}
