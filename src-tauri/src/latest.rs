// SPDX-License-Identifier: MIT OR Apache-2.0

//! One operation of a kind at a time: starting another, or cancelling, stops the one running.
//! Used for progressive searches and metadata prefetches, whose results only matter for the
//! latest request.

use tokio::sync::watch;

/// Hands out stop signals; each [`Latest::begin`] stops the operation begun before it.
#[derive(Debug)]
pub(crate) struct Latest(watch::Sender<u64>);

impl Default for Latest {
    fn default() -> Self {
        Self(watch::Sender::new(0))
    }
}

impl Latest {
    /// Stops the running operation, if any, and returns a future that completes when the
    /// caller's operation is stopped in turn.
    pub(crate) fn begin(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        self.cancel();
        let mut generation = self.0.subscribe();
        async move {
            if generation.changed().await.is_err() {
                // The state is gone (the app is closing): nothing will stop this operation.
                std::future::pending::<()>().await;
            }
        }
    }

    /// Stops the running operation, if any.
    pub(crate) fn cancel(&self) {
        self.0.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// The current operation's number: [`Latest::begin`] and [`Latest::cancel`] change it, so
    /// an operation that read it right after its own `begin` can check with
    /// [`Latest::is_current`] that nothing stopped it since.
    pub(crate) fn generation(&self) -> u64 {
        *self.0.borrow()
    }

    /// Whether no operation began, and nothing was cancelled, since `generation` was read.
    pub(crate) fn is_current(&self, generation: u64) -> bool {
        *self.0.borrow() == generation
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_new_operation_stops_the_running_one() {
        let latest = Latest::default();
        let first = latest.begin();
        let second = latest.begin();
        let wait = Duration::from_millis(1);
        assert!(tokio::time::timeout(wait, first).await.is_ok(), "stopped");
        let second = tokio::spawn(second);
        tokio::task::yield_now().await;
        assert!(!second.is_finished(), "still running");
        latest.cancel();
        assert!(
            tokio::time::timeout(wait, second).await.is_ok(),
            "cancelled"
        );
    }
}
