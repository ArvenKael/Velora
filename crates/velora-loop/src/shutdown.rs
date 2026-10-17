use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

/// A cloneable handle that stops the loop.
///
/// The loop is OS-agnostic and does not install signal handlers itself: a
/// runtime decides what stopping means and calls [`Shutdown::trigger`]. That
/// keeps Ctrl-C, a supervisor's request, or a test's own decision out of the
/// core.
#[derive(Clone, Debug, Default)]
pub struct Shutdown {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    triggered: AtomicBool,
    notify: Notify,
}

impl Shutdown {
    /// Creates a handle that has not been triggered.
    pub fn new() -> Self {
        Shutdown::default()
    }

    /// Asks the loop to stop.
    ///
    /// Returns immediately; the loop stops at its next check. Triggering twice,
    /// or before the loop starts, is harmless — a loop that starts with an
    /// already-triggered handle stops immediately.
    pub fn trigger(&self) {
        self.inner.triggered.store(true, Ordering::SeqCst);
        self.inner.notify.notify_waiters();
    }

    /// Whether a stop has been requested.
    pub fn is_triggered(&self) -> bool {
        self.inner.triggered.load(Ordering::SeqCst)
    }

    /// Resolves once a stop has been requested.
    pub(crate) async fn wait(&self) {
        if self.is_triggered() {
            return;
        }

        // Register before re-checking, so a trigger landing between the check
        // and the await is not missed.
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        if self.is_triggered() {
            return;
        }

        notified.await;
    }
}
