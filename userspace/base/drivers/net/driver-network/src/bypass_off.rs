//! Ferric Oxide: the stand-in for `bypass.rs` without the `bypass` feature. The scheme has no
//! `queue` path, steering does nothing, and netstack reads straight from the adapter, as
//! upstream.

use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use redox_scheme::Socket;
use scheme_utils::ResourceSync;

use crate::{NetworkAdapter, NetworkResource, NetworkSchemeData};

/// The scheme's bypass state: none.
pub(crate) struct State;

impl State {
    pub(crate) fn new(_socket: Rc<Socket>) -> std::io::Result<Self> {
        Ok(Self)
    }

    pub(crate) fn wake_handle(&self) -> Option<usize> {
        None
    }

    pub(crate) fn note_answered(&mut self) {}

    pub(crate) fn drop_unclaimed(&mut self) {}

    pub(crate) fn pending(&self) -> usize {
        0
    }

    pub(crate) fn steering(&self) -> bool {
        false
    }

    pub(crate) fn take_pending(&mut self, _buf: &mut [u8]) -> Option<usize> {
        None
    }
}

/// Whether `path` opens a queue socket: never.
pub(crate) fn is_queue_path(_path: &str) -> bool {
    false
}

/// Steering: nothing to steer to.
pub(crate) fn steer<'a, T: NetworkAdapter + 'a>(
    _handles: impl Iterator<Item = (&'a usize, &'a mut NetworkResource<T>)>,
    _data: &mut NetworkSchemeData<T>,
) -> Vec<usize> {
    Vec::new()
}

/// A queue socket, which cannot be opened without the feature.
pub(crate) struct Queue<T>(core::convert::Infallible, PhantomData<T>);

impl<T> Queue<T> {
    /// `None`: there are no queue sockets.
    pub(crate) fn new() -> Option<Self> {
        None
    }
}

impl<T> fmt::Debug for Queue<T> {
    fn fmt(&self, _f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {}
    }
}

impl<T: NetworkAdapter> ResourceSync for Queue<T> {
    type SchemeData = NetworkSchemeData<T>;
    type ResourceEnum = NetworkResource<T>;
}
