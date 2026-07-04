pub mod actor;
pub mod handle;
pub mod http;
pub mod registry;
pub mod wire;
pub mod ws_browser;
pub mod ws_rabbit;

pub use handle::AgentHandle;
pub use registry::{new_registry, AgentRegistry};
