//! The one task per account that concurrent callers join, for the refresh and the usage service's reads.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt, Shared};

pub(crate) type Joined<T> = Shared<BoxFuture<'static, T>>;

pub(crate) type InflightMap<O, V> = fn(&O) -> &Mutex<HashMap<String, V>>;

/// Removes an account's in-flight entry from `map` once its task ends, however it ends. A task that ends by a panic
/// leaves no entry behind, or every later caller would join a future that has already resolved and the account would
/// stay stuck on that one result until the process restarts. A poisoned map is left alone rather than asserted on,
/// since a panic inside `Drop` while the task already unwinds aborts the process.
struct InflightGuard<O: 'static, V: 'static> {
    account_id: String,
    inner: Arc<O>,
    map: InflightMap<O, V>,
}

impl<O: 'static, V: 'static> Drop for InflightGuard<O, V> {
    fn drop(&mut self) {
        if let Ok(mut inflight) = (self.map)(&self.inner).lock() {
            inflight.remove(&self.account_id);
        }
    }
}

/// `task` spawned for the account under the guard that removes its entry from `map` once it ends, and recorded in
/// `inflight`, the map the caller holds locked.
pub(crate) fn spawn_into<O, T, F>(
    inflight: &mut HashMap<String, Joined<T>>,
    owner: &Arc<O>,
    map: InflightMap<O, Joined<T>>,
    account_id: &str,
    panicked: fn() -> T,
    task: impl FnOnce(Arc<O>, String) -> F,
) -> Joined<T>
where
    O: Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
    F: Future<Output = T> + Send + 'static,
{
    let work = task(owner.clone(), account_id.to_string());
    let guarded = owner.clone();
    let id = account_id.to_string();
    // The task runs in a spawn of its own, so a caller that goes away mid-task only stops awaiting, and the task's
    // writes (a refresh's attempt row, commit and resolution) still run to the end. The guard drops the entry once the
    // task is over and takes the same map lock, which the caller holds across the spawn so a task that finishes at
    // once cannot remove the entry before it is inserted. Dropping the entry last also keeps the next caller from
    // starting a second task (for a refresh, a second rotation) while this one's writes are still in flight.
    let handle = tokio::spawn(async move {
        let _guard = InflightGuard {
            account_id: id,
            inner: guarded,
            map,
        };
        work.await
    });
    let shared = async move {
        match handle.await {
            Ok(outcome) => outcome,
            // Nothing else joins a detached task, so a join error is a panic in the task.
            Err(_) => panicked(),
        }
    }
    .boxed()
    .shared();
    inflight.insert(account_id.to_string(), shared.clone());
    shared
}
