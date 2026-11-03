//! Reporting the error that stops a driver at its outermost boundary.
//!
//! Library code returns typed errors; a driver's `main` or daemon entry turns the one that
//! ends it into log lines here, the error first and then each of its causes, so the source
//! chain is never cut to the outermost message.

use std::error::Error;
use std::iter;

/// Iterate over `error` and then each error in its source chain, outermost first.
///
/// # Examples
///
/// ```
/// use std::io;
///
/// let error = io::Error::other("disk on fire");
/// let messages: Vec<String> = common::error_chain(&error).map(|e| e.to_string()).collect();
/// assert_eq!(messages, ["disk on fire"]);
/// ```
pub fn error_chain<'a>(
    error: &'a (dyn Error + 'static),
) -> impl Iterator<Item = &'a (dyn Error + 'static)> {
    iter::successors(Some(error), |&error| error.source())
}

/// Log `error` and its full source chain as errors prefixed with `driver`, then exit the
/// process with status 1.
///
/// The driver must have set up logging (see [`crate::setup_logging`]) for the lines to be
/// visible.
pub fn exit_with_error(driver: &str, error: &(dyn Error + 'static)) -> ! {
    let mut chain = error_chain(error);
    if let Some(error) = chain.next() {
        log::error!("{driver}: {error}");
    }
    for cause in chain {
        log::error!("{driver}: caused by: {cause}");
    }
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    #[derive(Debug)]
    struct Layer {
        message: &'static str,
        source: Option<Box<Layer>>,
    }

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl Error for Layer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.source.as_deref().map(|layer| layer as _)
        }
    }

    #[test]
    fn chain_lists_every_cause_outermost_first() {
        let error = Layer {
            message: "cannot start",
            source: Some(Box::new(Layer {
                message: "cannot register",
                source: Some(Box::new(Layer {
                    message: "permission denied",
                    source: None,
                })),
            })),
        };
        let messages: Vec<String> = error_chain(&error).map(ToString::to_string).collect();
        assert_eq!(
            messages,
            ["cannot start", "cannot register", "permission denied"]
        );
    }

    #[test]
    fn chain_of_a_leaf_error_is_the_error_alone() {
        let error = Layer {
            message: "leaf",
            source: None,
        };
        assert_eq!(error_chain(&error).count(), 1);
    }
}
