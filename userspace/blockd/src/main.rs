//! `ferric-blockd`: serves a LUKS volume, an LVM logical volume or a stack of them,
//! read-only, as a scheme.
//!
//! ```text
//! ferric-blockd SCHEME STEP...
//! ```
//!
//! See `ferric_blockd::spec` for the steps. The volume appears at `/scheme/SCHEME` once it is
//! built, and the daemon serves it in the foreground until the scheme is closed. Only Redox
//! has schemes; elsewhere the program says so and exits.

#![forbid(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "redox")]
    {
        daemon::main()
    }
    #[cfg(not(target_os = "redox"))]
    {
        eprintln!("ferric-blockd: schemes exist only on Redox; this build only runs its tests");
        ExitCode::FAILURE
    }
}

#[cfg(target_os = "redox")]
mod daemon {
    use std::error::Error as _;
    use std::io;
    use std::process::ExitCode;

    use ferric_blockd::Files;
    use ferric_blockd::assemble::{AssembleError, assemble};
    use ferric_blockd::scheme::{Mount, ServeError};
    use ferric_blockd::spec::{Spec, SpecError};
    use ferric_blockd::volume::Volume;
    use thiserror::Error;

    #[derive(Debug, Error)]
    enum DaemonError {
        #[error(transparent)]
        Spec(#[from] SpecError),
        #[error("cannot build the volume")]
        Assemble(#[from] AssembleError<io::Error>),
        #[error("serving the scheme failed")]
        Serve(#[from] ServeError),
    }

    fn run() -> Result<(), DaemonError> {
        let spec = Spec::parse(std::env::args().skip(1))?;
        let volume = assemble(&spec.steps, &mut Files)?;
        let size = ferric_blocklayer::BlockDevice::size(&volume);
        let mount = Mount::new(Volume::new(volume), &spec.scheme)?;
        eprintln!(
            "ferric-blockd: {size}-byte volume served read-only at /scheme/{}",
            spec.scheme
        );
        mount.serve()?;
        Ok(())
    }

    pub fn main() -> ExitCode {
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprint!("ferric-blockd: {err}");
                let mut source = err.source();
                while let Some(cause) = source {
                    eprint!(": {cause}");
                    source = cause.source();
                }
                eprintln!();
                ExitCode::FAILURE
            }
        }
    }
}
