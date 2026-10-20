//! The order a job reads its blocks in.
//!
//! A job reads `io_size / bs` blocks of the region it was given, each exactly once, which is
//! what fio does by default with `--rw=randread` (its random map, `norandommap` unset) and
//! trivially with `--rw=read`. [`Order::block`] maps the `i`-th read of the job to its block
//! in O(1), so submitter threads can share one atomic counter of reads started and each take
//! the next index, with no table and no lock.
//!
//! The random order is a seeded permutation, not fio's: fio walks its blocks with an LFSR,
//! this crate with a bijective mix over the next power of two and cycle-walking. Both read
//! every block once in an order with no locality, which is what an SSD's random-read figure
//! measures; the exact sequence does not matter to a flash drive, so the numbers compare.

use crate::job::{Job, Rw};

/// The order of a job's blocks: block `i` of the job is [`Order::block`]`(i)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Order {
    blocks: u64,
    random: Option<Mix>,
}

impl Order {
    /// The order of `blocks` blocks: in sequence for [`Rw::Read`], a permutation seeded with
    /// `seed` for [`Rw::RandRead`].
    pub const fn new(rw: Rw, blocks: u64, seed: u64) -> Self {
        let random = match rw {
            Rw::Read => None,
            Rw::RandRead => Some(Mix::new(blocks, seed)),
        };
        Self { blocks, random }
    }

    /// The order of `job`'s blocks.
    pub const fn of(job: &Job) -> Self {
        Self::new(job.rw, job.ios(), job.seed)
    }

    /// Blocks in the order.
    pub const fn len(&self) -> u64 {
        self.blocks
    }

    /// Whether the order has no blocks.
    pub const fn is_empty(&self) -> bool {
        self.blocks == 0
    }

    /// The block read `i`-th, or `None` once every block has been read (`i` at or past
    /// [`Order::len`]).
    pub fn block(&self, i: u64) -> Option<u64> {
        if i >= self.blocks {
            return None;
        }
        Some(match self.random {
            None => i,
            Some(mix) => mix.walk(i, self.blocks),
        })
    }
}

/// A bijection of `0..2^bits` built from steps that are each one: adding a key, multiplying
/// by an odd number and xor-shifting right, all modulo `2^bits`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mix {
    mask: u64,
    shift: u32,
    keys: [u64; ROUNDS],
}

const ROUNDS: usize = 3;

impl Mix {
    const fn new(blocks: u64, seed: u64) -> Self {
        // At least one bit, so a single block still has a domain to walk.
        let bits = match blocks {
            0 | 1 => 1,
            n => 64 - (n - 1).leading_zeros(),
        };
        let mask = if bits == 64 {
            u64::MAX
        } else {
            (1 << bits) - 1
        };
        let mut keys = [0; ROUNDS];
        let mut state = seed;
        let mut round = 0;
        while round < ROUNDS {
            state = splitmix(state);
            keys[round] = state;
            round += 1;
        }
        Self {
            mask,
            shift: bits / 2 + 1,
            keys,
        }
    }

    const fn apply(&self, mut x: u64) -> u64 {
        let mut round = 0;
        while round < ROUNDS {
            let key = self.keys[round];
            x = x.wrapping_add(key) & self.mask;
            // An odd multiplier is invertible modulo a power of two.
            x = x.wrapping_mul(key | 1) & self.mask;
            x ^= x >> self.shift;
            round += 1;
        }
        x
    }

    /// Cycle-walking: apply the bijection until the value is a block. The domain is less than
    /// twice `blocks`, so this takes under two steps on average, and it ends because `i` is on
    /// a cycle of the bijection that holds at least one block (`i` itself).
    fn walk(&self, i: u64, blocks: u64) -> u64 {
        let mut x = self.apply(i);
        while x >= blocks {
            x = self.apply(x);
        }
        x
    }
}

/// One step of SplitMix64 (Steele, Lea and Flood, "Fast splittable pseudorandom number
/// generators", OOPSLA 2014), to spread the seed over the round keys.
const fn splitmix(state: u64) -> u64 {
    let mut z = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{vec, vec::Vec};

    use super::*;

    fn all(order: &Order) -> Vec<u64> {
        (0..order.len()).map(|i| order.block(i).unwrap()).collect()
    }

    fn is_permutation(blocks: &[u64]) -> bool {
        let mut seen = vec![false; blocks.len()];
        blocks.iter().all(|&b| {
            let fresh = !seen[b as usize];
            seen[b as usize] = true;
            fresh
        })
    }

    #[test]
    fn sequential_reads_go_in_order() {
        let order = Order::new(Rw::Read, 5, 1);
        assert_eq!(all(&order), [0, 1, 2, 3, 4]);
        assert_eq!(order.block(5), None);
    }

    #[test]
    fn random_reads_read_every_block_once() {
        for blocks in [1, 2, 3, 7, 64, 65, 1000, 4096, 100_003] {
            let order = Order::new(Rw::RandRead, blocks, 42);
            let got = all(&order);
            assert!(is_permutation(&got), "{blocks} blocks");
            assert_eq!(order.block(blocks), None);
        }
    }

    #[test]
    fn random_reads_are_scattered() {
        let order = Order::new(Rw::RandRead, 4096, 7);
        let got = all(&order);
        assert_ne!(got, all(&Order::new(Rw::Read, 4096, 7)));
        // Neighbours in the order are rarely neighbours on the disk.
        let adjacent = got.windows(2).filter(|w| w[0].abs_diff(w[1]) == 1).count();
        assert!(adjacent < 40, "{adjacent} sequential steps");
    }

    #[test]
    fn the_seed_picks_the_order() {
        let a = all(&Order::new(Rw::RandRead, 1000, 1));
        assert_eq!(a, all(&Order::new(Rw::RandRead, 1000, 1)));
        assert_ne!(a, all(&Order::new(Rw::RandRead, 1000, 2)));
    }

    #[test]
    fn an_empty_order_has_no_blocks() {
        let order = Order::new(Rw::RandRead, 0, 1);
        assert!(order.is_empty());
        assert_eq!(order.block(0), None);
    }

    #[test]
    fn a_job_orders_its_ios() {
        let job = Job::parse(["--rw=randread", "--bs=4k", "--io_size=1m"]).unwrap();
        let order = Order::of(&job);
        assert_eq!(order.len(), 256);
        assert!(is_permutation(&all(&order)));
    }
}
