//! Host-testable validation of the actual CQEs returned by the target ring tests.

use ferric_ring::Cqe;

/// Which completion invariant failed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mismatch {
    /// A missing or duplicate CQE.
    Count { expected: usize, got: usize },
    /// A CQE belongs to the wrong request.
    Identity { expected: u64, got: u64 },
    /// The operation failed or transferred the wrong amount.
    Result { expected: i64, got: i64 },
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count { expected, got } => write!(f, "expected {expected} CQEs, got {got}"),
            Self::Identity { expected, got } => write!(f, "expected request {expected}, got {got}"),
            Self::Result { expected, got } => write!(f, "expected result {expected}, got {got}"),
        }
    }
}
impl std::error::Error for Mismatch {}

/// Validates count, order, request identity and results of a CQE batch.
///
/// # Errors
/// Returns the first count, identity or result mismatch.
pub(crate) fn check(cqes: &[Cqe], expected: &[(u64, i64)]) -> Result<(), Mismatch> {
    if cqes.len() != expected.len() {
        return Err(Mismatch::Count {
            expected: expected.len(),
            got: cqes.len(),
        });
    }
    for (cqe, &(id, result)) in cqes.iter().zip(expected) {
        if cqe.user_data != id {
            return Err(Mismatch::Identity {
                expected: id,
                got: cqe.user_data,
            });
        }
        if cqe.result != result {
            return Err(Mismatch::Result {
                expected: result,
                got: cqe.result,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_read_must_have_exactly_one_matching_cqe() {
        let cqe = Cqe {
            user_data: 7,
            result: 1,
            ..Default::default()
        };
        assert_eq!(check(&[cqe], &[(7, 1)]), Ok(()));
        assert!(matches!(check(&[], &[(7, 1)]), Err(Mismatch::Count { .. })));
        assert!(matches!(
            check(&[cqe, cqe], &[(7, 1)]),
            Err(Mismatch::Count { .. })
        ));
        assert!(matches!(
            check(&[cqe], &[(8, 1)]),
            Err(Mismatch::Identity { .. })
        ));
        for result in [-5, 0, 2] {
            assert!(matches!(
                check(&[Cqe { result, ..cqe }], &[(7, 1)]),
                Err(Mismatch::Result { .. })
            ));
        }
        assert!(matches!(check(&[cqe], &[]), Err(Mismatch::Count { .. })));
    }
}
