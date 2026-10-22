//! An isolated DMA mapping capture, using the fault tracer's existing ring reader.

use std::fs::{File, OpenOptions};
use std::io::Write;

use ferric_trace::EventId;
use ferric_trace::kernel::{IOMMU_MAP, IOMMU_PROTECT, IOMMU_UNMAP};

use super::{Error, Step, Tracer, abi, io_error, path, read_text};
use crate::{Mapped, PAGE, Translator};

const ENABLE: &[u8] = b"enable iommu.map iommu.protect iommu.unmap";
const DISABLE: &[u8] = b"disable iommu.map iommu.protect iommu.unmap";

/// One domain operation collected by the shared ring reader.
pub(super) struct Entry {
    /// The catalog event ID.
    pub(super) event: EventId,
    /// Its raw arguments, in catalog order.
    pub(super) args: Vec<u64>,
    /// Timestamp from the emitting CPU's ring.
    pub(super) timestamp: u64,
}

/// Whether the event describes a completed domain table operation.
pub(super) fn is_event(id: EventId) -> bool {
    [IOMMU_MAP.id(), IOMMU_PROTECT.id(), IOMMU_UNMAP.id()].contains(&id)
}

/// Keeps the original event states even when an operation or validation fails.
struct Controls {
    file: File,
    previous: [bool; 3],
    restored: bool,
}

impl Controls {
    fn set(&mut self, command: &[u8]) -> Result<(), Error> {
        self.file
            .write_all(command)
            .map_err(io_error(&path(abi::EVENTS_PATH)))
    }

    fn restore(&mut self) -> Result<(), Error> {
        for (name, enabled) in ["iommu.map", "iommu.protect", "iommu.unmap"]
            .into_iter()
            .zip(self.previous)
        {
            let command = format!("{} {name}", if enabled { "enable" } else { "disable" });
            self.set(command.as_bytes())?;
        }
        self.restored = true;
        Ok(())
    }
}

impl Drop for Controls {
    fn drop(&mut self) {
        if !self.restored
            && let Err(err) = self.restore()
        {
            eprintln!("restoring DMA trace events: {err}");
        }
    }
}

/// Performs exactly one new map, a cache hit, a permission narrowing and an unmap.
fn operations(mut page: Mapped, translator: &Translator) -> Result<u64, Error> {
    let operation = || -> Result<u64, crate::Failure> {
        let iova = translator.translate(page.addr(0))?;
        let again = translator.translate(page.addr(0))?;
        if again != iova {
            return Err(crate::Failure::Translation(
                "repeat translation moved",
                again,
                iova,
            ));
        }
        page.make_read_only()?;
        page.unmap()?;
        Ok(iova as u64)
    };
    operation().map_err(Error::MappingOperation)
}

fn mismatch(what: &'static str) -> Error {
    Error::MappingMismatch { what }
}

/// Requires one exact operation of each kind, one live domain identity and causal order.
fn verify(entries: &[Entry], iova: u64) -> Result<(), Error> {
    let maps: Vec<_> = entries
        .iter()
        .filter(|entry| entry.event == IOMMU_MAP.id() && entry.args.get(2) == Some(&iova))
        .collect();
    let [map] = &maps[..] else {
        return Err(mismatch("new translation did not produce exactly one map"));
    };
    let &[domain, root, _, phys, bytes, read, write] = &map.args[..] else {
        return Err(mismatch("wrong map argument count"));
    };
    if domain == 0
        || domain > u16::MAX as u64
        || root == 0
        || phys == 0
        || root % PAGE as u64 != 0
        || phys % PAGE as u64 != 0
        || bytes != PAGE as u64
        || read != 1
        || write != 1
    {
        return Err(mismatch("wrong map identity, range or permissions"));
    }
    let own: Vec<_> = entries
        .iter()
        .filter(|e| e.args.get(..2) == Some(&[domain, root]))
        .collect();
    if own.len() != 3 {
        return Err(mismatch(
            "cached translation produced a change, or an operation is missing",
        ));
    }
    let mut after = map.timestamp;
    for (event, args) in [
        (
            IOMMU_PROTECT.id(),
            vec![domain, root, iova, PAGE as u64, 1, 0],
        ),
        (IOMMU_UNMAP.id(), vec![domain, root, iova, PAGE as u64]),
    ] {
        let mut matches = own.iter().filter(|e| e.event == event && e.args == args);
        let entry = matches
            .next()
            .ok_or_else(|| mismatch("missing exact protect/unmap record"))?;
        if matches.next().is_some() || entry.timestamp < after {
            return Err(mismatch("duplicate or unordered protect/unmap record"));
        }
        after = entry.timestamp;
    }
    Ok(())
}

fn run(tracer: &mut Tracer, translator: &Translator, previous: [bool; 3]) -> Result<(), Error> {
    let file = OpenOptions::new()
        .write(true)
        .open(path(abi::EVENTS_PATH))
        .map_err(io_error(&path(abi::EVENTS_PATH)))?;
    let mut controls = Controls {
        file,
        previous,
        restored: false,
    };
    let page = Mapped::dma(PAGE).map_err(Error::MappingOperation)?;
    tracer.drain()?;
    tracer.mappings.clear();
    controls.set(ENABLE)?;
    let result = operations(page, translator);
    // No allocations after unmap until recording is disabled: the IOVA can be reused.
    controls.set(DISABLE)?;
    let iova = result?;
    tracer.drain()?;
    verify(&tracer.mappings, iova)?;
    tracer.mappings.clear();
    operations(
        Mapped::dma(PAGE).map_err(Error::MappingOperation)?,
        translator,
    )?;
    tracer.drain()?;
    if !tracer.mappings.is_empty() {
        return Err(mismatch("a disabled operation was recorded"));
    }
    controls.restore()
}

/// Checks the isolated sequence, or skips when these events are compiled out.
pub(super) fn check(tracer: &mut Tracer, translator: &Translator) -> Step {
    let result = (|| {
        let listing = read_text(abi::EVENTS_PATH)?;
        let mut previous = [false; 3];
        for (index, event) in [IOMMU_MAP, IOMMU_PROTECT, IOMMU_UNMAP].iter().enumerate() {
            let Some(line) = listing
                .lines()
                .filter_map(|s| abi::EventLine::parse(s).ok())
                .find(|line| line.name == event.name())
            else {
                return Ok(Step::Skip("DMA mapping events are not compiled in"));
            };
            previous[index] = line.enabled;
        }
        run(tracer, translator, previous)?;
        Ok(Step::Pass(
            "exact domain/range/permissions, map cache hit, ordered protect/unmap and live disable"
                .into(),
        ))
    })();
    result.unwrap_or_else(Step::Fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture() -> Vec<Entry> {
        vec![
            Entry {
                event: IOMMU_MAP.id(),
                args: vec![5, 0x1000, 0x2000, 0x3000, PAGE as u64, 1, 1],
                timestamp: 10,
            },
            Entry {
                event: IOMMU_PROTECT.id(),
                args: vec![5, 0x1000, 0x2000, PAGE as u64, 1, 0],
                timestamp: 20,
            },
            Entry {
                event: IOMMU_UNMAP.id(),
                args: vec![5, 0x1000, 0x2000, PAGE as u64],
                timestamp: 30,
            },
        ]
    }

    #[test]
    fn exact_capture_accepts_only_the_completed_sequence() {
        assert!(verify(&capture(), 0x2000).is_ok());
        let mut wrong = capture();
        wrong[1].args[5] = 1;
        assert!(verify(&wrong, 0x2000).is_err());
        let mut wrong = capture();
        wrong[1].timestamp = 9;
        assert!(verify(&wrong, 0x2000).is_err());
        let mut wrong = capture();
        wrong[2].args[0] = 6;
        assert!(verify(&wrong, 0x2000).is_err());
        let mut wrong = capture();
        wrong.push(capture().remove(0));
        assert!(verify(&wrong, 0x2000).is_err());
        assert!(verify(&capture()[..2], 0x2000).is_err());
    }
}
