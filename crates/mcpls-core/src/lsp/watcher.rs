use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::debug;
use url::Url;

use super::types::JsonRpcError;

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_REGISTRATIONS: usize = 64;
const MAX_WATCHERS_PER_REGISTRATION: usize = 128;
const WATCH_CREATE: u8 = 1;
const WATCH_CHANGE: u8 = 2;
const WATCH_DELETE: u8 = 4;

#[derive(Debug, Deserialize)]
struct RegistrationParams {
    registrations: Vec<Registration>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Registration {
    id: String,
    method: String,
    register_options: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct UnregistrationParams {
    #[serde(alias = "unregistrations")]
    unregisterations: Vec<Unregistration>,
}

#[derive(Debug, Deserialize)]
struct Unregistration {
    id: String,
    method: String,
}

#[derive(Debug, Deserialize)]
struct WatchedFilesOptions {
    watchers: Vec<RawWatcher>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWatcher {
    glob_pattern: Value,
    #[serde(default = "all_watch_kinds")]
    kind: u8,
}

const fn all_watch_kinds() -> u8 {
    WATCH_CREATE | WATCH_CHANGE | WATCH_DELETE
}

#[derive(Debug)]
struct WatchSpec {
    root: PathBuf,
    matcher: WatchMatcher,
    kind: u8,
}

#[derive(Debug)]
enum WatchMatcher {
    Glob(Gitignore),
    Exact { path: PathBuf, recursive: bool },
}

impl WatchSpec {
    fn glob(root: PathBuf, pattern: &str, kind: u8) -> Result<Self, JsonRpcError> {
        let mut builder = GitignoreBuilder::new(&root);
        builder
            .add_line(None, pattern)
            .map_err(|error| invalid_params(format!("invalid watched-file glob: {error}")))?;
        let matcher = builder
            .build()
            .map_err(|error| invalid_params(format!("invalid watched-file glob: {error}")))?;
        Ok(Self {
            root,
            matcher: WatchMatcher::Glob(matcher),
            kind,
        })
    }

    fn exact(path: PathBuf, kind: u8) -> Result<Self, JsonRpcError> {
        let recursive = path.is_dir() || path.extension().is_none();
        let root = if path.is_dir() {
            path.clone()
        } else {
            path.parent()
                .ok_or_else(|| invalid_params("absolute watched path has no parent"))?
                .to_path_buf()
        };
        Ok(Self {
            root,
            matcher: WatchMatcher::Exact { path, recursive },
            kind,
        })
    }

    fn matches(&self, path: &Path, is_dir: bool) -> bool {
        match &self.matcher {
            WatchMatcher::Glob(matcher) => {
                !is_dir
                    && path.strip_prefix(&self.root).is_ok_and(|relative| {
                        matcher
                            .matched_path_or_any_parents(relative, false)
                            .is_ignore()
                    })
            }
            WatchMatcher::Exact {
                path: exact,
                recursive,
            } => path == exact || (*recursive && path.starts_with(exact)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Fingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WatchedEntry {
    fingerprint: Fingerprint,
    kind: u8,
}

#[derive(Debug)]
struct WatchTask {
    stop_tx: std_mpsc::Sender<()>,
    _thread: thread::JoinHandle<()>,
}

impl Drop for WatchTask {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
    }
}

#[derive(Debug)]
struct RegisteredWatch {
    generation: u64,
    _task: WatchTask,
}

#[derive(Debug)]
pub(super) struct WatchedFileEvent {
    registration_id: String,
    generation: u64,
    pub(super) params: Value,
}

#[derive(Debug)]
pub(super) struct WatchRegistry {
    roots: Vec<PathBuf>,
    event_tx: mpsc::Sender<WatchedFileEvent>,
    registrations: HashMap<String, RegisteredWatch>,
    next_generation: u64,
}

impl WatchRegistry {
    pub(super) fn new(roots: Vec<PathBuf>, event_tx: mpsc::Sender<WatchedFileEvent>) -> Self {
        Self {
            roots,
            event_tx,
            registrations: HashMap::new(),
            next_generation: 1,
        }
    }

    pub(super) fn register(&mut self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        let params: RegistrationParams = serde_json::from_value(
            params
                .cloned()
                .ok_or_else(|| invalid_params("missing registration parameters"))?,
        )
        .map_err(|error| invalid_params(format!("invalid registration parameters: {error}")))?;

        let mut planned = Vec::new();
        for registration in params
            .registrations
            .into_iter()
            .filter(|registration| registration.method == "workspace/didChangeWatchedFiles")
        {
            let options: WatchedFilesOptions = serde_json::from_value(
                registration
                    .register_options
                    .ok_or_else(|| invalid_params("watched-file registration has no options"))?,
            )
            .map_err(|error| invalid_params(format!("invalid watched-file options: {error}")))?;
            if options.watchers.len() > MAX_WATCHERS_PER_REGISTRATION {
                return Err(invalid_params(format!(
                    "watched-file registration exceeds {MAX_WATCHERS_PER_REGISTRATION} watchers"
                )));
            }
            let specs = watch_specs(&self.roots, options.watchers)?;
            planned.push((registration.id, specs));
        }
        let new_registrations = planned
            .iter()
            .filter(|(id, _)| !self.registrations.contains_key(id))
            .count();
        if self.registrations.len() + new_registrations > MAX_REGISTRATIONS {
            return Err(invalid_params(format!(
                "watched-file registrations exceed {MAX_REGISTRATIONS}"
            )));
        }

        let mut started = Vec::with_capacity(planned.len());
        for (id, specs) in planned {
            let generation = self.next_generation;
            self.next_generation = self.next_generation.wrapping_add(1);
            let task = spawn_watch_task(id.clone(), generation, specs, self.event_tx.clone())?;
            started.push((
                id,
                RegisteredWatch {
                    generation,
                    _task: task,
                },
            ));
        }
        for (id, registration) in started {
            self.registrations.insert(id, registration);
        }

        Ok(Value::Null)
    }

    pub(super) fn unregister(&mut self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        let params: UnregistrationParams = serde_json::from_value(
            params
                .cloned()
                .ok_or_else(|| invalid_params("missing unregistration parameters"))?,
        )
        .map_err(|error| invalid_params(format!("invalid unregistration parameters: {error}")))?;

        for unregistration in params
            .unregisterations
            .into_iter()
            .filter(|unregistration| unregistration.method == "workspace/didChangeWatchedFiles")
        {
            self.registrations.remove(&unregistration.id);
        }
        Ok(Value::Null)
    }

    pub(super) fn accepts(&self, event: &WatchedFileEvent) -> bool {
        self.registrations
            .get(&event.registration_id)
            .is_some_and(|registration| registration.generation == event.generation)
    }
}

fn watch_specs(
    workspace_roots: &[PathBuf],
    watchers: Vec<RawWatcher>,
) -> Result<Vec<WatchSpec>, JsonRpcError> {
    let mut specs = Vec::new();
    for watcher in watchers {
        if watcher.kind & all_watch_kinds() == 0 {
            continue;
        }
        match watcher.glob_pattern {
            Value::String(pattern) => {
                let path = Path::new(&pattern);
                if path.is_absolute() && !has_glob_meta(&pattern) {
                    specs.push(WatchSpec::exact(path.to_path_buf(), watcher.kind)?);
                } else if path.is_absolute() {
                    let Some((root, relative)) = workspace_roots
                        .iter()
                        .filter_map(|root| {
                            path.strip_prefix(root)
                                .ok()
                                .map(|relative| (root, relative))
                        })
                        .max_by_key(|(root, _)| root.components().count())
                    else {
                        return Err(invalid_params(
                            "absolute watched-file glob is outside the workspace",
                        ));
                    };
                    specs.push(WatchSpec::glob(
                        root.clone(),
                        &relative.to_string_lossy(),
                        watcher.kind,
                    )?);
                } else {
                    for root in workspace_roots {
                        specs.push(WatchSpec::glob(root.clone(), &pattern, watcher.kind)?);
                    }
                }
            }
            Value::Object(pattern) => {
                let glob = pattern
                    .get("pattern")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_params("relative watched-file glob has no pattern"))?;
                let base_uri = pattern
                    .get("baseUri")
                    .and_then(relative_base_uri)
                    .ok_or_else(|| invalid_params("relative watched-file glob has no base URI"))?;
                let root = Url::parse(base_uri)
                    .ok()
                    .and_then(|url| url.to_file_path().ok())
                    .ok_or_else(|| {
                        invalid_params("relative watched-file base URI is not a file")
                    })?;
                let canonical_root = root.canonicalize().unwrap_or_else(|_| root.clone());
                if !workspace_roots.iter().any(|workspace_root| {
                    let canonical_workspace = workspace_root
                        .canonicalize()
                        .unwrap_or_else(|_| workspace_root.clone());
                    canonical_root.starts_with(canonical_workspace)
                }) {
                    return Err(invalid_params(
                        "relative watched-file base URI is outside the workspace",
                    ));
                }
                specs.push(WatchSpec::glob(root, glob, watcher.kind)?);
            }
            _ => {
                return Err(invalid_params(
                    "watched-file glob must be a string or object",
                ));
            }
        }
    }
    Ok(specs)
}

fn has_glob_meta(pattern: &str) -> bool {
    pattern
        .bytes()
        .any(|byte| matches!(byte, b'*' | b'?' | b'[' | b'{'))
}

fn relative_base_uri(value: &Value) -> Option<&str> {
    value
        .as_str()
        .or_else(|| value.as_object()?.get("uri")?.as_str())
}

fn spawn_watch_task(
    registration_id: String,
    generation: u64,
    specs: Vec<WatchSpec>,
    event_tx: mpsc::Sender<WatchedFileEvent>,
) -> Result<WatchTask, JsonRpcError> {
    let (stop_tx, stop_rx) = std_mpsc::channel();
    let watcher_thread = thread::Builder::new()
        .name(format!("mcpls-watch-{registration_id}"))
        .spawn(move || {
            watch_loop(&registration_id, generation, &specs, &event_tx, &stop_rx);
        })
        .map_err(|error| internal_error(format!("failed to spawn watched-file poller: {error}")))?;
    Ok(WatchTask {
        stop_tx,
        _thread: watcher_thread,
    })
}

fn watch_loop(
    registration_id: &str,
    generation: u64,
    specs: &[WatchSpec],
    event_tx: &mpsc::Sender<WatchedFileEvent>,
    stop_rx: &std_mpsc::Receiver<()>,
) {
    let mut previous = scan(specs);
    debug!(
        registration_id,
        files = previous.len(),
        "watched-file poller started"
    );
    loop {
        match stop_rx.recv_timeout(WATCH_POLL_INTERVAL) {
            Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
        }
        let current = scan(specs);
        let changes = diff(&previous, &current);
        if !changes.is_empty()
            && event_tx
                .blocking_send(WatchedFileEvent {
                    registration_id: registration_id.to_string(),
                    generation,
                    params: json!({ "changes": changes }),
                })
                .is_err()
        {
            break;
        }
        previous = current;
    }
    debug!(registration_id, "watched-file poller stopped");
}

fn scan(specs: &[WatchSpec]) -> HashMap<PathBuf, WatchedEntry> {
    let mut entries = HashMap::new();
    let mut grouped: HashMap<&Path, Vec<&WatchSpec>> = HashMap::new();
    for spec in specs {
        if matches!(
            &spec.matcher,
            WatchMatcher::Exact {
                recursive: false,
                ..
            }
        ) {
            if let WatchMatcher::Exact { path, .. } = &spec.matcher
                && let Ok(metadata) = path.metadata()
            {
                entries.insert(
                    path.clone(),
                    WatchedEntry {
                        fingerprint: Fingerprint {
                            len: metadata.len(),
                            modified: metadata.modified().ok(),
                        },
                        kind: spec.kind,
                    },
                );
            }
        } else {
            grouped.entry(&spec.root).or_default().push(spec);
        }
    }
    for (root, root_specs) in grouped {
        let mut builder = WalkBuilder::new(root);
        builder
            .follow_links(false)
            .hidden(false)
            .parents(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .filter_entry(|entry| entry.file_name() != ".git");
        for entry in builder.build().filter_map(Result::ok) {
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let matching_kind = root_specs
                .iter()
                .filter(|spec| spec.matches(entry.path(), file_type.is_dir()))
                .fold(0, |kind, spec| kind | spec.kind);
            if matching_kind == 0 {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let watched = entries
                .entry(entry.into_path())
                .or_insert_with(|| WatchedEntry {
                    fingerprint: Fingerprint {
                        len: metadata.len(),
                        modified: metadata.modified().ok(),
                    },
                    kind: 0,
                });
            watched.kind |= matching_kind;
        }
    }
    entries
}

fn diff(
    previous: &HashMap<PathBuf, WatchedEntry>,
    current: &HashMap<PathBuf, WatchedEntry>,
) -> Vec<Value> {
    let mut changes = Vec::new();
    for (path, entry) in current {
        match previous.get(path) {
            None if entry.kind & WATCH_CREATE != 0 => push_change(&mut changes, path, 1),
            Some(old) if old.fingerprint != entry.fingerprint && entry.kind & WATCH_CHANGE != 0 => {
                push_change(&mut changes, path, 2);
            }
            _ => {}
        }
    }
    for (path, entry) in previous {
        if !current.contains_key(path) && entry.kind & WATCH_DELETE != 0 {
            push_change(&mut changes, path, 3);
        }
    }
    changes.sort_by(|left, right| left["uri"].as_str().cmp(&right["uri"].as_str()));
    changes
}

fn push_change(changes: &mut Vec<Value>, path: &Path, kind: u8) {
    if let Ok(uri) = Url::from_file_path(path) {
        changes.push(json!({ "uri": uri.as_str(), "type": kind }));
    }
}

fn invalid_params(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: -32602,
        message: message.into(),
        data: None,
    }
}

fn internal_error(message: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: -32603,
        message: message.into(),
        data: None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use tokio::time::timeout;

    use super::*;

    fn rust_spec(root: &Path) -> WatchSpec {
        WatchSpec::glob(root.to_path_buf(), "**/*.rs", all_watch_kinds()).unwrap()
    }

    #[test]
    fn scan_respects_gitignore_and_symlink_boundaries() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::create_dir(temp.path().join("target")).unwrap();
        fs::write(temp.path().join(".gitignore"), "target/\n").unwrap();
        fs::write(temp.path().join("src/lib.rs"), "pub fn kept() {}\n").unwrap();
        fs::write(temp.path().join("target/generated.rs"), "fn ignored() {}\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(temp.path().join("src"), temp.path().join("linked")).unwrap();

        let snapshot = scan(&[rust_spec(temp.path())]);

        assert!(snapshot.contains_key(&temp.path().join("src/lib.rs")));
        assert!(!snapshot.contains_key(&temp.path().join("target/generated.rs")));
        assert!(!snapshot.contains_key(&temp.path().join("linked/lib.rs")));
    }

    #[test]
    fn diff_coalesces_create_change_and_delete() {
        let temp = TempDir::new().unwrap();
        let created = temp.path().join("created.rs");
        let changed = temp.path().join("changed.rs");
        let deleted = temp.path().join("deleted.rs");
        let old = WatchedEntry {
            fingerprint: Fingerprint {
                len: 1,
                modified: None,
            },
            kind: all_watch_kinds(),
        };
        let new = WatchedEntry {
            fingerprint: Fingerprint {
                len: 2,
                modified: None,
            },
            kind: all_watch_kinds(),
        };
        let previous = HashMap::from([(changed.clone(), old), (deleted.clone(), old)]);
        let current = HashMap::from([(created.clone(), new), (changed.clone(), new)]);

        let events = diff(&previous, &current);

        assert_eq!(events.len(), 3);
        assert!(events.contains(&json!({
            "uri": Url::from_file_path(created).unwrap().as_str(),
            "type": 1
        })));
        assert!(events.contains(&json!({
            "uri": Url::from_file_path(changed).unwrap().as_str(),
            "type": 2
        })));
        assert!(events.contains(&json!({
            "uri": Url::from_file_path(deleted).unwrap().as_str(),
            "type": 3
        })));
    }

    #[test]
    fn registration_supports_workspace_and_relative_patterns() {
        let temp = TempDir::new().unwrap();
        let watchers = vec![
            RawWatcher {
                glob_pattern: json!("**/*.rs"),
                kind: all_watch_kinds(),
            },
            RawWatcher {
                glob_pattern: json!({
                    "baseUri": Url::from_directory_path(temp.path()).unwrap().as_str(),
                    "pattern": "**/Cargo.{lock,toml}"
                }),
                kind: WATCH_CHANGE,
            },
        ];

        let specs = watch_specs(&[temp.path().to_path_buf()], watchers).unwrap();

        assert_eq!(specs.len(), 2);
        assert!(specs[0].matches(&temp.path().join("src/lib.rs"), false));
        assert!(specs[1].matches(&temp.path().join("Cargo.toml"), false));
        assert!(specs[1].matches(&temp.path().join("Cargo.lock"), false));
    }

    #[test]
    fn absolute_file_and_directory_patterns_are_scanned() {
        let temp = TempDir::new().unwrap();
        let manifest = temp.path().join("Cargo.toml");
        let config_dir = temp.path().join("rust-analyzer");
        let config = config_dir.join("rust-analyzer.toml");
        fs::create_dir(&config_dir).unwrap();
        fs::write(&manifest, "[package]\n").unwrap();
        fs::write(&config, "[cargo]\n").unwrap();
        let watchers = vec![
            RawWatcher {
                glob_pattern: json!(manifest.to_string_lossy()),
                kind: WATCH_CHANGE,
            },
            RawWatcher {
                glob_pattern: json!(config_dir.to_string_lossy()),
                kind: WATCH_CHANGE,
            },
        ];

        let specs = watch_specs(&[temp.path().to_path_buf()], watchers).unwrap();
        let snapshot = scan(&specs);

        assert!(snapshot.contains_key(&manifest));
        assert!(snapshot.contains_key(&config_dir));
        assert!(snapshot.contains_key(&config));
    }

    #[tokio::test]
    async fn registration_emits_events_and_unregistration_stops_delivery() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], event_tx);
        let registration = json!({
            "registrations": [{
                "id": "rust-files",
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": {
                    "watchers": [{ "globPattern": "**/*.rs", "kind": 7 }]
                }
            }]
        });

        registry.register(Some(&registration)).unwrap();
        tokio::time::sleep(WATCH_POLL_INTERVAL * 2).await;
        let created = temp.path().join("src/created.rs");
        fs::write(&created, "pub fn created() {}\n").unwrap();

        let event = timeout(WATCH_POLL_INTERVAL * 4, event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(registry.accepts(&event));
        assert_eq!(
            event.params,
            json!({
                "changes": [{
                    "uri": Url::from_file_path(&created).unwrap().as_str(),
                    "type": 1
                }]
            })
        );

        registry
            .unregister(Some(&json!({
                "unregisterations": [{
                    "id": "rust-files",
                    "method": "workspace/didChangeWatchedFiles"
                }]
            })))
            .unwrap();
        fs::write(temp.path().join("src/after-stop.rs"), "fn stopped() {}\n").unwrap();
        tokio::time::sleep(WATCH_POLL_INTERVAL * 2).await;
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn relative_pattern_must_stay_inside_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let result = watch_specs(
            &[workspace.path().to_path_buf()],
            vec![RawWatcher {
                glob_pattern: json!({
                    "baseUri": Url::from_directory_path(outside.path()).unwrap().as_str(),
                    "pattern": "**/*.rs"
                }),
                kind: all_watch_kinds(),
            }],
        );

        assert!(result.is_err());
    }

    #[test]
    fn absolute_glob_requires_a_workspace_path_component_prefix() {
        let temp = TempDir::new().unwrap();
        let workspace = temp.path().join("repo");
        fs::create_dir(&workspace).unwrap();
        let lookalike = temp.path().join("repo-other/**/*.rs");

        let result = watch_specs(
            &[workspace],
            vec![RawWatcher {
                glob_pattern: json!(lookalike.to_string_lossy()),
                kind: all_watch_kinds(),
            }],
        );

        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn relative_pattern_rejects_a_symlink_escape() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let linked = workspace.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();

        let result = watch_specs(
            &[workspace.path().to_path_buf()],
            vec![RawWatcher {
                glob_pattern: json!({
                    "baseUri": Url::from_directory_path(linked).unwrap().as_str(),
                    "pattern": "**/*.rs"
                }),
                kind: all_watch_kinds(),
            }],
        );

        assert!(result.is_err());
    }
}
