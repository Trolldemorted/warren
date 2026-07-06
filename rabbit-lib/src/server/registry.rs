use crate::server::handle::AgentHandle;
use dashmap::DashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::Notify;
use uuid::Uuid;

/// Shared live-agent map. One entry per `rabbit` process whose WS
/// handshake has reached the registry-insert step in
/// `ws_rabbit::handle_session`. Reused by the browser/shell WS handlers,
/// action-button HTTP routes, and the actor's `set_recorder_url` /
/// `publish_meta` broadcasts.
///
/// Holds a `tokio::sync::Notify` alongside the map so that callers
/// opening a browser WS *before* a rabbit has connected can wait for
/// the rabbit to register instead of erroring out the WS upgrade.
/// Holding the WS open through the gap eliminates a one-second-class
/// flicker on the `/agent/:id/claude` page when no rabbit is running.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<AgentRegistryInner>,
}

pub struct AgentRegistryInner {
    /// Live map of `agent_id → AgentHandle`. All readers/writers go
    /// through `DashMap`'s shard locks; the methods on
    /// `AgentRegistryInner` are forwarders so existing call sites that
    /// previously called `state.live.contains_key(&id)` keep working.
    pub map: DashMap<Uuid, AgentHandle>,
    /// Signals waiters that a *new* handle has been inserted via
    /// `register`. Waiters must re-check the map after wakeup — the
    /// permit can be consumed even if their target id was a duplicate
    /// insert (a stale wake after a teardown, or an insert for a
    /// different id than the one this caller is waiting on).
    pub arrived: Notify,
}

pub fn new_registry() -> AgentRegistry {
    AgentRegistry {
        inner: Arc::new(AgentRegistryInner {
            map: DashMap::new(),
            arrived: Notify::new(),
        }),
    }
}

impl AgentRegistry {
    /// Forwarder — `state.live.contains_key(&id)`.
    pub fn contains_key(&self, id: &Uuid) -> bool {
        self.inner.contains_key(id)
    }

    /// Forwarder — `state.live.get(&id)`. Returns a `DashMap`
    /// read-guard holding a shard lock; release it (drop the guard)
    /// before any I/O.
    pub fn get(&self, id: &Uuid) -> Option<dashmap::mapref::one::Ref<'_, Uuid, AgentHandle>> {
        self.inner.get(id)
    }

    /// Forwarder — `state.live.get_mut(&id)`. Returns a write-guard.
    pub fn get_mut(
        &self,
        id: &Uuid,
    ) -> Option<dashmap::mapref::one::RefMut<'_, Uuid, AgentHandle>> {
        self.inner.get_mut(id)
    }

    /// Forwarder — `state.live.entry(id).or_insert_with(...)`.
    /// Prefer `register` for new code that wants the notifier wakeup.
    pub fn entry(&self, id: Uuid) -> dashmap::mapref::entry::Entry<'_, Uuid, AgentHandle> {
        self.inner.entry(id)
    }

    /// Iterate over `(agent_id, handle)` pairs.
    pub fn iter(&self) -> dashmap::iter::Iter<'_, Uuid, AgentHandle> {
        self.inner.iter()
    }

    /// Returns a future that completes the next time `register` signals
    /// the notify. Waiters must re-check `get` after wakeup.
    pub fn wait_for_arrival(&self) -> impl Future<Output = ()> + Send + '_ {
        self.inner.wait_for_arrival()
    }

    /// Insert (or reuse) the entry for `agent_id` and wake one waiter.
    /// Used by `ws_rabbit::handle_session` after the rabbit WS handshake
    /// completes so any browser WS already past the "no rabbit yet"
    /// gate can proceed.
    pub fn register(&self, agent_id: Uuid) -> AgentHandle {
        self.inner.register(agent_id)
    }
}

impl AgentRegistryInner {
    /// Forwarder — `state.live.contains_key(&id)`.
    pub fn contains_key(&self, id: &Uuid) -> bool {
        self.map.contains_key(id)
    }

    /// Forwarder — `state.live.get(&id)`. Returns a `DashMap` read-guard
    /// holding a shard lock; release it (drop the guard) before any I/O.
    pub fn get(&self, id: &Uuid) -> Option<dashmap::mapref::one::Ref<'_, Uuid, AgentHandle>> {
        self.map.get(id)
    }

    /// Forwarder — `state.live.get_mut(&id)`. Returns a write-guard.
    pub fn get_mut(
        &self,
        id: &Uuid,
    ) -> Option<dashmap::mapref::one::RefMut<'_, Uuid, AgentHandle>> {
        self.map.get_mut(id)
    }

    /// Forwarder — `state.live.entry(id).or_insert_with(...)`.
    /// Prefer `register` for new code that wants the notifier wakeup.
    pub fn entry(
        &self,
        id: Uuid,
    ) -> dashmap::mapref::entry::Entry<'_, Uuid, AgentHandle> {
        self.map.entry(id)
    }

    /// Iterate over `(agent_id, handle)` pairs.
    pub fn iter(&self) -> dashmap::iter::Iter<'_, Uuid, AgentHandle> {
        self.map.iter()
    }

    /// Returns a future that completes the next time `register` signals
    /// the notify. Waiters must re-check `get` after wakeup.
    pub fn wait_for_arrival(&self) -> impl Future<Output = ()> + Send + '_ {
        self.arrived.notified()
    }

    /// Insert (or reuse) the entry for `agent_id` and wake one waiter.
    /// Used by `ws_rabbit::handle_session` after the rabbit WS handshake
    /// completes so any browser WS already past the "no rabbit yet"
    /// gate can proceed.
    pub fn register(&self, agent_id: Uuid) -> AgentHandle {
        let h = self
            .map
            .entry(agent_id)
            .or_insert_with(|| AgentHandle::new(agent_id))
            .clone();
        // Always notify — over-waking is harmless (waiters re-check
        // `get`) and we want every browser WS that opened before this
        // rabbit to wake up immediately.
        self.arrived.notify_one();
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_then_get_returns_handle() {
        let reg = new_registry();
        let id = Uuid::new_v4();
        assert!(reg.inner.get(&id).is_none());
        let h = reg.inner.register(id);
        let stored = reg.inner.get(&id).expect("entry exists").clone();
        assert_eq!(stored.agent_id, h.agent_id);
    }

    #[tokio::test]
    async fn wait_for_arrival_wakes_after_register() {
        let reg = new_registry();
        let id = Uuid::new_v4();
        // Trigger an arrival from another task first; pre-acquire the
        // permit so `wait_for_arrival` returns immediately on the
        // current task.
        let spawned = {
            let reg = reg.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                reg.inner.register(id);
            })
        };
        let woken = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            reg.inner.wait_for_arrival(),
        )
        .await;
        spawned.await.unwrap();
        assert!(woken.is_ok(), "register must wake at least one waiter");
        assert!(reg.inner.contains_key(&id));
    }
}
