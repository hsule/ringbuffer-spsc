//! A fast thread-safe `no_std` single-producer single-consumer ring buffer.
//! For performance reasons, the capacity of the buffer is determined
//! at compile time via a const generic and it is required to be a
//! power of two for a more efficient index handling.
//!
//! # Example
//! ```
//! use ringbuffer_spsc::RingBuffer;
//!
//! const N: usize = 1_000_000;
//! let (mut tx, mut rx) = RingBuffer::<usize, 16>::init();
//!
//! let p = std::thread::spawn(move || {
//!     let mut current: usize = 0;
//!     while current < N {
//!         if tx.push(current).is_none() {
//!             current = current.wrapping_add(1);
//!         } else {
//!             std::thread::yield_now();
//!         }
//!     }
//! });
//!
//! let c = std::thread::spawn(move || {
//!     let mut current: usize = 0;
//!     while current < N {
//!         if let Some(c) = rx.pull() {
//!             assert_eq!(c, current);
//!             current = current.wrapping_add(1);
//!         } else {
//!             std::thread::yield_now();
//!         }
//!     }
//! });
//!
//! p.join().unwrap();
//! c.join().unwrap();
//! ```
// #![no_std]
extern crate alloc;

use alloc::sync::Arc;
use cache_padded::CachePadded;
use core::{
    cell::UnsafeCell,
    mem::{self, MaybeUninit},
    sync::atomic::{AtomicUsize, Ordering},
};

pub struct RingBuffer<T, const N: usize> {
    buffer: UnsafeCell<[MaybeUninit<T>; N]>,
    idx_r: CachePadded<AtomicUsize>,
    idx_w: CachePadded<AtomicUsize>,
}

unsafe impl<T, const N: usize> Send for RingBuffer<T, N> {}
unsafe impl<T, const N: usize> Sync for RingBuffer<T, N> {}

impl<T, const N: usize> RingBuffer<T, N> {
    #[allow(clippy::new_ret_no_self)]
    #[deprecated(since = "0.1.8", note = "please use `init()` instead.")]
    pub fn new() -> (RingBufferWriter<T, N>, RingBufferReader<T, N>) {
        Self::init()
    }

    pub fn init() -> (RingBufferWriter<T, N>, RingBufferReader<T, N>) {
        assert!(
            N.is_power_of_two(),
            "RingBuffer requires the capacity to be a power of 2. {N} is not."
        );
        let rb = Arc::new(RingBuffer {
            buffer: UnsafeCell::new(array_init::array_init(|_| MaybeUninit::uninit())),
            idx_r: CachePadded::new(AtomicUsize::new(0)),
            idx_w: CachePadded::new(AtomicUsize::new(0)),
        });
        (
            RingBufferWriter {
                inner: rb.clone(),
                cached_idx_r: 0,
                local_idx_w: 0,
            },
            RingBufferReader {
                inner: rb,
                local_idx_r: 0,
                cached_idx_w: 0,
            },
        )
    }

    #[allow(clippy::mut_from_ref)]
    #[inline]
    unsafe fn get_mut(&self, idx: usize) -> &mut MaybeUninit<T> {
        // Since N is a power of two, N-1 is a mask covering N
        // elements overflowing when N elements have been added.
        // Indexes are left growing indefinetely and naturally wraps
        // around once the index increment reaches usize::MAX.
        &mut (*self.buffer.get())[idx & (N - 1)]
    }
}

impl<T, const N: usize> Drop for RingBuffer<T, N> {
    fn drop(&mut self) {
        let mut idx_r = self.idx_r.load(Ordering::Acquire);
        let idx_w = self.idx_w.load(Ordering::Acquire);

        while idx_r != idx_w {
            let t =
                unsafe { mem::replace(self.get_mut(idx_r), MaybeUninit::uninit()).assume_init() };
            mem::drop(t);
            idx_r = idx_r.wrapping_add(1);
        }
    }
}

pub struct RingBufferWriter<T, const N: usize> {
    inner: Arc<RingBuffer<T, N>>,
    cached_idx_r: usize,
    local_idx_w: usize,
}

impl<T, const N: usize> RingBufferWriter<T, N> {
    #[inline]
    pub fn push(&mut self, t: T) -> Option<T> {
        // Check if the ring buffer is potentially full.
        // This happens when the difference between the write and read indexes equals
        // the ring buffer capacity. Note that the write and read indexes are left growing
        // indefinitely, so we need to compute the difference by accounting for any eventual
        // overflow. This requires wrapping the subtraction operation.
        if self.local_idx_w.wrapping_sub(self.cached_idx_r) == N {
            self.cached_idx_r = self.inner.idx_r.load(Ordering::Acquire);
            // Check if the ring buffer is really full
            if self.local_idx_w.wrapping_sub(self.cached_idx_r) == N {
                return Some(t);
            }
        }

        // Insert the element in the ring buffer
        unsafe { mem::replace(self.inner.get_mut(self.local_idx_w), MaybeUninit::new(t)) };
        // Let's increment the counter and let it grow indefinitely and potentially overflow resetting it to 0.
        self.local_idx_w = self.local_idx_w.wrapping_add(1);
        self.inner.idx_w.store(self.local_idx_w, Ordering::Release);

        None
    }
}

pub struct RingBufferReader<T, const N: usize> {
    inner: Arc<RingBuffer<T, N>>,
    local_idx_r: usize,
    cached_idx_w: usize,
}

impl<T, const N: usize> RingBufferReader<T, N> {
    /// Calculate the number of elements currently in the ring buffer
    pub fn len(&self) -> usize {
        let write_index = self.inner.idx_w.load(Ordering::Acquire);
        let read_index = self.local_idx_r;

        // Log the current read and write indices
        // println!("[Debug] RingBufferReader - Write index: {}, Read index: {}", write_index, read_index);

        // If the write index is greater than or equal to the read index, calculate the difference directly
        if write_index >= read_index {
            let length = write_index - read_index;
            // println!("[Debug] RingBufferReader - Current length (direct): {}", length);
            length
        } else {
            // If the write index has wrapped around, add the buffer size to the difference
            let length = (write_index + N) - read_index;
            // println!("[Debug] RingBufferReader - Current length (wrapped): {}", length);
            length
        }
    }

    #[inline]
    pub fn pull(&mut self) -> Option<T> {
        // Check if the ring buffer is potentially empty
        // println!("[Debug] RingBufferReader - Attempting to pull element");
        if self.local_idx_r == self.cached_idx_w {
            // Update the write index
            self.cached_idx_w = self.inner.idx_w.load(Ordering::Acquire);
            // println!("[Debug] RingBufferReader - Updated Write index: {}, Read index: {}", self.cached_idx_w, self.local_idx_r);

            // Check if the ring buffer is really empty
            if self.local_idx_r == self.cached_idx_w {
                // println!("[Debug] RingBufferReader - Ring buffer is empty");
                return None;
            }
        }
        // Remove the element from the ring buffer
        let t = unsafe {
            // println!("[Debug] RingBufferReader - Removing element at index {}", self.local_idx_r);
            mem::replace(self.inner.get_mut(self.local_idx_r), MaybeUninit::uninit()).assume_init()
        };
        // Let's increment the counter and let it grow indefinitely
        // and potentially overflow resetting it to 0.
        self.local_idx_r = self.local_idx_r.wrapping_add(1);
        self.inner.idx_r.store(self.local_idx_r, Ordering::Release);
        // println!("[Debug] RingBufferReader - Updated Read index to {}", self.local_idx_r);

        Some(t)
    }
}
