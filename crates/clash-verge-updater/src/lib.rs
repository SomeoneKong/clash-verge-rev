//! Shared parts of the privileged update component: the payload format and its signature, the
//! IPC protocol, the persistent state and the audit log. The GUI and the component both use them.

pub mod audit;
pub mod client;
pub mod keys;
pub mod manifest;
pub mod paths;
pub mod payload;
pub mod protocol;
pub mod signature;
pub mod state;
pub mod sys;
pub mod version;
