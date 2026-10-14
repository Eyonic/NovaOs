//! A minimal spinlock.
//!
//! Nova runs on one CPU and interrupt handlers never take a lock, so the only
//! way to find a lock held is re-entry from an exception or panic raised while
//! the lock was held. Callers on those paths use [`SpinLock::try_lock`] and skip
//! the protected work instead of deadlocking.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialised by `locked`.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        (!self.locked.swap(true, Ordering::Acquire)).then_some(SpinLockGuard { lock: self })
    }

    /// Lock, letting other tasks run while it is held elsewhere. Only for task
    /// context (never IRQ handlers, never fault reporting): the holder may be a task
    /// that yielded inside the critical section, e.g. waiting for a disk.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        loop {
            if let Some(guard) = self.try_lock() {
                return guard;
            }
            crate::task::yield_now();
        }
    }
}

pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the guard proves exclusive access.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
