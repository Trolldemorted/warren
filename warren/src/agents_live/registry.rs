use crate::agents_live::handle::AgentHandle;
use dashmap::DashMap;
use std::sync::Arc;
use uuid::Uuid;

pub type AgentRegistry = Arc<DashMap<Uuid, AgentHandle>>;

pub fn new_registry() -> AgentRegistry {
    Arc::new(DashMap::new())
}
