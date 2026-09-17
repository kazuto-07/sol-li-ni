//! Tasks that die with their owner.
//!
//! Aborting a tokio task drops its future, but tasks *it* spawned keep running: a `JoinHandle`
//! going out of scope detaches the task rather than stopping it. A call spawns tasks holding
//! Deepgram and Murf sockets, so without this a finished call leaves billable streams open —
//! kept alive indefinitely by Deepgram's KeepAlive.

use tokio::task::JoinHandle;

/// A `JoinHandle` that aborts its task when dropped, including when the owning task is aborted.
pub struct AbortOnDrop<T>(Option<JoinHandle<T>>);

impl<T> AbortOnDrop<T> {
    pub fn new(task: JoinHandle<T>) -> Self {
        Self(Some(task))
    }

    /// Hands the task over to a new owner, who becomes responsible for stopping it.
    pub fn keep(mut self) -> JoinHandle<T> {
        self.0.take().expect("taken once")
    }

    pub fn abort(&self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }

    pub fn is_finished(&self) -> bool {
        self.0.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Waits for the task, keeping the abort-on-drop guarantee while waiting.
    pub async fn join(&mut self) -> Result<T, tokio::task::JoinError> {
        self.0.as_mut().expect("not yet taken").await
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    /// The bug this exists for: aborting a call must stop the tasks the call spawned.
    #[tokio::test]
    async fn aborting_the_owner_stops_its_children() {
        let child_still_running = Arc::new(AtomicBool::new(false));

        let owner = tokio::spawn({
            let flag = Arc::clone(&child_still_running);
            async move {
                let _child = AbortOnDrop::new(tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    flag.store(true, Ordering::SeqCst);
                }));
                std::future::pending::<()>().await;
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        owner.abort();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(!child_still_running.load(Ordering::SeqCst), "the child outlived its owner");
    }

    #[tokio::test]
    async fn a_plain_join_handle_would_have_leaked() {
        // Documents the tokio behaviour the guard corrects.
        let child_ran = Arc::new(AtomicBool::new(false));
        let owner = tokio::spawn({
            let flag = Arc::clone(&child_ran);
            async move {
                let _child = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    flag.store(true, Ordering::SeqCst);
                });
                std::future::pending::<()>().await;
            }
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        owner.abort();
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(child_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn kept_tasks_are_not_aborted() {
        let guard = AbortOnDrop::new(tokio::spawn(async { 7 }));
        assert_eq!(guard.keep().await.unwrap(), 7);
    }
}
