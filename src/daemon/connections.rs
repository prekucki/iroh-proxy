//! Registry of in-flight proxied connections, surfaced over the control plane.

use std::collections::HashMap;
use std::sync::{
    Arc, Mutex as StdMutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone)]
pub(super) struct ActiveConnection {
    pub(super) src: Box<str>,
    pub(super) kind: Box<str>,
    pub(super) dst: Box<str>,
}

/// Shared handle to the table of active connections. [`register`] returns a
/// guard whose `Drop` removes the entry and emits a `connection-closed` state
/// event, so entries cannot outlive the task that owns them.
///
/// [`register`]: ConnectionRegistry::register
#[derive(Clone)]
pub(super) struct ConnectionRegistry {
    active: Arc<StdMutex<HashMap<u64, ActiveConnection>>>,
    next_id: Arc<AtomicU64>,
    state_tx: UnboundedSender<Box<str>>,
}

impl ConnectionRegistry {
    pub(super) fn new(state_tx: UnboundedSender<Box<str>>) -> Self {
        Self {
            active: Arc::new(StdMutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            state_tx,
        }
    }

    pub(super) fn register(&self, info: ActiveConnection) -> ActiveConnGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock_active().insert(id, info);
        let _ = self.state_tx.send("connection-opened".into());
        ActiveConnGuard {
            id,
            registry: self.clone(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.lock_active().len()
    }

    pub(super) fn snapshot(&self) -> Vec<(u64, ActiveConnection)> {
        self.lock_active()
            .iter()
            .map(|(id, conn)| (*id, conn.clone()))
            .collect()
    }

    fn lock_active(&self) -> MutexGuard<'_, HashMap<u64, ActiveConnection>> {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub(super) struct ActiveConnGuard {
    id: u64,
    registry: ConnectionRegistry,
}

impl Drop for ActiveConnGuard {
    fn drop(&mut self) {
        self.registry.lock_active().remove(&self.id);
        let _ = self.registry.state_tx.send("connection-closed".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_conn(tag: &str) -> ActiveConnection {
        ActiveConnection {
            src: tag.into(),
            kind: "test".into(),
            dst: tag.into(),
        }
    }

    #[test]
    fn register_connection_tracks_inserts_and_guard_drop_removes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Box<str>>();
        let registry = ConnectionRegistry::new(tx);

        let g1 = registry.register(sample_conn("a"));
        let g2 = registry.register(sample_conn("b"));

        assert_eq!(registry.len(), 2);
        assert!(
            g1.id < g2.id,
            "connection ids must be monotonically increasing"
        );
        assert_eq!(rx.try_recv().unwrap().as_ref(), "connection-opened");
        assert_eq!(rx.try_recv().unwrap().as_ref(), "connection-opened");

        drop(g1);
        assert_eq!(registry.len(), 1);
        assert_eq!(rx.try_recv().unwrap().as_ref(), "connection-closed");

        drop(g2);
        assert_eq!(registry.len(), 0);
        assert_eq!(rx.try_recv().unwrap().as_ref(), "connection-closed");
    }

    #[test]
    fn register_connection_recovers_from_a_poisoned_lock() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Box<str>>();
        let registry = ConnectionRegistry::new(tx);

        // Poison the mutex by panicking while holding the guard.
        let poison_target = Arc::clone(&registry.active);
        let prev_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = poison_target.lock().unwrap();
            panic!("intentional poison");
        }));
        std::panic::set_hook(prev_hook);
        assert!(registry.active.is_poisoned());

        // register / guard Drop both use unwrap_or_else(into_inner), so they
        // must keep working despite the poison.
        let guard = registry.register(sample_conn("a"));
        assert_eq!(registry.len(), 1);
        drop(guard);
        assert_eq!(registry.len(), 0);
    }
}
