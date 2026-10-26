//! Lock ordering, sleeping wait queues, and their record representations.

pub use self::{ordered::*, wait_condition::WaitCondition, wait_queue::WaitQueue};

pub mod ordered;
pub(crate) mod records;
pub(crate) mod spin_wait;
pub mod wait_condition;
pub mod wait_queue;
