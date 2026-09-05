//! Poison-tolerant lock helpers (#143).
//!
//! Petal treats a poisoned lock as recoverable: a panic while some *other*
//! thread held a guard should not cascade into every subsequent locker
//! `unwrap()`-panicking in turn. The data behind these locks is small,
//! self-consistent, and cheap to reason about after a poison, so the policy
//! everywhere is "take the inner guard and carry on."
//!
//! Before this module that policy was open-coded as
//! `.lock().unwrap_or_else(|e| e.into_inner())` at ~90 call sites. These
//! extension traits collapse it to `.lock_unpoisoned()` /
//! `.read_unpoisoned()` / `.write_unpoisoned()` with byte-identical
//! semantics (same `into_inner()` recovery, no behavior change).

use std::sync::{Mutex, MutexGuard};

/// `Mutex::lock` that recovers the guard on poison instead of panicking.
///
/// (Every lock in this crate today is a `Mutex`; if an `RwLock` is ever
/// introduced, mirror this with a `read_unpoisoned`/`write_unpoisoned` trait.)
pub trait MutexExt<T: ?Sized> {
    /// Lock, taking the inner guard even if the mutex was poisoned.
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
    #[inline]
    fn lock_unpoisoned(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn mutex_recovers_after_poison() {
        let m = Arc::new(Mutex::new(7u32));
        let m2 = Arc::clone(&m);
        // Poison the mutex by panicking while holding the guard.
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.lock().is_err(), "precondition: mutex should be poisoned");
        // The extension trait still yields the (unchanged) inner value.
        assert_eq!(*m.lock_unpoisoned(), 7);
    }
}
