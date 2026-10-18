//! Grant-map transactions, including failures after page-table entries become reachable.
//!
//! The backend reports whether a failed run was strictly rolled back. Uncertain mappings
//! must retain their domain resources and be published to their grant before returning an
//! error. These rules are independent of VT-d or AMD-Vi register encodings.

use alloc::collections::BTreeMap;
use core::fmt;

/// Work for the hardware backend while installing a grant's physical runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapStep {
    /// Install a physical run at an offset within the reserved IOVA block.
    Map {
        /// Offset from the block's first IOVA.
        offset: u64,
        /// First physical address.
        phys: u64,
        /// Run length in bytes.
        len: u64,
    },
    /// Strictly undo all preceding successful runs, from the block's first IOVA.
    Rollback {
        /// Length of the preceding runs in bytes.
        len: u64,
    },
}

/// Why installation of a grant's runs failed.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MapRunsError<E> {
    /// The combined length is zero or exceeds `u64`; no backend operation ran.
    InvalidLength,
    /// All installed entries were strictly rolled back; resources may be released.
    Clean(E),
    /// Reachability is uncertain; resources and a grant teardown record must be retained.
    Retained {
        /// The original installation error.
        map: E,
        /// A failure undoing preceding runs, if rollback was attempted.
        rollback: Option<E>,
    },
}

impl<E: fmt::Display> fmt::Display for MapRunsError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("invalid combined grant-run length"),
            Self::Clean(error) => write!(f, "grant mapping rolled back: {error}"),
            Self::Retained {
                map,
                rollback: None,
            } => {
                write!(
                    f,
                    "grant mapping retained after uncertain invalidation: {map}"
                )
            }
            Self::Retained {
                map,
                rollback: Some(rollback),
            } => {
                write!(
                    f,
                    "grant mapping failed: {map}; rollback failed: {rollback}"
                )
            }
        }
    }
}

impl<E: core::error::Error + 'static> core::error::Error for MapRunsError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::InvalidLength => None,
            Self::Clean(error) => Some(error),
            Self::Retained { map, rollback } => Some(rollback.as_ref().unwrap_or(map)),
        }
    }
}

/// Installs physical `(address, length)` runs in order within a reserved IOVA block.
///
/// `change` must strictly complete each operation before returning success. `clean_failure`
/// may return true only when the failed run itself is known to be unreachable, including
/// completed invalidation. In that case preceding runs are rolled back too. Otherwise no
/// retry of partially cleared entries can prove safety: retain the entire reservation.
/// The caller validates alignment, physical addresses and the reserved block's capacity.
///
/// # Errors
///
/// [`MapRunsError::InvalidLength`] before any operation for a zero or overflowing total;
/// [`MapRunsError::Clean`] only after complete rollback; [`MapRunsError::Retained`] when
/// installation or rollback cannot establish that every installed translation is gone.
///
/// # Panics
///
/// Propagates a panic from `change` or `clean_failure`. Backends must not panic, because
/// unwinding cannot establish whether installed entries remain reachable.
///
/// # Examples
///
/// ```
/// use ferric_iommu::dma::grant::{map_runs, MapRunsError};
/// let result = map_runs(&[(0x1000, 0x1000)], |_| Err("IOTLB timeout"), |_| false);
/// assert_eq!(result, Err(MapRunsError::Retained {
///     map: "IOTLB timeout", rollback: None,
/// }));
/// ```
pub fn map_runs<E>(
    runs: &[(u64, u64)],
    mut change: impl FnMut(MapStep) -> Result<(), E>,
    clean_failure: impl Fn(&E) -> bool,
) -> Result<(), MapRunsError<E>> {
    let total = runs
        .iter()
        .try_fold(0_u64, |n, (_, len)| n.checked_add(*len));
    if total.is_none_or(|n| n == 0) {
        return Err(MapRunsError::InvalidLength);
    }
    let mut done = 0_u64;
    for &(phys, len) in runs {
        if let Err(map) = change(MapStep::Map {
            offset: done,
            phys,
            len,
        }) {
            if !clean_failure(&map) {
                return Err(MapRunsError::Retained {
                    map,
                    rollback: None,
                });
            }
            if done != 0
                && let Err(rollback) = change(MapStep::Rollback { len: done })
            {
                return Err(MapRunsError::Retained {
                    map,
                    rollback: Some(rollback),
                });
            }
            return Err(MapRunsError::Clean(map));
        }
        // The total was checked before any hardware operation.
        done = done.saturating_add(len);
    }
    Ok(())
}

/// Records installed or uncertain resources in the domain before producing a grant attempt.
///
/// Call for a freshly reserved `iova`, holding the domain lock. `mapping` owns its pins and
/// reservation. `mark_failed` must make uncertain teardown terminal until confirmed device
/// blocking. Feed the returned attempt to grant-link and region publication before returning
/// its result to user space. This is the same path for first-run and later-run failures.
///
/// # Errors
///
/// A fully rolled-back installation returns its error and owned resources without inserting
/// a record. Only then may the caller release them. Uncertain failures remain recorded and
/// return an attempt whose error may be reported only after grant publication.
///
/// # Panics
///
/// Propagates a panic from `mark_failed` before the uncertain mapping is recorded. Use a
/// non-panicking state update so ownership of reachable resources is retained.
pub fn record_installation<T, E>(
    mappings: &mut BTreeMap<u64, T>,
    iova: u64,
    mut mapping: T,
    outcome: Result<(), MapRunsError<E>>,
    mark_failed: impl FnOnce(&mut T),
) -> Result<MapAttempt<u64, MapRunsError<E>>, (MapRunsError<E>, T)> {
    let error = match outcome {
        Ok(()) => None,
        Err(error @ (MapRunsError::Clean(_) | MapRunsError::InvalidLength)) => {
            return Err((error, mapping));
        }
        Err(error) => {
            mark_failed(&mut mapping);
            Some(error)
        }
    };
    mappings.insert(iova, mapping);
    Ok(match error {
        Some(error) => MapAttempt::Retained {
            record: iova,
            error,
        },
        None => MapAttempt::Mapped(iova),
    })
}

/// An installed mapping that must be published before reporting its result to the caller.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "publish the mapping record before reporting its result"]
pub enum MapAttempt<R, E> {
    /// Installation and invalidation completed.
    Mapped(R),
    /// Installation failed, but the mapping can still be reachable by a device.
    Retained {
        /// The mapping's teardown record.
        record: R,
        /// The installation or rollback error.
        error: E,
    },
}

impl<R, E> MapAttempt<R, E> {
    /// Converts the record of either outcome, including an uncertain mapping's record.
    ///
    /// # Panics
    ///
    /// Propagates a panic from `map`; use a non-panicking conversion to retain the record.
    pub fn map_record<S>(self, map: impl FnOnce(R) -> S) -> MapAttempt<S, E> {
        match self {
            Self::Mapped(record) => MapAttempt::Mapped(map(record)),
            Self::Retained { record, error } => MapAttempt::Retained {
                record: map(record),
                error,
            },
        }
    }

    /// Converts an uncertain mapping's error at an API boundary, preserving its record.
    ///
    /// # Panics
    ///
    /// Propagates a panic from `map` for a retained attempt. Use a non-panicking conversion
    /// so the mapping can still be published to its grant.
    pub fn map_error<F>(self, map: impl FnOnce(E) -> F) -> MapAttempt<R, F> {
        match self {
            Self::Mapped(record) => MapAttempt::Mapped(record),
            Self::Retained { record, error } => MapAttempt::Retained {
                record,
                error: map(error),
            },
        }
    }

    /// Publishes the teardown record before returning success or an installation error.
    ///
    /// The caller must hold the grant's lock throughout validation, installation and this
    /// publication. `publish` must retain the record, or safely tear it down on error.
    ///
    /// # Errors
    ///
    /// An uncertain installation's error after its record is published, or a publication
    /// error. Successful publication does not make an uncertain mapping safe to release.
    ///
    /// # Panics
    ///
    /// Propagates a panic from `publish`. Use a non-panicking callback so grant bookkeeping
    /// retains every reachable mapping.
    ///
    /// # Examples
    ///
    /// ```
    /// use ferric_iommu::dma::grant::MapAttempt;
    /// let attempt = MapAttempt::Retained { record: 42, error: "timeout" };
    /// let mut records = Vec::new();
    /// let result = attempt.publish(|record| { records.push(record); Ok(record) });
    /// assert_eq!(result, Err("timeout"));
    /// assert_eq!(records, [42]);
    /// ```
    pub fn publish(self, publish: impl FnOnce(R) -> Result<R, E>) -> Result<R, E> {
        match self {
            Self::Mapped(record) => publish(record),
            Self::Retained { record, error } => {
                publish(record)?;
                Err(error)
            }
        }
    }
}
