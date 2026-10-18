//! Fault injection across grant installation, revocation and domain-close bookkeeping.
#![cfg(feature = "alloc")]

use std::collections::{BTreeMap, BTreeSet};

use ferric_iommu::{
    dma::{
        UnmapFailure, UnmapState, complete_unmap,
        grant::{MapRunsError, MapStep, map_runs, record_installation},
        links::DomainLinks,
    },
    iova::{IovaAllocator, IovaRange},
};
use ferric_regbuf::{
    abi::{Access, GrantSpec},
    table::GrantTable,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    NoMemory,
    Invalidation,
}

#[derive(Default)]
struct Backend {
    entries: BTreeSet<u64>,
    cached: BTreeSet<u64>,
    steps: Vec<MapStep>,
}

impl Backend {
    fn change(&mut self, step: MapStep, fail_at: u64, rollback_fails: bool) -> Result<(), Fault> {
        self.steps.push(step);
        match step {
            MapStep::Map { offset, .. } => {
                if offset == fail_at && rollback_fails {
                    return Err(Fault::NoMemory);
                }
                self.entries.insert(offset);
                self.cached.insert(offset);
                if offset == fail_at {
                    return Err(Fault::Invalidation);
                }
                Ok(())
            }
            MapStep::Rollback { .. } => {
                self.entries.clear();
                if rollback_fails {
                    // Entries are gone, but cached DMA is still possible.
                    return Err(Fault::Invalidation);
                }
                self.cached.clear();
                Ok(())
            }
        }
    }
}

#[derive(Debug)]
struct Held {
    block: IovaRange,
    pins: usize,
    unmap: UnmapState,
}

#[test]
fn uncertain_first_and_later_runs_stay_recorded_through_revoke_and_close() {
    for fail_at in [0, 0x1000, 0x2000] {
        for rollback_fails in [false, true] {
            if rollback_fails && fail_at == 0 {
                continue; // No preceding run needs rollback.
            }
            let mut backend = Backend::default();
            let result = map_runs(
                &[
                    (0x10_0000, 0x1000),
                    (0x20_0000, 0x1000),
                    (0x30_0000, 0x1000),
                ],
                |step| backend.change(step, fail_at, rollback_fails),
                |error| *error == Fault::NoMemory,
            );
            assert!(matches!(result, Err(MapRunsError::Retained { .. })));
            assert!(!backend.cached.is_empty());
            if rollback_fails {
                assert!(backend.entries.is_empty());
                assert!(matches!(
                    result,
                    Err(MapRunsError::Retained {
                        rollback: Some(Fault::Invalidation),
                        ..
                    })
                ));
            }

            let mut iovas = IovaAllocator::new(0x1_0000, []);
            let block = iovas.alloc(0x3000).unwrap();
            let mut mappings = BTreeMap::new();
            let attempt = record_installation(
                &mut mappings,
                block.start,
                Held {
                    block,
                    pins: 3,
                    unmap: UnmapState::default(),
                },
                result,
                |held| held.unmap = UnmapState::failed(),
            )
            .unwrap();
            let mut links = DomainLinks::new();
            let mut grants = GrantTable::new();
            grants.register(0x3000).unwrap();
            let grant = grants
                .create(GrantSpec::new(Access::ReadWrite, 0, 0x3000).unwrap())
                .unwrap();
            let attempt = attempt.map_record(|record| {
                links.insert(1, 7, record);
                record
            });
            assert!(matches!(
                attempt.publish(|record| {
                    grants.add_mapping(grant, record).unwrap();
                    Ok(record)
                }),
                Err(MapRunsError::Retained { .. })
            ));
            assert!(!grants.settled(grant));

            // The failed map's resources cannot be reused, even though MapGrant returned EIO.
            let another = iovas.alloc(0x3000).unwrap();
            assert_ne!(another.start, block.start);
            for taken in grants.revoke(grant).unwrap() {
                let result = complete_unmap(&mut mappings, taken.mapping, |held| {
                    held.unmap
                        .attempt(|| panic!("a partial invalidation must not be retried"))
                });
                assert!(matches!(
                    result,
                    Err(UnmapFailure::<Fault>::PreviouslyFailed)
                ));
                grants.restore(taken);
            }
            assert!(!grants.settled(grant));
            for taken in grants.remove(grant) {
                grants.restore(taken);
            }
            for taken in grants.close() {
                grants.restore(taken);
            }
            assert!(!grants.drained());
            assert_eq!(mappings[&block.start].pins, 3);

            // A failed device block preserves the link and refuses a successful revoke.
            assert!(links.close(7, false).is_empty());
            assert!(links.closed_failed(7));
            assert!(!grants.drained());
            // Confirmed blocking retires both tables and releases resources exactly once.
            backend.cached.clear();
            for record in links.close(7, true) {
                let taken = grants
                    .take_teardown_mapping(grant, |iova| *iova == record)
                    .unwrap();
                grants.finish(taken);
                let held = mappings.remove(&record).unwrap();
                assert_eq!(held.pins, 3);
                iovas.free(held.block).unwrap();
            }
            assert!(grants.drained());
            assert!(mappings.is_empty());
            assert!(links.close(7, true).is_empty());
            assert_eq!(iovas.alloc(0x3000).unwrap(), block);
        }
    }
}

#[test]
fn clean_failures_release_only_after_all_preceding_runs_are_unreachable() {
    for fail_at in [0, 0x1000, 0x2000] {
        let mut steps = Vec::new();
        let mut live = BTreeSet::new();
        let result = map_runs(
            &[
                (0x10_0000, 0x1000),
                (0x20_0000, 0x1000),
                (0x30_0000, 0x1000),
            ],
            |step| {
                steps.push(step);
                match step {
                    MapStep::Map { offset, .. } if offset == fail_at => Err(Fault::NoMemory),
                    MapStep::Map { offset, .. } => {
                        live.insert(offset);
                        Ok(())
                    }
                    MapStep::Rollback { len } => {
                        assert_eq!(len, fail_at);
                        live.clear();
                        Ok(())
                    }
                }
            },
            |_| true,
        );
        assert_eq!(result, Err(MapRunsError::Clean(Fault::NoMemory)));
        assert!(live.is_empty());
        let mut owned = BTreeMap::new();
        assert!(matches!(
            record_installation(&mut owned, 0x1000, (), result, |_| {
                panic!("clean rollback must not retain resources")
            }),
            Err((MapRunsError::Clean(Fault::NoMemory), ()))
        ));
        assert!(owned.is_empty());
        assert_eq!(
            steps
                .iter()
                .filter(|step| matches!(step, MapStep::Rollback { .. }))
                .count(),
            usize::from(fail_at != 0)
        );
    }
}

#[test]
fn success_publishes_once_and_preserves_run_offsets() {
    let mut steps = Vec::new();
    let outcome = map_runs(
        &[(0x10_0000, 0x1000), (0x20_0000, 0x2000)],
        |step| {
            steps.push(step);
            Ok::<(), Fault>(())
        },
        |_| false,
    );
    assert_eq!(outcome, Ok(()));
    assert_eq!(
        steps,
        [
            MapStep::Map {
                offset: 0,
                phys: 0x10_0000,
                len: 0x1000
            },
            MapStep::Map {
                offset: 0x1000,
                phys: 0x20_0000,
                len: 0x2000
            }
        ]
    );
    let mut mappings = BTreeMap::new();
    let attempt = record_installation(&mut mappings, 4, UnmapState::default(), outcome, |_| {
        panic!("successful installation cannot become failed")
    })
    .unwrap();
    let mut records = Vec::new();
    assert_eq!(
        attempt.map_record(|iova| (iova, 77)).publish(|record| {
            records.push(record);
            Ok(record)
        }),
        Ok((4, 77))
    );
    assert_eq!(records, [(4, 77)]);
    let mut completed = 0;
    let gone = complete_unmap(&mut mappings, 4, |state| {
        state.attempt(|| {
            completed += 1;
            Ok::<(), Fault>(())
        })
    })
    .unwrap();
    assert!(gone.is_some());
    assert!(
        complete_unmap(&mut mappings, 4, |_| Ok::<(), Fault>(()))
            .unwrap()
            .is_none()
    );
    assert_eq!(completed, 1);
}

#[test]
fn invalid_lengths_do_not_touch_the_backend() {
    for runs in [&[][..], &[(0, 0)][..], &[(0, u64::MAX), (0, 1)][..]] {
        assert_eq!(
            map_runs(
                runs,
                |_| panic!("invalid length reached the backend"),
                |_| false
            ),
            Err(MapRunsError::<Fault>::InvalidLength)
        );
    }
}
