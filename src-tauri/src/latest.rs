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
        self.begin_numbered().1
    }

    /// [`Latest::begin`], also returning the new operation's number for [`Latest::is_current`].
    /// Both come from the one change, so an operation begun at the same time on another thread
    /// cannot slip in between.
    pub(crate) fn begin_numbered(
        &self,
    ) -> (u64, impl Future<Output = ()> + Send + 'static + use<>) {
        let mut changes = self.0.subscribe();
        let mut generation = 0;
        self.0.send_modify(|g| {
            *g = g.wrapping_add(1);
            generation = *g;
        });
        let stopped = async move {
            if changes.wait_for(|g| *g != generation).await.is_err() {
                // The state is gone (the app is closing): nothing will stop this operation.
                std::future::pending::<()>().await;
            }
        };
        (generation, stopped)
    }

    /// Stops the running operation, if any.
    pub(crate) fn cancel(&self) {
        self.0.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// Whether no operation began, and nothing was cancelled, since the one numbered
    /// `generation` (see [`Latest::begin_numbered`]).
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

    #[tokio::test(start_paused = true)]
    async fn numbered_operations_know_whether_they_are_current() {
        let latest = Latest::default();
        let (first, stopped) = latest.begin_numbered();
        assert!(latest.is_current(first));
        let (second, _running) = latest.begin_numbered();
        assert_ne!(first, second);
        assert!(!latest.is_current(first));
        assert!(latest.is_current(second));
        let wait = Duration::from_millis(1);
        assert!(tokio::time::timeout(wait, stopped).await.is_ok(), "stopped");
    }

    #[test]
    fn concurrent_operations_never_share_a_number_or_miss_their_stop() {
        let latest = std::sync::Arc::new(Latest::default());
        let begun: Vec<_> = (0..8)
            .map(|_| {
                let latest = std::sync::Arc::clone(&latest);
                std::thread::spawn(move || {
                    (0..200)
                        .map(|_| latest.begin_numbered())
                        .collect::<Vec<_>>()
                })
            })
            .flat_map(|t| t.join().unwrap())
            .collect();
        let mut numbers: Vec<u64> = begun.iter().map(|(g, _)| *g).collect();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), begun.len());
        // Every operation but the last one begun was stopped by a later one.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let last = *numbers.last().unwrap();
        for (generation, stopped) in begun {
            let done = runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(50), stopped)
                    .await
                    .is_ok()
            });
            assert_eq!(done, generation != last, "operation {generation}");
        }
    }
}
