//! Stacks whose shape is chosen at run time.
//!
//! The layer types nest at compile time (`Crypt<Slice<File>>`), which suits code that knows
//! the disk layout. A daemon learns the layout from its configuration instead, so [`Stack`]
//! is one type for any stack over one kind of base device, and [`StackError`] one error type
//! for its reads, keeping each layer's error in the source chain.

use alloc::boxed::Box;
use core::error::Error;
use core::fmt;

use crate::{BlockDevice, OutOfRange, Slice, SliceError};
#[cfg(feature = "luks")]
use crate::{Crypt, CryptError, CryptOpenError, Limits};
#[cfg(feature = "lvm")]
use crate::{Lvm, LvmError, LvmOpenError};

/// A stack of layers over base devices of type `D`.
#[derive(Debug)]
#[non_exhaustive]
pub enum Stack<D> {
    /// A base device.
    Device(D),
    /// A byte window of a stack.
    Slice(Box<Slice<Stack<D>>>),
    /// A LUKS volume on a stack.
    #[cfg(feature = "luks")]
    Crypt(Box<Crypt<Stack<D>>>),
    /// A logical volume on stacks.
    #[cfg(feature = "lvm")]
    Lvm(Box<Lvm<Stack<D>>>),
}

impl<D: BlockDevice> Stack<D> {
    /// The bytes `offset..offset + len` of `self`; see [`Slice::new`].
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] when the window does not fit.
    pub fn slice(self, offset: u64, len: u64) -> Result<Self, OutOfRange> {
        Ok(Self::Slice(Box::new(Slice::new(self, offset, len)?)))
    }

    /// The LUKS volume on `self`; see [`Crypt::open`].
    ///
    /// # Errors
    ///
    /// The [`CryptOpenError`] of [`Crypt::open`].
    #[cfg(feature = "luks")]
    pub fn crypt(
        self,
        passphrase: &[u8],
        limits: &Limits,
    ) -> Result<Self, CryptOpenError<StackError<D::Error>>> {
        Ok(Self::Crypt(Box::new(Crypt::open(
            self, passphrase, limits,
        )?)))
    }

    /// Logical volume `name` on the physical volumes `pvs`; see [`Lvm::open`].
    ///
    /// # Errors
    ///
    /// The [`LvmOpenError`] of [`Lvm::open`].
    #[cfg(feature = "lvm")]
    pub fn lvm(
        pvs: alloc::vec::Vec<Self>,
        name: &str,
    ) -> Result<Self, LvmOpenError<StackError<D::Error>>> {
        Ok(Self::Lvm(Box::new(Lvm::open(pvs, name)?)))
    }
}

impl<D: BlockDevice> BlockDevice for Stack<D> {
    type Error = StackError<D::Error>;

    fn size(&self) -> u64 {
        match self {
            Self::Device(d) => d.size(),
            Self::Slice(s) => s.size(),
            #[cfg(feature = "luks")]
            Self::Crypt(c) => c.size(),
            #[cfg(feature = "lvm")]
            Self::Lvm(l) => l.size(),
        }
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        match self {
            Self::Device(d) => d.read_at(offset, buf).map_err(StackError::Device),
            Self::Slice(s) => s
                .read_at(offset, buf)
                .map_err(|e| StackError::Slice(Box::new(e))),
            #[cfg(feature = "luks")]
            Self::Crypt(c) => c
                .read_at(offset, buf)
                .map_err(|e| StackError::Crypt(Box::new(e))),
            #[cfg(feature = "lvm")]
            Self::Lvm(l) => l
                .read_at(offset, buf)
                .map_err(|e| StackError::Lvm(Box::new(e))),
        }
    }
}

/// A read from a [`Stack`] that failed: the error of the top layer, whose source chain leads
/// down to the layer that failed first.
#[derive(Debug)]
#[non_exhaustive]
pub enum StackError<E> {
    /// A base device failed.
    Device(E),
    /// A [`Slice`] failed.
    Slice(Box<SliceError<StackError<E>>>),
    /// A [`Crypt`] volume failed.
    #[cfg(feature = "luks")]
    Crypt(Box<CryptError<StackError<E>>>),
    /// An [`Lvm`] volume failed.
    #[cfg(feature = "lvm")]
    Lvm(Box<LvmError<StackError<E>>>),
}

impl<E> StackError<E> {
    /// Whether the read failed because it was not inside some layer, rather than because a
    /// device failed or data could not be mapped.
    pub fn is_out_of_range(&self) -> bool {
        match self {
            Self::Device(_) => false,
            Self::Slice(e) => match &**e {
                SliceError::OutOfRange(_) => true,
                SliceError::Inner(e) => e.is_out_of_range(),
            },
            #[cfg(feature = "luks")]
            Self::Crypt(e) => match &**e {
                CryptError::OutOfRange(_) => true,
                CryptError::Inner(e) => e.is_out_of_range(),
                CryptError::Decrypt(_) => false,
            },
            #[cfg(feature = "lvm")]
            Self::Lvm(e) => match &**e {
                LvmError::OutOfRange(_) => true,
                LvmError::Inner { source, .. } => source.is_out_of_range(),
                LvmError::Map(_) => false,
            },
        }
    }
}

// Written out rather than derived: each variant shows the layer error it holds, so the chain
// reads as the layers do, top first.
impl<E: Error + 'static> fmt::Display for StackError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(e) => fmt::Display::fmt(e, f),
            Self::Slice(e) => fmt::Display::fmt(e, f),
            #[cfg(feature = "luks")]
            Self::Crypt(e) => fmt::Display::fmt(e, f),
            #[cfg(feature = "lvm")]
            Self::Lvm(e) => fmt::Display::fmt(e, f),
        }
    }
}

impl<E: Error + 'static> Error for StackError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        // Transparent, like the Display above: the source is the held layer error's.
        match self {
            Self::Device(e) => e.source(),
            Self::Slice(e) => e.source(),
            #[cfg(feature = "luks")]
            Self::Crypt(e) => e.source(),
            #[cfg(feature = "lvm")]
            Self::Lvm(e) => e.source(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Memory;

    #[test]
    fn slices_of_slices_compose_and_report_range_errors() {
        let bytes: [u8; 64] = core::array::from_fn(|i| i as u8);
        let stack = Stack::Device(Memory::new(bytes))
            .slice(8, 32)
            .unwrap()
            .slice(4, 8)
            .unwrap();
        assert_eq!(stack.size(), 8);
        let mut buf = [0; 3];
        stack.read_at(5, &mut buf).unwrap();
        assert_eq!(buf, [17, 18, 19]);
        let error = stack.read_at(6, &mut buf).unwrap_err();
        assert!(error.is_out_of_range());
        assert_eq!(
            alloc::format!("{error}"),
            "3 bytes at 0x6 are not inside the 8-byte device"
        );
    }
}
