//! Test-only allocation tracker: lets a regression test assert that a decoder stays within
//! a memory bound (invariants I3/I7) without needing an allocation failure to prove it.
//!
//! Counters are per thread, so tests running in parallel do not see each other's
//! allocations; measure only code that runs on the calling thread.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    // `const` initialisers: no lazy registration, so touching them inside the allocator is safe.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

pub struct Tracking;

fn grow(by: usize, request: usize) {
    LIVE.with(|live| {
        let now = live.get().saturating_add(by as isize);
        live.set(now);
        PEAK.with(|peak| peak.set(peak.get().max(now)));
    });
    LARGEST.with(|largest| largest.set(largest.get().max(request)));
}

fn shrink(by: usize) {
    LIVE.with(|live| live.set(live.get().saturating_sub(by as isize)));
}

// SAFETY: every method forwards to `System` with the caller's layout unchanged; the counters
// only observe sizes.
unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size(), layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grow(layout.size(), layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        shrink(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grow(new_size - layout.size(), new_size);
            } else {
                shrink(layout.size() - new_size);
            }
        }
        p
    }
}

/// What [`measure`] observed.
#[derive(Clone, Copy, Debug)]
pub struct Usage {
    /// Highest live heap above the level at entry, in bytes.
    pub peak: usize,
    /// Largest single allocation (or realloc target) requested, in bytes.
    pub largest: usize,
}

/// Runs `f` on this thread and reports its heap usage next to its result.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, Usage) {
    let start = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(start));
    LARGEST.with(|l| l.set(0));
    let out = f();
    let peak = PEAK.with(Cell::get).saturating_sub(start).max(0) as usize;
    let largest = LARGEST.with(Cell::get);
    (out, Usage { peak, largest })
}
