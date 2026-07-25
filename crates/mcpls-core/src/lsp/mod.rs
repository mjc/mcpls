//! LSP client implementation.
//!
//! This module provides the LSP client for communicating with language servers
//! over JSON-RPC 2.0.

mod client;
mod lifecycle;
mod transport;
pub(crate) mod types;

pub use client::LspClient;
pub use lifecycle::{LspServer, ServerInitConfig, ServerInitResult, ServerState};
pub(crate) use lifecycle::{load_project_environment, resolve_command};
pub use transport::LspTransport;
pub use types::{
    InboundMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LspNotification,
    RequestId,
};
