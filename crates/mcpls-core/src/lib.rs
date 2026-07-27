//! # mcpls-core
//!
//! Core library for MCP (Model Context Protocol) to LSP (Language Server Protocol) translation.
//!
//! This crate provides the fundamental building blocks for bridging AI agents with
//! language servers, enabling semantic code intelligence through MCP tools.
//!
//! ## Architecture
//!
//! The library is organized into several modules:
//!
//! - [`lsp`] - LSP client implementation for communicating with language servers
//! - [`mcp`] - MCP tool definitions and handlers
//! - [`bridge`] - Translation layer between MCP and LSP protocols
//! - [`config`] - Configuration types and loading
//! - [`mod@error`] - Error types for the library
//!
//! ## Example
//!
//! ```rust,ignore
//! use mcpls_core::{serve, serve_with, Transport, ServerConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), mcpls_core::Error> {
//!     let config = ServerConfig::load()?;
//!     // Stdio (default):
//!     serve(config).await
//!     // HTTP (requires `transport-http` feature):
//!     // serve_with(config, Transport::Http(mcpls_core::HttpConfig {
//!     //     bind: "127.0.0.1:3000".parse().unwrap(),
//!     //     path: "/mcp".to_string(),
//!     // })).await
//! }
//! ```

pub mod bridge;
pub mod config;
pub mod edit_apply;
pub mod edit_backup;
pub mod edit_paths;
pub mod edit_plan;
pub mod edit_planner;
pub mod edit_policy;
pub mod edit_preview;
pub mod error;
pub mod lsp;
pub mod mcp;
pub mod project;
pub mod project_persistence;
pub mod transport;
pub mod workspace_edit;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bridge::{ResourceSubscriptions, TranslatorTemplate};
pub use config::{DaemonConfig, ServerConfig};
pub use error::Error;
use tokio::sync::OnceCell;
use tracing::{info, warn};
#[cfg(feature = "transport-http")]
pub use transport::HttpConfig;
pub use transport::Transport;
#[cfg(feature = "transport-http")]
use transport::run_http;
use transport::run_stdio;

const PROJECT_MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "CMakeLists.txt",
    "Gemfile",
    "Package.swift",
    "build.gradle",
    "composer.json",
    "dune-project",
    "flake.nix",
    "go.mod",
    "meson.build",
    "mix.exs",
    "package.json",
    "pom.xml",
    "pyproject.toml",
    "settings.gradle",
];

/// Resolve explicitly configured roots or detect the project containing the current directory.
///
/// Detection walks ancestors only. Git checkout roots take precedence over
/// manifest-only roots so nested package manifests do not split one checkout.
///
/// # Returns
///
/// Explicit roots are returned as-is. With no explicit roots, returns the
/// detected project root or an empty vector when the current directory is not
/// inside a recognizable project.
fn resolve_workspace_roots(config_roots: &[PathBuf]) -> Vec<PathBuf> {
    if !config_roots.is_empty() {
        return config_roots.to_vec();
    }

    let Ok(cwd) = std::env::current_dir() else {
        warn!("Failed to get current directory; no default workspace project will be registered");
        return Vec::new();
    };
    resolve_workspace_roots_from(config_roots, &cwd)
}

fn resolve_workspace_roots_from(config_roots: &[PathBuf], start: &Path) -> Vec<PathBuf> {
    if !config_roots.is_empty() {
        return config_roots.to_vec();
    }

    let Ok(start) = start.canonicalize() else {
        warn!(
            "Failed to canonicalize current directory {}; no default workspace project will be registered",
            start.display()
        );
        return Vec::new();
    };

    let root = start
        .ancestors()
        .find(|ancestor| ancestor.join(".git").try_exists().unwrap_or(false))
        .or_else(|| {
            start.ancestors().find(|ancestor| {
                PROJECT_MANIFESTS
                    .iter()
                    .any(|manifest| ancestor.join(manifest).is_file())
            })
        });

    root.map_or_else(
        || {
            info!(
                "No project root detected from {}; no default workspace project will be registered",
                start.display()
            );
            Vec::new()
        },
        |root| {
            info!("Detected workspace root: {}", root.display());
            vec![root.to_path_buf()]
        },
    )
}

async fn register_default_workspace_projects(
    registry: &project::ProjectRegistry,
    roots: &[PathBuf],
) -> usize {
    let mut registered = 0;
    for (index, root) in roots.iter().enumerate() {
        let Ok(canonical_root) = project::CanonicalRoot::new(root) else {
            warn!(
                "Skipping unavailable configured workspace root: {}",
                root.display()
            );
            continue;
        };
        let project_id = if roots.len() == 1 {
            "default".to_string()
        } else {
            format!("workspace-{index}")
        };
        let Ok(project_id) = project::ProjectId::new(project_id) else {
            continue;
        };
        let repository = project::GitRepositoryIdentity::discover(canonical_root.as_path())
            .ok()
            .flatten();
        let identity = repository.map_or_else(
            || project::ProjectIdentity::new(project_id.clone(), canonical_root.clone()),
            |repository| {
                project::ProjectIdentity::new(project_id.clone(), canonical_root.clone())
                    .with_repository_identity(repository)
            },
        );
        if let Err(error) = registry.add(identity).await {
            warn!(
                "Skipping default workspace project for {}: {error}",
                root.display()
            );
        } else {
            registered += 1;
        }
    }
    registered
}

/// Start the MCPLS server with the given configuration over stdio.
///
/// This is the backward-compatible entry point. It is equivalent to calling
/// `serve_with(config, Transport::Stdio)`.
///
/// # Errors
///
/// Returns an error if MCP server setup or the transport fails. Individual
/// project activation failures are reported through project status instead of
/// terminating the daemon.
pub async fn serve(config: ServerConfig) -> Result<(), Error> {
    serve_with(config, Transport::Stdio).await
}

/// Start the MCPLS server with an explicit transport.
///
/// Performs shared setup (workspace discovery, actor registry, and translator
/// configuration) and then delegates to the appropriate transport runner.
///
/// # Errors
///
/// Returns an error if the MCP server or transport fails to start.
///
/// # HTTP trust boundary
///
/// The built-in HTTP transport accepts loopback binds only. `rmcp` Host
/// validation protects against DNS rebinding but is not authentication; put an
/// authenticated reverse proxy in front of MCPLS for remote access.
///
/// # Examples
///
/// ```rust,ignore
/// use mcpls_core::{serve_with, Transport, ServerConfig};
///
/// #[tokio::main]
/// async fn main() -> Result<(), mcpls_core::Error> {
///     let config = ServerConfig::load()?;
///     serve_with(config, Transport::Stdio).await
/// }
/// ```
pub async fn serve_with(config: ServerConfig, transport: Transport) -> Result<(), Error> {
    info!("Starting MCPLS server...");

    let workspace_roots = resolve_workspace_roots(&config.workspace.roots);
    let translator_template = TranslatorTemplate::from_server_config(&config);
    let subscriptions = Arc::new(ResourceSubscriptions::new());
    // Peer cell is populated after the MCP transport is established (Phase B).
    let peer_cell = Arc::new(OnceCell::new());
    let project_registry =
        project::ProjectRegistry::with_translator_template(32, translator_template)
            .with_shutdown_timeout(config.daemon.shutdown_timeout());
    let project_registry = if let Some(path) = config.daemon.state_file.clone() {
        project_registry.with_persistence(project_persistence::ProjectRegistrationStore::new(path))
    } else {
        project_registry
    };
    project_registry
        .restore_from_persistence()
        .await
        .map_err(|error| Error::Config(format!("failed to restore project state: {error}")))?;
    let registered = register_default_workspace_projects(&project_registry, &workspace_roots).await;
    info!("Registered {registered} default workspace project(s)");

    info!("Starting MCP server with rmcp...");
    let transport_snapshot = transport::TransportSnapshot::from(&transport);
    let session_manager = transport::session_manager_for(&transport);
    let mcp_server = mcp::McplsServer::from_registry_with_transport(
        Arc::clone(&subscriptions),
        project_registry.clone(),
        transport_snapshot,
        session_manager.clone(),
    );
    info!("MCPLS server initialized successfully");

    let result = match transport {
        Transport::Stdio => {
            info!("Listening for MCP requests on stdio...");
            run_stdio(mcp_server, &peer_cell).await
        }
        #[cfg(feature = "transport-http")]
        Transport::Http(cfg) => run_http(mcp_server, cfg, session_manager).await,
    };

    let shutdown = project_registry.shutdown_all().await;
    if !shutdown.failed.is_empty() {
        warn!(
            failed_projects = ?shutdown.failed,
            "some project actors did not shut down cleanly"
        );
    }

    info!("MCPLS server shutting down");
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn registers_a_default_project_for_one_workspace_root() {
        let root = TempDir::new().unwrap();
        let registry = project::ProjectRegistry::new(2);

        let registered =
            register_default_workspace_projects(&registry, &[root.path().to_path_buf()]).await;

        assert_eq!(registered, 1);
        let projects = registry.list().await;
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].id().as_str(), "default");
        assert!(registry.actor_for_path(root.path()).await.is_ok());
    }

    #[test]
    fn detects_git_workspace_from_nested_directory() {
        let workspace = TempDir::new().unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        let nested = workspace.path().join("crates").join("core");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("Cargo.toml"), "[package]\nname = \"core\"\n").unwrap();

        let roots = resolve_workspace_roots_from(&[], &nested);

        assert_eq!(roots, [workspace.path().canonicalize().unwrap()]);
    }

    #[test]
    fn test_resolve_workspace_roots_with_config() {
        let config_roots = vec![PathBuf::from("/test/root")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
    }

    #[test]
    fn test_resolve_workspace_roots_multiple_paths() {
        let config_roots = vec![PathBuf::from("/test/root1"), PathBuf::from("/test/root2")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn test_resolve_workspace_roots_preserves_order() {
        let config_roots = vec![
            PathBuf::from("/workspace/alpha"),
            PathBuf::from("/workspace/beta"),
            PathBuf::from("/workspace/gamma"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots[0], PathBuf::from("/workspace/alpha"));
        assert_eq!(roots[1], PathBuf::from("/workspace/beta"));
        assert_eq!(roots[2], PathBuf::from("/workspace/gamma"));
    }

    #[test]
    fn test_resolve_workspace_roots_single_path() {
        let config_roots = vec![PathBuf::from("/single/workspace")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from("/single/workspace"));
    }

    #[test]
    fn detects_linked_worktree_from_nested_directory() {
        let worktree = TempDir::new().unwrap();
        std::fs::write(worktree.path().join(".git"), "gitdir: /tmp/example\n").unwrap();
        let nested = worktree.path().join("src");
        std::fs::create_dir(&nested).unwrap();

        let roots = resolve_workspace_roots_from(&[], &nested);

        assert_eq!(roots, [worktree.path().canonicalize().unwrap()]);
    }

    #[test]
    fn detects_manifest_workspace_without_git() {
        let workspace = TempDir::new().unwrap();
        std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        let nested = workspace.path().join("src");
        std::fs::create_dir(&nested).unwrap();

        let roots = resolve_workspace_roots_from(&[], &nested);

        assert_eq!(roots, [workspace.path().canonicalize().unwrap()]);
    }

    #[test]
    fn empty_config_without_project_marker_registers_nothing() {
        let directory = TempDir::new().unwrap();

        assert!(resolve_workspace_roots_from(&[], directory.path()).is_empty());
    }

    #[test]
    fn configured_roots_win_over_detected_workspace() {
        let workspace = TempDir::new().unwrap();
        std::fs::create_dir(workspace.path().join(".git")).unwrap();
        let configured = [PathBuf::from("/configured/root")];

        assert_eq!(
            resolve_workspace_roots_from(&configured, workspace.path()),
            configured
        );
    }

    #[test]
    fn test_resolve_workspace_roots_relative_paths() {
        let config_roots = vec![
            PathBuf::from("relative/path1"),
            PathBuf::from("relative/path2"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("relative/path1"));
        assert_eq!(roots[1], PathBuf::from("relative/path2"));
    }

    #[test]
    fn test_resolve_workspace_roots_mixed_paths() {
        let config_roots = vec![
            PathBuf::from("/absolute/path"),
            PathBuf::from("relative/path"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/absolute/path"));
        assert_eq!(roots[1], PathBuf::from("relative/path"));
    }

    #[test]
    fn translator_template_is_built_without_a_live_translator() {
        let config = ServerConfig::default();
        let template = bridge::TranslatorTemplate::from_server_config(&config);

        assert!(template.rust_server_config().is_some());
    }

    #[test]
    fn test_resolve_workspace_roots_with_dot_path() {
        let config_roots = vec![PathBuf::from(".")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots, config_roots);
    }

    #[test]
    fn test_resolve_workspace_roots_with_parent_path() {
        let config_roots = vec![PathBuf::from("..")];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], PathBuf::from(".."));
    }

    #[test]
    fn test_resolve_workspace_roots_unicode_paths() {
        let config_roots = vec![
            PathBuf::from("/workspace/テスト"),
            PathBuf::from("/workspace/тест"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/workspace/テスト"));
        assert_eq!(roots[1], PathBuf::from("/workspace/тест"));
    }

    #[test]
    fn test_resolve_workspace_roots_spaces_in_paths() {
        let config_roots = vec![
            PathBuf::from("/workspace/path with spaces"),
            PathBuf::from("/another path/workspace"),
        ];
        let roots = resolve_workspace_roots(&config_roots);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0], PathBuf::from("/workspace/path with spaces"));
    }

    // Tests for graceful degradation behavior
    mod graceful_degradation_tests {
        use super::*;
        use crate::error::ServerSpawnFailure;
        use crate::lsp::ServerInitResult;

        #[test]
        fn test_all_servers_failed_error_handling() {
            let mut result = ServerInitResult::new();
            result.add_failure(ServerSpawnFailure {
                language_id: "rust".to_string(),
                command: "rust-analyzer".to_string(),
                message: "not found".to_string(),
            });
            result.add_failure(ServerSpawnFailure {
                language_id: "python".to_string(),
                command: "pyright".to_string(),
                message: "not found".to_string(),
            });

            assert!(result.all_failed());
            assert_eq!(result.failure_count(), 2);
            assert_eq!(result.server_count(), 0);
        }

        #[test]
        fn test_partial_success_detection() {
            use std::collections::HashMap;

            let mut result = ServerInitResult::new();
            // Simulate one success and one failure
            result.servers = HashMap::new(); // Would have a real server in production
            result.add_failure(ServerSpawnFailure {
                language_id: "python".to_string(),
                command: "pyright".to_string(),
                message: "not found".to_string(),
            });

            // Without actual servers, we can verify the failure was recorded
            assert_eq!(result.failure_count(), 1);
            assert_eq!(result.server_count(), 0);
        }

        #[test]
        fn test_all_servers_succeeded_detection() {
            use std::collections::HashMap;

            let mut result = ServerInitResult::new();
            result.servers = HashMap::new(); // Would have real servers in production

            assert_eq!(result.failure_count(), 0);
            assert!(!result.all_failed());
            assert!(!result.partial_success());
        }

        #[test]
        fn test_all_servers_failed_to_init_error() {
            let failures = vec![
                ServerSpawnFailure {
                    language_id: "rust".to_string(),
                    command: "rust-analyzer".to_string(),
                    message: "command not found".to_string(),
                },
                ServerSpawnFailure {
                    language_id: "python".to_string(),
                    command: "pyright".to_string(),
                    message: "permission denied".to_string(),
                },
            ];

            let err = Error::AllServersFailedToInit { count: 2, failures };

            assert!(err.to_string().contains("all LSP servers failed"));
            assert!(err.to_string().contains("2 configured"));

            // Verify failures are preserved
            if let Error::AllServersFailedToInit { count, failures: f } = err {
                assert_eq!(count, 2);
                assert_eq!(f.len(), 2);
                assert_eq!(f[0].language_id, "rust");
                assert_eq!(f[1].language_id, "python");
            } else {
                panic!("Expected AllServersFailedToInit error");
            }
        }

        #[test]
        fn test_graceful_degradation_with_empty_config() {
            let result = ServerInitResult::new();

            // Empty config means no servers configured
            assert!(!result.all_failed());
            assert!(!result.partial_success());
            assert!(!result.has_servers());
            assert_eq!(result.server_count(), 0);
            assert_eq!(result.failure_count(), 0);
        }

        #[test]
        fn test_server_spawn_failure_display() {
            let failure = ServerSpawnFailure {
                language_id: "typescript".to_string(),
                command: "tsserver".to_string(),
                message: "executable not found in PATH".to_string(),
            };

            let display = failure.to_string();
            assert!(display.contains("typescript"));
            assert!(display.contains("tsserver"));
            assert!(display.contains("executable not found"));
        }

        #[test]
        fn test_result_helpers_consistency() {
            let mut result = ServerInitResult::new();

            // Initially empty
            assert!(!result.has_servers());
            assert!(!result.all_failed());
            assert!(!result.partial_success());

            // Add a failure
            result.add_failure(ServerSpawnFailure {
                language_id: "go".to_string(),
                command: "gopls".to_string(),
                message: "error".to_string(),
            });

            assert!(result.all_failed());
            assert!(!result.has_servers());
            assert!(!result.partial_success());
        }

        #[tokio::test]
        async fn test_serve_degrades_when_all_servers_fail_to_spawn() {
            use crate::config::{LspServerConfig, WorkspaceConfig};

            // A configured server whose command cannot spawn used to make serve()
            // fail synchronously with NoServersAvailable / AllServersFailedToInit.
            // LSP initialization now runs in a background task so the MCP
            // `initialize` handshake is never blocked, which means the spawn
            // failure is handled in the background instead: serve() starts the MCP
            // server in degraded mode (mirroring `test_serve_starts_with_empty_config`)
            // rather than failing fast. Any error it surfaces must therefore be a
            // transport/MCP error from the closed test connection, NOT a fail-fast
            // server-availability error.
            let config = ServerConfig {
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                },
                lsp_servers: vec![LspServerConfig {
                    language_id: "rust".to_string(),
                    command: "nonexistent-command-that-will-fail-12345".to_string(),
                    args: vec![],
                    env: std::collections::HashMap::new(),
                    file_patterns: vec!["**/*.rs".to_string()],
                    initialization_options: None,
                    timeout_seconds: 10,
                    heuristics: None,
                }],
                daemon: crate::config::DaemonConfig::default(),
            };

            // serve() proceeds to run the MCP server and blocks on the stdio
            // transport until EOF; bound it so the test can't hang if stdin stays
            // open (e.g. under multi-threaded `cargo test`, where several serve()
            // tests share the process stdin).
            let outcome =
                tokio::time::timeout(std::time::Duration::from_secs(2), serve(config)).await;

            match outcome {
                // Still serving after the deadline => it did not fail fast. Good.
                Err(_elapsed) => {}
                // Transport closed cleanly. Also fine.
                Ok(Ok(())) => {}
                // It returned an error: it must not be a fail-fast availability error.
                Ok(Err(err)) => assert!(
                    !matches!(err, Error::NoServersAvailable(_))
                        && !matches!(err, Error::AllServersFailedToInit { .. }),
                    "serve() must not fail fast now that LSP init is backgrounded; got: {err:?}"
                ),
            }
        }

        #[tokio::test]
        async fn test_serve_starts_with_empty_config() {
            use crate::config::WorkspaceConfig;

            // Server starts in protocol-only mode when no LSP servers are configured.
            // serve() blocks until the MCP transport closes, so it will error with a
            // connection/transport error — not NoServersAvailable.
            let config = ServerConfig {
                workspace: WorkspaceConfig {
                    roots: vec![PathBuf::from("/tmp/test-workspace")],
                    position_encodings: vec!["utf-8".to_string(), "utf-16".to_string()],
                    language_extensions: vec![],
                    heuristics_max_depth: 10,
                },
                lsp_servers: vec![],
                daemon: crate::config::DaemonConfig::default(),
            };

            let result = serve(config).await;

            // serve() may succeed or fail with a transport error, but must NOT
            // return NoServersAvailable when the config simply has no servers.
            if let Err(ref err) = result {
                assert!(
                    !matches!(err, Error::NoServersAvailable(_)),
                    "serve() must not return NoServersAvailable for empty lsp_servers config"
                );
            }
        }
    }
}
