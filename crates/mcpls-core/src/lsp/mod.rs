//! LSP client implementation.
//!
//! This module provides the LSP client for communicating with language servers
//! over JSON-RPC 2.0.

mod client;
mod lifecycle;
mod notification;
mod transport;
pub(crate) mod types;
pub(crate) mod watcher;

/// The tracked-document scope affected by a native filesystem watcher signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchInvalidation {
    /// Concrete file paths whose snapshots are no longer current.
    Paths(Vec<std::path::PathBuf>),
    /// Directory roots whose tracked descendants may have changed.
    Roots(Vec<std::path::PathBuf>),
    /// A watcher overflow or error lost the affected path set.
    All,
}

pub use client::LspClient;
#[cfg(test)]
pub(crate) use lifecycle::fake_lsp_server;
pub use lifecycle::{LspServer, ServerInitConfig, ServerInitResult, ServerState};
pub(crate) use lifecycle::{apply_project_environment, load_project_environment, resolve_command};
pub use transport::LspTransport;
pub use types::{
    InboundMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, LspNotification,
    RequestId,
};
