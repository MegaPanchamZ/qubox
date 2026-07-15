//! Lock-free single-producer single-consumer (SPSC) ring buffer
//! for F32 PCM samples.
//!
//! The cpal capture callback is the sole producer; the mic
//! pipeline thread is the sole consumer. The buffer is fixed-size
//! and overwrites the oldest sample on overflow (the cpal callback
//! must never block, and dropping a few reference samples is far
//! better than glitching the audio thread).
//!
//! The structure is a `Vec<f32>` (wrapped in `UnsafeCell` so
//! concurrent producer/consumer can both take `&self`) plus
//! monotonically-increasing atomic read and write cursors. We use
//! `Release`/`Acquire` ordering so the producer's writes become
//! visible to the consumer; no locks are involved.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

/// SPSC ring buffer of `f32` samples.
///
/// Capacity is always a power of two so the modulo wraps without a
/// `%` instruction. Methods take `&self` so the ring can be shared
/// via `Arc<SpscRing>` between a real-time cpal callback and a
/// pipeline thread; soundness relies on the SPSC discipline (only
/// one producer thread, only one consumer thread).
pub struct SpscRing {
    buf: UnsafeCell<Vec<f32>>,
    mask: usize,
    /// Total samples ever written (monotonic, wraps at `usize::MAX`).
    write_total: AtomicUsize,
    /// Total samples ever read (monotonic, wraps at `usize::MAX`).
    read_total: AtomicUsize,
}

unsafe impl Send for SpscRing {}
unsafe impl Sync for SpscRing {}

impl SpscRing {
    /// Create a new ring with `capacity` samples. `capacity` is
    /// rounded up to the next power of two.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(2).next_power_of_two();
        Self {
            buf: UnsafeCell::new(vec![0.0_f32; cap]),
            mask: cap - 1,
            write_total: AtomicUsize::new(0),
            read_total: AtomicUsize::new(0),
        }
    }

    /// Capacity in samples.
    pub fn capacity(&self) -> usize {
        unsafe { (*self.buf.get()).len() }
    }

    /// Number of samples currently buffered (producer-pushed but
    /// not yet consumer-popped).
    pub fn len(&self) -> usize {
        let w = self.write_total.load(Ordering::Acquire);
        let r = self.read_total.load(Ordering::Acquire);
        w.saturating_sub(r)
    }

    /// True when the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Push a single sample. Overwrites the oldest sample if the
    /// ring is full (and advances the read cursor to maintain
    /// `len() == capacity`).
    pub fn push(&self, sample: f32) {
        let w = self.write_total.load(Ordering::Relaxed);
        let idx = w & self.mask;
        let buf = unsafe { &mut (*self.buf.get()) };
        buf[idx] = sample;
        let new_w = w.wrapping_add(1);
        self.write_total.store(new_w, Ordering::Release);

        if new_w - self.read_total.load(Ordering::Relaxed) > buf.len() {
            let new_r = new_w - buf.len();
            self.read_total.store(new_r, Ordering::Release);
        }
    }

    /// Push a slice. Drops oldest samples on overflow.
    pub fn push_slice(&self, samples: &[f32]) {
        for s in samples {
            self.push(*s);
        }
    }

    /// Pop up to `out.len()` samples into `out`. Returns the number
    /// of samples actually written.
    pub fn pop_into(&self, out: &mut [f32]) -> usize {
        let r = self.read_total.load(Ordering::Relaxed);
        let w = self.write_total.load(Ordering::Acquire);
        let available = w.saturating_sub(r);
        let n = available.min(out.len());
        let buf = unsafe { &(*self.buf.get()) };
        for i in 0..n {
            let idx = (r.wrapping_add(i)) & self.mask;
            out[i] = buf[idx];
        }
        self.read_total.store(r.wrapping_add(n), Ordering::Release);
        n
    }

    /// Discard all buffered samples. Returns the number discarded.
    pub fn drain(&self) -> usize {
        let r = self.read_total.load(Ordering::Relaxed);
        let w = self.write_total.load(Ordering::Acquire);
        let discarded = w.saturating_sub(r);
        self.read_total.store(w, Ordering::Release);
        discarded
    }
}

impl std::fmt::Debug for SpscRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpscRing")
            .field("capacity", &self.capacity())
            .field("len", &self.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_pop_returns_in_order() {
        let ring = SpscRing::new(8);
        ring.push_slice(&[1.0, 2.0, 3.0, 4.0]);
        let mut out = [0.0_f32; 4];
        let n = ring.pop_into(&mut out);
        assert_eq!(n, 4);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        assert!(ring.is_empty());
    }

    #[test]
    fn overflow_drops_oldest_samples() {
        let ring = SpscRing::new(4);
        ring.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let mut out = [0.0_f32; 4];
        let n = ring.pop_into(&mut out);
        assert_eq!(n, 4);
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn pop_into_empty_returns_zero() {
        let ring = SpscRing::new(4);
        let mut out = [0.0_f32; 4];
        assert_eq!(ring.pop_into(&mut out), 0);
    }

    #[test]
    fn drain_clears_buffered_samples() {
        let ring = SpscRing::new(8);
        ring.push_slice(&[1.0, 2.0, 3.0]);
        let n = ring.drain();
        assert_eq!(n, 3);
        assert!(ring.is_empty());
    }

    #[test]
    fn spsc_safety_under_concurrent_producer_and_consumer() {
        use std::sync::Arc;
        use std::thread;

        let ring = Arc::new(SpscRing::new(1024));
        let producer_ring = Arc::clone(&ring);
        let producer = thread::spawn(move || {
            for i in 0..10_000_u32 {
                producer_ring.push(i as f32);
            }
        });

        let mut total = 0_usize;
        let mut out = [0.0_f32; 64];
        while ring.len() > 0 || !producer.is_finished() {
            total += ring.pop_into(&mut out);
        }
        let _ = producer.join();
        let final_pop = ring.pop_into(&mut out);
        total += final_pop;
        assert!(total > 0);
    }
}
