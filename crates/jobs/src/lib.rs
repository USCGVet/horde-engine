//! Fork-join job pool for the fixed-rate simulation.
//!
//! Each tick runs a few parallel passes back to back, then the server idles until the next tick. So after
//! a pass, workers spin briefly (the next pass usually starts within microseconds) and otherwise sleep on
//! a condition variable: an idle server costs the client nothing. The calling thread always works on the
//! pass too, and passes are split into chunks that threads claim from a shared counter.

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::marker::PhantomData;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// How long a worker keeps spinning for the next pass before it goes to sleep.
const SPIN_BEFORE_SLEEP: Duration = Duration::from_micros(250);

type Task = dyn Fn(usize) + Sync;

struct Shared {
    /// Bumped once per pass (and once more at shutdown). Workers wait for it to change.
    generation: AtomicU64,
    /// The current pass's task, lifetime-erased. See `JobPool::run` for why that's sound.
    task: UnsafeCell<Option<*const Task>>,
    chunk_count: AtomicUsize,
    next_chunk: AtomicUsize,
    /// Workers that haven't finished the current pass yet.
    pending: AtomicUsize,
    shutdown: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

// SAFETY: `task` is written only by `JobPool::run` while no worker is inside a pass (`pending` is 0),
// before the new generation is published with Release ordering. Workers read it only after observing that
// generation with Acquire ordering, and stop using it before decrementing `pending`.
unsafe impl Sync for Shared {}
// SAFETY: as above; the raw pointer is only a shared reference to a `Sync` closure.
unsafe impl Send for Shared {}

pub struct JobPool {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl JobPool {
    /// One worker per logical CPU in `cpus`, each pinned to its CPU.
    pub fn pinned(cpus: &[usize]) -> JobPool {
        JobPool::spawn(cpus.iter().map(|&cpu| Some(cpu)).collect())
    }

    /// `workers` threads left to the OS scheduler (tests, or machines without a CPU plan).
    pub fn unpinned(workers: usize) -> JobPool {
        JobPool::spawn(vec![None; workers])
    }

    fn spawn(cpus: Vec<Option<usize>>) -> JobPool {
        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            task: UnsafeCell::new(None),
            chunk_count: AtomicUsize::new(0),
            next_chunk: AtomicUsize::new(0),
            pending: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            lock: Mutex::new(()),
            wake: Condvar::new(),
        });
        let workers = cpus
            .into_iter()
            .enumerate()
            .map(|(i, cpu)| {
                let shared = Arc::clone(&shared);
                thread::Builder::new()
                    .name(format!("sim-worker-{i}"))
                    .spawn(move || {
                        if let Some(cpu) = cpu {
                            horde_cpu::pin_current_thread(cpu);
                        }
                        worker_loop(&shared);
                    })
                    .expect("failed to start a simulation worker thread")
            })
            .collect();
        JobPool { shared, workers }
    }

    /// Threads that work on each pass: the workers plus the caller.
    pub fn threads(&self) -> usize {
        self.workers.len() + 1
    }

    /// Calls `task(chunk)` once for every chunk in `0..chunks`, spread over all threads, and returns when
    /// every call has finished.
    pub fn run(&self, chunks: usize, task: &(dyn Fn(usize) + Sync)) {
        if self.workers.is_empty() || chunks <= 1 {
            (0..chunks).for_each(task);
            return;
        }
        let s = &*self.shared;
        // SAFETY: only the lifetime is erased. Workers use the pointer between seeing this pass's
        // generation and decrementing `pending`, and this function doesn't return before `pending` is 0,
        // so the closure outlives every use.
        let erased = unsafe { std::mem::transmute::<*const (dyn Fn(usize) + Sync + '_), *const Task>(task) };
        // SAFETY: the previous pass finished (`pending` reached 0), so no worker is reading `task`.
        unsafe { *s.task.get() = Some(erased) };
        s.chunk_count.store(chunks, Ordering::Relaxed);
        s.next_chunk.store(0, Ordering::Relaxed);
        s.pending.store(self.workers.len(), Ordering::Relaxed);
        s.generation.fetch_add(1, Ordering::Release);
        // Taking the lock means any worker that checked `generation` before the bump is now waiting on
        // the condvar, so the notification can't be missed.
        drop(s.lock.lock().unwrap_or_else(PoisonError::into_inner));
        s.wake.notify_all();

        drain(s, task);
        let mut spins = 0u32;
        while s.pending.load(Ordering::Acquire) != 0 {
            if spins < 4_000 {
                spins += 1;
                spin_loop();
            } else {
                thread::yield_now();
            }
        }
    }

    /// Calls `f` on consecutive ranges of `0..len`, `chunk_size` items each (the last may be shorter).
    /// Chunk `k` covers `k * chunk_size..`, so `range.start / chunk_size` identifies the chunk.
    pub fn for_ranges(&self, len: usize, chunk_size: usize, f: impl Fn(Range<usize>) + Sync) {
        let chunk_size = chunk_size.max(1);
        self.run(len.div_ceil(chunk_size), &|chunk| {
            let start = chunk * chunk_size;
            f(start..(start + chunk_size).min(len));
        });
    }
}

impl Drop for JobPool {
    fn drop(&mut self) {
        let s = &*self.shared;
        s.shutdown.store(true, Ordering::Release);
        s.generation.fetch_add(1, Ordering::Release);
        drop(s.lock.lock().unwrap_or_else(PoisonError::into_inner));
        s.wake.notify_all();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(s: &Shared) {
    let mut seen = 0;
    loop {
        seen = wait_for_pass(s, seen);
        if s.shutdown.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: `run` stored the task before publishing the generation we just observed, and keeps it
        // alive until this worker decrements `pending` below.
        let task = unsafe { &*(*s.task.get()).expect("pass published without a task") };
        drain(s, task);
        s.pending.fetch_sub(1, Ordering::Release);
    }
}

fn wait_for_pass(s: &Shared, seen: u64) -> u64 {
    let start = Instant::now();
    let mut spins = 0u32;
    loop {
        let current = s.generation.load(Ordering::Acquire);
        if current != seen {
            return current;
        }
        spins = spins.wrapping_add(1);
        if spins.is_multiple_of(64) && start.elapsed() >= SPIN_BEFORE_SLEEP {
            break;
        }
        spin_loop();
    }
    let mut guard = s.lock.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let current = s.generation.load(Ordering::Acquire);
        if current != seen {
            return current;
        }
        guard = s.wake.wait(guard).unwrap_or_else(PoisonError::into_inner);
    }
}

fn drain(s: &Shared, task: &(dyn Fn(usize) + Sync + '_)) {
    let chunks = s.chunk_count.load(Ordering::Relaxed);
    loop {
        let chunk = s.next_chunk.fetch_add(1, Ordering::Relaxed);
        if chunk >= chunks {
            break;
        }
        task(chunk);
    }
}

/// A mutable slice shared with a parallel pass in which each index is written by at most one task.
pub struct DisjointSlice<'a, T> {
    ptr: *mut T,
    len: usize,
    _data: PhantomData<&'a mut [T]>,
}

// SAFETY: all access goes through `get_mut`, whose contract keeps tasks on disjoint indices, so sharing
// the slice between threads is like handing each thread its own `&mut` to separate elements.
unsafe impl<T: Send> Sync for DisjointSlice<'_, T> {}

impl<'a, T> DisjointSlice<'a, T> {
    pub fn new(data: &'a mut [T]) -> DisjointSlice<'a, T> {
        DisjointSlice { ptr: data.as_mut_ptr(), len: data.len(), _data: PhantomData }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// # Safety
    /// `i < self.len()`, and no other task touches index `i` during the current parallel pass.
    #[allow(clippy::mut_from_ref)]
    #[inline(always)]
    pub unsafe fn get_mut(&self, i: usize) -> &mut T {
        debug_assert!(i < self.len);
        // SAFETY: in bounds and exclusive to this task, per the caller's contract.
        unsafe { &mut *self.ptr.add(i) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_chunk_runs_exactly_once() {
        let pool = JobPool::unpinned(3);
        for round in 0..500 {
            let counts: Vec<AtomicUsize> = (0..997).map(|_| AtomicUsize::new(0)).collect();
            pool.run(counts.len(), &|chunk| {
                counts[chunk].fetch_add(1, Ordering::Relaxed);
            });
            assert!(counts.iter().all(|c| c.load(Ordering::Relaxed) == 1), "round {round}");
        }
    }

    #[test]
    fn disjoint_writes_are_visible_after_the_pass() {
        let pool = JobPool::unpinned(4);
        let mut data = vec![0u64; 10_000];
        for round in 1..=50u64 {
            let out = DisjointSlice::new(&mut data);
            pool.for_ranges(out.len(), 64, |range| {
                for i in range {
                    // SAFETY: each index belongs to exactly one range.
                    unsafe { *out.get_mut(i) += i as u64 * round };
                }
            });
        }
        let total: u64 = (1..=50).sum();
        assert!(data.iter().enumerate().all(|(i, &v)| v == i as u64 * total));
    }

    #[test]
    fn works_without_workers() {
        let pool = JobPool::unpinned(0);
        let hits = AtomicUsize::new(0);
        pool.for_ranges(1000, 10, |range| {
            hits.fetch_add(range.len(), Ordering::Relaxed);
        });
        assert_eq!(hits.into_inner(), 1000);
    }
}
