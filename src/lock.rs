//! Lightweight mutex abstraction for `Router`
//!
//! This module provides a small wrapper type, [`RouterMutex`], which
//! abstracts over `std::sync::Mutex` (when the `std` feature is enabled)
//! and host-provided lock hooks (`telemetry_lock` / `telemetry_unlock`) in
//! `no_std` builds.
//!
//! The goal is to provide a uniform API where:
//! - In `std` builds, a poisoned mutex will **panic** with
//!   `"RouterMutex poisoned"` rather than forcing callers to handle
//!   `PoisonError` everywhere.
//! - In `no_std` builds, locking delegates to platform hooks.
//!
//! Typical usage:
//!
//! ```text
//! let m = RouterMutex::new(router);
//! {
//!     let mut r = m.lock();
//!     // use `r`
//! }
//! ```

use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
pub struct ReentryGate {
    busy: AtomicBool,
}

pub struct ReentryGuard<'a> {
    busy: &'a AtomicBool,
}

impl ReentryGate {
    #[inline]
    pub const fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn try_enter(&self) -> Option<ReentryGuard<'_>> {
        self.busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| ReentryGuard { busy: &self.busy })
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
}

impl Drop for ReentryGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.busy.store(false, Ordering::Release);
    }
}

// ============================================================================
//  std implementation
// ============================================================================

#[cfg(feature = "std")]
#[derive(Debug)]
pub struct RouterMutex<T>(std::sync::Mutex<T>);

impl<T> Clone for RouterMutex<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        RouterMutex::new(self.lock().clone())
    }
}
#[cfg(feature = "std")]
impl<T> RouterMutex<T> {
    /// Create a new `RouterMutex` wrapping the given value.
    #[inline]
    pub fn new(v: T) -> Self {
        Self(std::sync::Mutex::new(v))
    }

    /// Acquire the lock, panicking if the mutex has been poisoned.
    ///
    /// This is a convenience for router usage where poisoning is treated
    /// as a fatal error. Internally calls `.lock().expect("RouterMutex poisoned")`.
    #[inline]
    pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.0.lock().expect("RouterMutex poisoned")
    }
}

// ============================================================================
//  no_std implementation via telemetry_lock / telemetry_unlock
// ============================================================================

#[cfg(not(feature = "std"))]
unsafe extern "C" {
    fn telemetry_lock();
    fn telemetry_unlock();
}

#[cfg(not(feature = "std"))]
pub struct RouterMutex<T>(core::cell::UnsafeCell<T>);

#[cfg(not(feature = "std"))]
unsafe impl<T: Send> Send for RouterMutex<T> {}

#[cfg(not(feature = "std"))]
unsafe impl<T: Send> Sync for RouterMutex<T> {}

#[cfg(not(feature = "std"))]
pub struct RouterMutexGuard<'a, T> {
    m: &'a RouterMutex<T>,
}

#[cfg(not(feature = "std"))]
impl<T> core::ops::Deref for RouterMutexGuard<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.m.0.get() }
    }
}

#[cfg(not(feature = "std"))]
impl<T> core::ops::DerefMut for RouterMutexGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.m.0.get() }
    }
}

#[cfg(not(feature = "std"))]
impl<T> Drop for RouterMutexGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        unsafe { telemetry_unlock() }
    }
}

#[cfg(not(feature = "std"))]
impl<T> RouterMutex<T> {
    /// Create a new `RouterMutex` wrapping the given value.
    #[inline]
    pub fn new(v: T) -> Self {
        Self(core::cell::UnsafeCell::new(v))
    }

    /// Acquire the lock.
    ///
    /// In `no_std` builds this uses host-provided `telemetry_lock` /
    /// `telemetry_unlock` hooks.
    #[inline]
    pub fn lock(&self) -> RouterMutexGuard<'_, T> {
        unsafe { telemetry_lock() }
        RouterMutexGuard { m: self }
    }
}
