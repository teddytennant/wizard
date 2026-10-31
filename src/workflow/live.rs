//! The workflow runs this process is executing, so `/workflows` can stop one
//! without a handle on the session that launched it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Notify;

/// A stop request a run polls and awaits.
#[derive(Debug, Default)]
pub struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// Resolves once [`Cancel::cancel`] has been called.
    pub async fn cancelled(&self) {
        loop {
            let waiting = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            waiting.await;
        }
    }
}

fn table() -> &'static Mutex<HashMap<String, Arc<Cancel>>> {
    static LIVE: OnceLock<Mutex<HashMap<String, Arc<Cancel>>>> = OnceLock::new();
    LIVE.get_or_init(Mutex::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Cancel>>> {
    table().lock().unwrap_or_else(|e| e.into_inner())
}

/// Record that this process is running `run_id`.
pub fn register(run_id: &str, cancel: Arc<Cancel>) {
    lock().insert(run_id.to_string(), cancel);
}

pub fn unregister(run_id: &str) {
    lock().remove(run_id);
}

pub fn is_live(run_id: &str) -> bool {
    lock().contains_key(run_id)
}

/// Ask a run in this process to stop. False when it is not running here.
pub fn stop(run_id: &str) -> bool {
    match lock().get(run_id) {
        Some(cancel) => {
            cancel.cancel();
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stop_wakes_a_waiter_and_unregister_forgets_the_run() {
        let cancel = Cancel::new();
        register("wf_test0001-aaa", Arc::clone(&cancel));
        assert!(is_live("wf_test0001-aaa"));
        let waiter = {
            let cancel = Arc::clone(&cancel);
            tokio::spawn(async move { cancel.cancelled().await })
        };
        assert!(stop("wf_test0001-aaa"));
        tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("the waiter woke")
            .unwrap();
        unregister("wf_test0001-aaa");
        assert!(!is_live("wf_test0001-aaa"));
        assert!(!stop("wf_test0001-aaa"));
    }

    #[tokio::test]
    async fn a_stop_before_the_wait_is_not_missed() {
        let cancel = Cancel::new();
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(2), cancel.cancelled())
            .await
            .expect("already cancelled");
    }
}
