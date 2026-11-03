//! Fault injection across `memory:translation` installation, teardown and domain close.
//!
//! The model follows the kernel's `DmaSpace` (`arch/x86_64/iommu/dma.rs`): a translation
//! reserves its physical range, pins its frames, allocates an IOVA block and installs it
//! through [`Retained::install`]. A failed strict unmap moves the mapping's block and pins to
//! the same retained set. Closing the domain releases recorded and retained pins once, and
//! only when its devices were blocked.
#![cfg(feature = "alloc")]

use std::collections::BTreeMap;

use ferric_iommu::{
    dma::{DmaMappings, InstallError, Retained, Span, Teardown},
    iova::{IovaAllocator, IovaRange},
};

const PAGE: u64 = 0x1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    /// A table page could not be allocated; `Domain::map` undid its part.
    Tables,
    /// The IOTLB invalidation did not complete.
    Invalidation,
}

/// Frame references, by physical address, as the frame allocator counts them.
#[derive(Default)]
struct Frames(BTreeMap<u64, u32>);

/// References a translation holds on its frames. Dropping it keeps the references, like the
/// kernel's `Pins`.
#[derive(Debug)]
struct Pins(Vec<u64>);

impl Frames {
    fn pin(&mut self, phys: u64, len: u64) -> Pins {
        let frames = (phys..phys + len)
            .step_by(PAGE as usize)
            .collect::<Vec<_>>();
        for &frame in &frames {
            *self.0.entry(frame).or_default() += 1;
        }
        Pins(frames)
    }

    fn release(&mut self, pins: Pins) {
        for frame in pins.0 {
            let count = self.0.get_mut(&frame).expect("a pinned frame is counted");
            *count -= 1;
            if *count == 0 {
                self.0.remove(&frame);
            }
        }
    }

    fn pinned(&self) -> usize {
        self.0.values().map(|&n| n as usize).sum()
    }
}

/// A domain whose next table change can be made to fail.
struct Space {
    iova: IovaAllocator,
    entries: BTreeMap<u64, u64>,
    map_fault: Option<Fault>,
    unmap_fails: bool,
    mappings: DmaMappings<(IovaRange, Pins)>,
    uncertain: Retained<(IovaRange, Pins)>,
}

impl Space {
    fn new() -> Self {
        Self {
            iova: IovaAllocator::new(1 << 32, []),
            entries: BTreeMap::new(),
            map_fault: None,
            unmap_fails: false,
            mappings: DmaMappings::new(),
            uncertain: Retained::new(),
        }
    }

    /// The kernel's `DmaSpace::map`, with the same ownership decisions.
    fn translate(&mut self, frames: &mut Frames, phys: u64, len: u64) -> Result<u64, Fault> {
        let range = Span::new(phys, len);
        let slot = self
            .mappings
            .vacant(range)
            .expect("no overlap in this model");
        let pins = frames.pin(phys, len);
        let block = self.iova.alloc(len).expect("IOVA space");
        let map_fault = &mut self.map_fault;
        let entries = &mut self.entries;
        let installed = self.uncertain.install(
            (block, pins),
            |(block, _)| match map_fault.take() {
                Some(Fault::Tables) => Err(Fault::Tables),
                fault => {
                    entries.insert(block.start, phys);
                    fault.map_or(Ok(()), Err)
                }
            },
            |err| *err == Fault::Tables,
        );
        match installed {
            Ok((block, pins)) => {
                slot.insert(
                    block.start,
                    true,
                    Span::new(0x7000_0000, len),
                    (block, pins),
                );
                Ok(block.start)
            }
            Err(InstallError::Clean {
                error,
                value: (block, pins),
            }) => {
                self.iova.free(block).expect("allocated block");
                frames.release(pins);
                Err(error)
            }
            Err(InstallError::Retained(error)) => Err(error),
        }
    }

    /// The kernel's `DmaSpace::tear_down` for the mapping holding `phys`.
    fn tear_down(&mut self, frames: &mut Frames, phys: u64) -> Result<(), Fault> {
        let mapping = self.mappings.remove(phys).expect("mapped");
        let (block, pins) = mapping.data;
        if self.unmap_fails {
            self.uncertain.insert((block, pins));
            return Err(Fault::Invalidation);
        }
        self.entries.remove(&block.start);
        self.iova.free(block).expect("allocated block");
        frames.release(pins);
        Ok(())
    }

    /// The kernel's `Drop for DmaSpace`.
    fn close(mut self, frames: &mut Frames, teardown: Teardown) {
        for mapping in self.mappings.drain() {
            if teardown == Teardown::Blocked {
                frames.release(mapping.data.1);
            }
        }
        self.uncertain
            .release(teardown, |(_, pins)| frames.release(pins));
    }
}

#[test]
fn a_clean_installation_failure_releases_at_once() {
    let mut frames = Frames::default();
    let mut space = Space::new();
    let free = space.iova.free_bytes();
    space.map_fault = Some(Fault::Tables);
    assert_eq!(
        space.translate(&mut frames, 0x8000_0000, 2 * PAGE),
        Err(Fault::Tables)
    );
    assert_eq!(frames.pinned(), 0);
    assert_eq!(space.iova.free_bytes(), free);
    assert!(space.uncertain.is_empty());
    assert!(space.mappings.is_empty());
    space.translate(&mut frames, 0x8000_0000, 2 * PAGE).unwrap();
    assert_eq!(frames.pinned(), 2);
}

#[test]
fn a_first_installation_with_a_failed_invalidation_stays_owned_until_a_blocked_close() {
    for teardown in [Teardown::Blocked, Teardown::Unblocked] {
        let mut frames = Frames::default();
        let mut space = Space::new();
        space.map_fault = Some(Fault::Invalidation);
        assert_eq!(
            space.translate(&mut frames, 0x8000_0000, 2 * PAGE),
            Err(Fault::Invalidation)
        );
        // No mapping record, but the pins and the block (whose entries a device may reach)
        // are owned by the domain.
        assert!(space.mappings.is_empty());
        assert_eq!(space.uncertain.len(), 1);
        assert_eq!(frames.pinned(), 2);
        assert_eq!(space.entries.len(), 1);
        // The retained block is not handed out again.
        let retained = *space.entries.keys().next().unwrap();
        let next = space.translate(&mut frames, 0x9000_0000, PAGE).unwrap();
        assert_ne!(next, retained);
        assert_eq!(frames.pinned(), 3);

        space.close(&mut frames, teardown);
        let expected = if teardown == Teardown::Blocked { 0 } else { 3 };
        assert_eq!(frames.pinned(), expected, "{teardown:?}");
    }
}

#[test]
fn a_failed_teardown_keeps_its_pins_until_a_blocked_close_releases_them_once() {
    for teardown in [Teardown::Blocked, Teardown::Unblocked] {
        let mut frames = Frames::default();
        let mut space = Space::new();
        space.translate(&mut frames, 0x8000_0000, PAGE).unwrap();
        space.translate(&mut frames, 0xa000_0000, PAGE).unwrap();
        space.unmap_fails = true;
        assert_eq!(
            space.tear_down(&mut frames, 0x8000_0000),
            Err(Fault::Invalidation)
        );
        assert_eq!(space.mappings.len(), 1);
        assert_eq!(space.uncertain.len(), 1);
        assert_eq!(frames.pinned(), 2);

        space.close(&mut frames, teardown);
        let expected = if teardown == Teardown::Blocked { 0 } else { 2 };
        assert_eq!(frames.pinned(), expected, "{teardown:?}");
    }
}

#[test]
fn shared_frames_lose_only_the_released_references() {
    let mut frames = Frames::default();
    // The owner's own reference on the frame.
    let owner = frames.pin(0x8000_0000, PAGE);
    let mut space = Space::new();
    space.map_fault = Some(Fault::Invalidation);
    assert!(space.translate(&mut frames, 0x8000_0000, PAGE).is_err());
    assert_eq!(frames.0.get(&0x8000_0000), Some(&2));
    space.close(&mut frames, Teardown::Blocked);
    assert_eq!(frames.0.get(&0x8000_0000), Some(&1));
    frames.release(owner);
    assert_eq!(frames.pinned(), 0);
}
