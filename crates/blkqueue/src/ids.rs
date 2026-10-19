//! Job ids read back from a byte stream (a pipe), whole or split across reads.
//!
//! Workers report each finished job by writing its 8-byte id to a pipe the waiting thread
//! reads. A read may return part of an id, so [`IdStream`] keeps the remainder for the next
//! read instead of dropping it. [`WORKER_GONE`] is the id a worker writes as it exits, which
//! no job gets.

/// The id a worker reports as its thread ends; [`next_id`] never hands it to a job.
pub const WORKER_GONE: u64 = u64::MAX;

/// The id after `id` for the next job, skipping [`WORKER_GONE`].
pub const fn next_id(id: u64) -> u64 {
    let next = id.wrapping_add(1);
    if next == WORKER_GONE { 0 } else { next }
}

/// Reassembles native-endian `u64` ids from a stream of reads.
#[derive(Debug, Default)]
pub struct IdStream {
    partial: [u8; 8],
    len: usize,
}

impl IdStream {
    /// An empty stream.
    pub const fn new() -> Self {
        Self {
            partial: [0; 8],
            len: 0,
        }
    }

    /// Feeds the bytes of one read and calls `each` with every id they complete, in order;
    /// a trailing part of an id is kept for the next call.
    pub fn feed(&mut self, mut bytes: &[u8], mut each: impl FnMut(u64)) {
        if self.len != 0 {
            let take = (8 - self.len).min(bytes.len());
            self.partial[self.len..self.len + take].copy_from_slice(&bytes[..take]);
            self.len += take;
            bytes = &bytes[take..];
            if self.len < 8 {
                return;
            }
            each(u64::from_ne_bytes(self.partial));
            self.len = 0;
        }
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let mut id = [0; 8];
            id.copy_from_slice(chunk);
            each(u64::from_ne_bytes(id));
        }
        let rest = chunks.remainder();
        self.partial[..rest.len()].copy_from_slice(rest);
        self.len = rest.len();
    }

    /// How many bytes of an unfinished id are held.
    pub const fn pending(&self) -> usize {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(ids: &[u64]) -> [u8; 32] {
        let mut out = [0; 32];
        for (chunk, id) in out.chunks_exact_mut(8).zip(ids) {
            chunk.copy_from_slice(&id.to_ne_bytes());
        }
        out
    }

    fn collect(stream: &mut IdStream, read: &[u8], out: &mut [u64; 4], n: &mut usize) {
        stream.feed(read, |id| {
            out[*n] = id;
            *n += 1;
        });
    }

    #[test]
    fn whole_ids_come_out_in_order() {
        let data = bytes(&[1, 2, 3, 4]);
        let (mut stream, mut out, mut n) = (IdStream::new(), [0; 4], 0);
        collect(&mut stream, &data, &mut out, &mut n);
        assert_eq!((n, out, stream.pending()), (4, [1, 2, 3, 4], 0));
    }

    #[test]
    fn ids_split_at_every_point_are_kept() {
        let data = bytes(&[10, 0x0102_0304_0506_0708, WORKER_GONE, 7]);
        for first in 0..=data.len() {
            for second in first..=data.len() {
                let (mut stream, mut out, mut n) = (IdStream::new(), [0; 4], 0);
                for read in [&data[..first], &data[first..second], &data[second..]] {
                    collect(&mut stream, read, &mut out, &mut n);
                }
                assert_eq!(n, 4, "split at {first} and {second}");
                assert_eq!(out, [10, 0x0102_0304_0506_0708, WORKER_GONE, 7]);
                assert_eq!(stream.pending(), 0);
            }
        }
    }

    #[test]
    fn a_trailing_part_waits_for_the_rest() {
        let data = bytes(&[5]);
        let (mut stream, mut out, mut n) = (IdStream::new(), [0; 4], 0);
        collect(&mut stream, &data[..3], &mut out, &mut n);
        assert_eq!((n, stream.pending()), (0, 3));
        collect(&mut stream, &data[3..8], &mut out, &mut n);
        assert_eq!((n, out[0], stream.pending()), (1, 5, 0));
    }

    #[test]
    fn job_ids_skip_the_exit_marker() {
        assert_eq!(next_id(0), 1);
        assert_eq!(next_id(WORKER_GONE - 1), 0);
        assert_eq!(next_id(WORKER_GONE), 0);
    }
}
