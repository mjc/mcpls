use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use ignore::Match;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{ModifyKind, RenameMode};
use notify::{ErrorKind, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use url::Url;

use super::types::JsonRpcError;

const MAX_REGISTRATIONS: usize = 64;
const MAX_WATCHERS_PER_REGISTRATION: usize = 128;
const MAX_WATCHED_DIRECTORIES: usize = 16_384;
const MAX_CHANGES_PER_NOTIFICATION: usize = 512;
pub(super) const WATCH_EVENT_CHANNEL_CAPACITY: usize = 256;
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

#[derive(Clone, Debug)]
struct WatchSpec {
    root: PathBuf,
    matcher: WatchMatcher,
    kind: u8,
}

#[derive(Clone, Debug)]
enum WatchMatcher {
    Glob(Gitignore),
    Exact(PathBuf),
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
        let root = path
            .parent()
            .ok_or_else(|| invalid_params("absolute watched path has no parent"))?
            .to_path_buf();
        Ok(Self {
            root,
            matcher: WatchMatcher::Exact(path),
            kind,
        })
    }

    fn matches(&self, path: &Path, is_dir: bool) -> bool {
        match &self.matcher {
            WatchMatcher::Glob(matcher) => path.strip_prefix(&self.root).is_ok_and(|relative| {
                matcher
                    .matched_path_or_any_parents(relative, is_dir)
                    .is_ignore()
            }),
            WatchMatcher::Exact(exact) => path == exact,
        }
    }
}

#[derive(Clone, Debug)]
struct RegisteredWatch {
    generation: u64,
    specs: Vec<WatchSpec>,
    known_paths: HashMap<PathBuf, bool>,
}

#[derive(Debug)]
pub(super) struct WatchedFileEvent {
    registration_id: String,
    generation: u64,
    pub(super) params: Value,
}

#[derive(Debug)]
pub(super) enum WatchSignal {
    Event(Event),
    Rescan,
    Error(String),
}

pub(super) struct WatchRegistry {
    roots: Vec<PathBuf>,
    registrations: HashMap<String, RegisteredWatch>,
    next_generation: u64,
    watcher: RecommendedWatcher,
    watched_directories: HashSet<PathBuf>,
    overflowed: Arc<AtomicBool>,
    pending_error: Arc<Mutex<Option<String>>>,
}

impl std::fmt::Debug for WatchRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WatchRegistry")
            .field("roots", &self.roots)
            .field("registrations", &self.registrations)
            .field("next_generation", &self.next_generation)
            .field("watched_directories", &self.watched_directories)
            .finish_non_exhaustive()
    }
}

impl WatchRegistry {
    pub(super) fn new(
        roots: Vec<PathBuf>,
        signal_tx: mpsc::Sender<WatchSignal>,
    ) -> Result<Self, JsonRpcError> {
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = Arc::clone(&overflowed);
        let pending_error = Arc::new(Mutex::new(None));
        let callback_error = Arc::clone(&pending_error);
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let signal = match result {
                Ok(event) if event.paths.len() > MAX_CHANGES_PER_NOTIFICATION => {
                    callback_overflowed.store(true, Ordering::Release);
                    WatchSignal::Rescan
                }
                Ok(event) => WatchSignal::Event(event),
                Err(error) => {
                    let message = error.to_string();
                    *callback_error
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(message.clone());
                    WatchSignal::Error(message)
                }
            };
            if signal_tx.try_send(signal).is_err() {
                callback_overflowed.store(true, Ordering::Release);
            }
        })
        .map_err(|error| {
            internal_error(format!("failed to create watched-file runtime: {error}"))
        })?;
        Ok(Self {
            roots,
            registrations: HashMap::new(),
            next_generation: 1,
            watcher,
            watched_directories: HashSet::new(),
            overflowed,
            pending_error,
        })
    }

    pub(super) fn register(&mut self, params: Option<&Value>) -> Result<Value, JsonRpcError> {
        let params: RegistrationParams = serde_json::from_value(
            params
                .cloned()
                .ok_or_else(|| invalid_params("missing registration parameters"))?,
        )
        .map_err(|error| invalid_params(format!("invalid registration parameters: {error}")))?;

        let mut planned = Vec::new();
        let mut ids = HashSet::new();
        for registration in params.registrations {
            if registration.method != "workspace/didChangeWatchedFiles" {
                return Err(invalid_params(format!(
                    "unsupported dynamic registration method: {}",
                    registration.method
                )));
            }
            if !ids.insert(registration.id.clone()) {
                return Err(invalid_params(format!(
                    "duplicate watched-file registration id: {}",
                    registration.id
                )));
            }
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

        let mut prepared = Vec::with_capacity(planned.len());
        for (id, specs) in planned {
            let generation = self.next_generation;
            self.next_generation = self.next_generation.wrapping_add(1);
            let known_paths = snapshot(&specs)?;
            prepared.push((
                id,
                RegisteredWatch {
                    generation,
                    specs,
                    known_paths,
                },
            ));
        }
        let mut replaced = Vec::with_capacity(prepared.len());
        for (id, registration) in prepared {
            let previous = self.registrations.insert(id.clone(), registration);
            replaced.push((id, previous));
        }
        if let Err(error) = self.refresh_watches() {
            for (id, previous) in replaced.into_iter().rev() {
                match previous {
                    Some(registration) => {
                        self.registrations.insert(id, registration);
                    }
                    None => {
                        self.registrations.remove(&id);
                    }
                }
            }
            let _ = self.refresh_watches();
            return Err(error);
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

        for unregistration in &params.unregisterations {
            if unregistration.method != "workspace/didChangeWatchedFiles" {
                return Err(invalid_params(format!(
                    "unsupported dynamic unregistration method: {}",
                    unregistration.method
                )));
            }
        }
        let mut removed = Vec::new();
        for unregistration in params.unregisterations {
            if let Some(registration) = self.registrations.remove(&unregistration.id) {
                removed.push((unregistration.id, registration));
            }
        }
        if let Err(error) = self.refresh_watches() {
            self.registrations.extend(removed);
            let _ = self.refresh_watches();
            return Err(error);
        }
        Ok(Value::Null)
    }

    pub(super) fn accepts(&self, event: &WatchedFileEvent) -> bool {
        self.registrations
            .get(&event.registration_id)
            .is_some_and(|registration| registration.generation == event.generation)
    }

    pub(super) fn handle_signal(
        &mut self,
        signal: WatchSignal,
    ) -> Result<Vec<WatchedFileEvent>, JsonRpcError> {
        let pending_error = self
            .pending_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(error) = pending_error {
            return Err(internal_error(format!(
                "watched-file runtime failed: {error}"
            )));
        }
        let event = match signal {
            WatchSignal::Error(error) => {
                return Err(internal_error(format!(
                    "watched-file runtime failed: {error}"
                )));
            }
            WatchSignal::Rescan => {
                self.overflowed.store(false, Ordering::Release);
                return self.rescan();
            }
            WatchSignal::Event(event) => event,
        };
        if self.overflowed.swap(false, Ordering::AcqRel) || event.need_rescan() {
            return self.rescan();
        }

        let raw_changes = event_changes(&event);
        if raw_changes
            .iter()
            .any(|(path, _)| path.is_dir() || self.watched_directories.contains(path))
        {
            return self.rescan();
        }
        self.apply_changes(&raw_changes)
    }

    fn apply_changes(
        &mut self,
        raw_changes: &[(PathBuf, u8)],
    ) -> Result<Vec<WatchedFileEvent>, JsonRpcError> {
        let mut events = Vec::new();
        for (registration_id, registration) in &mut self.registrations {
            let mut changes = Vec::new();
            for (path, change_type) in raw_changes {
                let was_known = registration.known_paths.get(path).copied();
                let metadata = path.symlink_metadata().ok();
                let is_dir = metadata.as_ref().is_some_and(std::fs::Metadata::is_dir)
                    || was_known.unwrap_or(false)
                    || self.watched_directories.contains(path);
                let matches = registration.specs.iter().any(|spec| {
                    spec.matches(path, is_dir) && spec.kind & watch_bit(*change_type) != 0
                });
                match *change_type {
                    1 | 2 if matches && visible_path(path, is_dir, &registration.specs)? => {
                        registration.known_paths.insert(path.clone(), is_dir);
                        push_change(&mut changes, path, *change_type);
                    }
                    3 if was_known.is_some() && matches => {
                        registration.known_paths.remove(path);
                        push_change(&mut changes, path, 3);
                    }
                    _ => {}
                }
            }
            events.extend(watched_events(
                registration_id,
                registration.generation,
                changes,
            ));
        }
        Ok(events)
    }

    fn rescan(&mut self) -> Result<Vec<WatchedFileEvent>, JsonRpcError> {
        self.refresh_watches()?;
        let mut events = Vec::new();
        for (registration_id, registration) in &mut self.registrations {
            let current = snapshot(&registration.specs)?;
            let changes = set_diff(&registration.known_paths, &current, &registration.specs);
            registration.known_paths = current;
            events.extend(watched_events(
                registration_id,
                registration.generation,
                changes,
            ));
        }
        Ok(events)
    }

    fn refresh_watches(&mut self) -> Result<(), JsonRpcError> {
        let desired = desired_watch_directories(&self.roots, &self.registrations)?;
        if desired.len() > MAX_WATCHED_DIRECTORIES {
            return Err(internal_error(format!(
                "watched-file runtime requires {} directories, exceeding the limit of {MAX_WATCHED_DIRECTORIES}",
                desired.len()
            )));
        }
        let added = desired
            .difference(&self.watched_directories)
            .cloned()
            .collect::<Vec<_>>();
        let removed = self
            .watched_directories
            .difference(&desired)
            .cloned()
            .collect::<Vec<_>>();

        let mut installed: Vec<PathBuf> = Vec::new();
        for directory in &added {
            if let Err(error) = self.watcher.watch(directory, RecursiveMode::NonRecursive) {
                for installed_directory in installed {
                    let _ = self.watcher.unwatch(&installed_directory);
                }
                return Err(internal_error(format!(
                    "failed to watch directory {}: {error}",
                    directory.display()
                )));
            }
            installed.push(directory.clone());
        }

        let mut uninstalled: Vec<PathBuf> = Vec::new();
        for directory in &removed {
            if let Err(error) = self.watcher.unwatch(directory)
                && directory.exists()
                && !matches!(
                    error.kind,
                    ErrorKind::WatchNotFound | ErrorKind::PathNotFound
                )
            {
                for uninstalled_directory in uninstalled {
                    let _ = self
                        .watcher
                        .watch(&uninstalled_directory, RecursiveMode::NonRecursive);
                }
                for installed_directory in installed {
                    let _ = self.watcher.unwatch(&installed_directory);
                }
                return Err(internal_error(format!(
                    "failed to stop watching directory {}: {error}",
                    directory.display()
                )));
            }
            uninstalled.push(directory.clone());
        }
        self.watched_directories = desired;
        Ok(())
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

fn snapshot(specs: &[WatchSpec]) -> Result<HashMap<PathBuf, bool>, JsonRpcError> {
    let mut entries = HashMap::new();
    let mut grouped: HashMap<&Path, Vec<&WatchSpec>> = HashMap::new();
    for spec in specs {
        if let WatchMatcher::Exact(path) = &spec.matcher {
            if let Ok(metadata) = path.symlink_metadata() {
                entries.insert(path.clone(), metadata.is_dir());
            }
        } else {
            grouped.entry(&spec.root).or_default().push(spec);
        }
    }
    for (root, root_specs) in grouped {
        if !root.exists() {
            continue;
        }
        let builder = configured_walk(root);
        for entry in builder.build() {
            let entry = entry.map_err(|error| {
                internal_error(format!(
                    "failed to scan watched root {}: {error}",
                    root.display()
                ))
            })?;
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
            entries.insert(entry.into_path(), file_type.is_dir());
        }
    }
    Ok(entries)
}

fn set_diff(
    previous: &HashMap<PathBuf, bool>,
    current: &HashMap<PathBuf, bool>,
    specs: &[WatchSpec],
) -> Vec<Value> {
    let mut changes = Vec::new();
    for (path, is_dir) in current {
        if previous.contains_key(path) {
            continue;
        }
        if specs
            .iter()
            .any(|spec| spec.matches(path, *is_dir) && spec.kind & WATCH_CREATE != 0)
        {
            push_change(&mut changes, path, 1);
        }
    }
    for (path, is_dir) in previous {
        if current.contains_key(path) {
            continue;
        }
        if specs
            .iter()
            .any(|spec| spec.matches(path, *is_dir) && spec.kind & WATCH_DELETE != 0)
        {
            push_change(&mut changes, path, 3);
        }
    }
    changes.sort_by(|left, right| left["uri"].as_str().cmp(&right["uri"].as_str()));
    changes
}

fn desired_watch_directories(
    roots: &[PathBuf],
    registrations: &HashMap<String, RegisteredWatch>,
) -> Result<HashSet<PathBuf>, JsonRpcError> {
    let mut directories = HashSet::new();
    if registrations.is_empty() {
        return Ok(directories);
    }
    for root in roots {
        if let Some(parent) = root.parent() {
            directories.insert(parent.to_path_buf());
        }
    }
    for registration in registrations.values() {
        for spec in &registration.specs {
            match &spec.matcher {
                WatchMatcher::Exact(path) => {
                    if let Some(parent) = nearest_existing_directory(path.parent()) {
                        directories.insert(parent);
                    }
                }
                WatchMatcher::Glob(_) => {
                    if !spec.root.exists() {
                        continue;
                    }
                    let builder = configured_walk(&spec.root);
                    for entry in builder.build() {
                        let entry = entry.map_err(|error| {
                            internal_error(format!(
                                "failed to enumerate watched directories under {}: {error}",
                                spec.root.display()
                            ))
                        })?;
                        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                            directories.insert(entry.into_path());
                        }
                    }
                }
            }
        }
    }
    Ok(directories)
}

fn configured_walk(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(true)
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);
    builder.filter_entry(|entry| entry.file_name() != ".git");
    builder
}

fn nearest_existing_directory(start: Option<&Path>) -> Option<PathBuf> {
    let mut path = start?;
    loop {
        if path.is_dir() {
            return Some(path.to_path_buf());
        }
        path = path.parent()?;
    }
}

fn visible_path(path: &Path, is_dir: bool, specs: &[WatchSpec]) -> Result<bool, JsonRpcError> {
    if specs
        .iter()
        .any(|spec| matches!(&spec.matcher, WatchMatcher::Exact(exact) if exact == path))
    {
        return Ok(true);
    }
    for spec in specs {
        if !matches!(spec.matcher, WatchMatcher::Glob(_)) || !path.starts_with(&spec.root) {
            continue;
        }
        if !path.exists() {
            if missing_path_is_visible(&spec.root, path, is_dir)? {
                return Ok(true);
            }
            continue;
        }
        let target = path.to_path_buf();
        let mut builder = WalkBuilder::new(&spec.root);
        builder
            .follow_links(false)
            .hidden(true)
            .parents(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .filter_entry(move |entry| {
                entry.file_name() != ".git" && target.starts_with(entry.path())
            });
        for entry in builder.build() {
            let entry = entry.map_err(|error| {
                internal_error(format!(
                    "failed to check watched path {}: {error}",
                    path.display()
                ))
            })?;
            if entry.path() == path {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn missing_path_is_visible(root: &Path, path: &Path, is_dir: bool) -> Result<bool, JsonRpcError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| internal_error("watched path escaped its root"))?;
    if relative.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.starts_with('.'))
    }) {
        return Ok(false);
    }

    let parent = path.parent().unwrap_or(root);
    let mut directories = vec![root.to_path_buf()];
    let mut directory = root.to_path_buf();
    if let Ok(relative_parent) = parent.strip_prefix(root) {
        for component in relative_parent.components() {
            directory.push(component);
            if directory
                .symlink_metadata()
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                return Ok(false);
            }
            directories.push(directory.clone());
        }
    }

    let mut ignored = false;
    let exclude = root.join(".git/info/exclude");
    if exclude.is_file() {
        ignored = apply_ignore_file(root, &exclude, path, is_dir, ignored)?;
    }
    for directory in directories {
        let ignore = directory.join(".gitignore");
        if ignore.is_file() {
            ignored = apply_ignore_file(&directory, &ignore, path, is_dir, ignored)?;
        }
    }
    Ok(!ignored)
}

fn apply_ignore_file(
    root: &Path,
    ignore_file: &Path,
    path: &Path,
    is_dir: bool,
    ignored: bool,
) -> Result<bool, JsonRpcError> {
    let mut builder = GitignoreBuilder::new(root);
    if let Some(error) = builder.add(ignore_file) {
        return Err(internal_error(format!(
            "failed to read ignore file {}: {error}",
            ignore_file.display()
        )));
    }
    let matcher = builder.build().map_err(|error| {
        internal_error(format!(
            "failed to parse ignore file {}: {error}",
            ignore_file.display()
        ))
    })?;
    Ok(match matcher.matched_path_or_any_parents(path, is_dir) {
        Match::Ignore(_) => true,
        Match::Whitelist(_) => false,
        Match::None => ignored,
    })
}

fn event_changes(event: &Event) -> Vec<(PathBuf, u8)> {
    match &event.kind {
        EventKind::Create(_) => event.paths.iter().cloned().map(|path| (path, 1)).collect(),
        EventKind::Remove(_) => event.paths.iter().cloned().map(|path| (path, 3)).collect(),
        EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if event.paths.len() >= 2 => {
            vec![(event.paths[0].clone(), 3), (event.paths[1].clone(), 1)]
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
            event.paths.iter().cloned().map(|path| (path, 3)).collect()
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            event.paths.iter().cloned().map(|path| (path, 1)).collect()
        }
        EventKind::Modify(_) => event.paths.iter().cloned().map(|path| (path, 2)).collect(),
        _ => Vec::new(),
    }
}

const fn watch_bit(change_type: u8) -> u8 {
    match change_type {
        1 => WATCH_CREATE,
        2 => WATCH_CHANGE,
        3 => WATCH_DELETE,
        _ => 0,
    }
}

fn watched_events(
    registration_id: &str,
    generation: u64,
    mut changes: Vec<Value>,
) -> Vec<WatchedFileEvent> {
    changes.sort_by(|left, right| left["uri"].as_str().cmp(&right["uri"].as_str()));
    changes
        .chunks(MAX_CHANGES_PER_NOTIFICATION)
        .map(|chunk| WatchedFileEvent {
            registration_id: registration_id.to_string(),
            generation,
            params: json!({ "changes": chunk }),
        })
        .collect()
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
    use std::time::Duration;

    use tempfile::TempDir;
    use tokio::time::timeout;

    use super::*;

    fn rust_spec(root: &Path) -> WatchSpec {
        WatchSpec::glob(root.to_path_buf(), "**/*.rs", all_watch_kinds()).unwrap()
    }

    fn registration(id: &str, pattern: &Value, kind: u8) -> Value {
        json!({
            "registrations": [{
                "id": id,
                "method": "workspace/didChangeWatchedFiles",
                "registerOptions": {
                    "watchers": [{ "globPattern": pattern, "kind": kind }]
                }
            }]
        })
    }

    fn changes(events: Vec<WatchedFileEvent>) -> Vec<Value> {
        events
            .into_iter()
            .flat_map(|event| {
                event.params["changes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    fn other_event() -> WatchSignal {
        WatchSignal::Event(Event::new(EventKind::Other))
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

        let snapshot = snapshot(&[rust_spec(temp.path())]).unwrap();

        assert!(snapshot.contains_key(&temp.path().join("src/lib.rs")));
        assert!(!snapshot.contains_key(&temp.path().join("target/generated.rs")));
        assert!(!snapshot.contains_key(&temp.path().join("linked/lib.rs")));
    }

    #[test]
    fn scan_respects_nested_gitignore_excludes_hidden_files_and_exact_overrides() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".git/info")).unwrap();
        fs::create_dir_all(temp.path().join("nested")).unwrap();
        fs::create_dir_all(temp.path().join("target")).unwrap();
        fs::write(temp.path().join(".git/info/exclude"), "excluded.rs\n").unwrap();
        fs::write(temp.path().join("nested/.gitignore"), "ignored.rs\n").unwrap();
        fs::write(temp.path().join("nested/kept.rs"), "").unwrap();
        fs::write(temp.path().join("nested/ignored.rs"), "").unwrap();
        fs::write(temp.path().join("excluded.rs"), "").unwrap();
        fs::write(temp.path().join(".hidden.rs"), "").unwrap();
        fs::write(temp.path().join(".gitignore"), "target/\n").unwrap();
        let manifest = temp.path().join("target/Cargo.toml");
        fs::write(&manifest, "[package]\n").unwrap();
        fs::write(temp.path().join("target/generated.rs"), "").unwrap();

        let snapshot = snapshot(&[
            rust_spec(temp.path()),
            WatchSpec::exact(manifest.clone(), WATCH_CHANGE).unwrap(),
        ])
        .unwrap();

        assert!(snapshot.contains_key(&temp.path().join("nested/kept.rs")));
        assert!(snapshot.contains_key(&manifest));
        assert!(!snapshot.contains_key(&temp.path().join("nested/ignored.rs")));
        assert!(!snapshot.contains_key(&temp.path().join("excluded.rs")));
        assert!(!snapshot.contains_key(&temp.path().join(".hidden.rs")));
        assert!(!snapshot.contains_key(&temp.path().join("target/generated.rs")));
    }

    #[test]
    fn ignored_paths_are_not_visible_to_native_events() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        fs::write(temp.path().join(".gitignore"), "ignored.rs\n").unwrap();
        let ignored = temp.path().join("ignored.rs");
        fs::write(&ignored, "").unwrap();

        assert!(!visible_path(&ignored, false, &[rust_spec(temp.path())]).unwrap());
        fs::remove_file(&ignored).unwrap();
        assert!(!missing_path_is_visible(temp.path(), &ignored, false).unwrap());
        assert!(
            missing_path_is_visible(temp.path(), &temp.path().join("visible.rs"), false).unwrap()
        );
    }

    #[test]
    fn event_mapping_covers_create_change_delete_and_rename() {
        let temp = TempDir::new().unwrap();
        let created = temp.path().join("created.rs");
        let changed = temp.path().join("changed.rs");
        let deleted = temp.path().join("deleted.rs");
        let renamed = temp.path().join("renamed.rs");

        assert_eq!(
            event_changes(
                &Event::new(EventKind::Create(notify::event::CreateKind::Any))
                    .add_path(created.clone())
            ),
            vec![(created, 1)]
        );
        assert_eq!(
            event_changes(
                &Event::new(EventKind::Modify(ModifyKind::Data(
                    notify::event::DataChange::Any
                )))
                .add_path(changed.clone())
            ),
            vec![(changed, 2)]
        );
        assert_eq!(
            event_changes(
                &Event::new(EventKind::Remove(notify::event::RemoveKind::Any))
                    .add_path(deleted.clone())
            ),
            vec![(deleted.clone(), 3)]
        );
        assert_eq!(
            event_changes(
                &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                    .add_path(deleted.clone())
                    .add_path(renamed.clone())
            ),
            vec![(deleted, 3), (renamed, 1)]
        );
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
    fn glob_patterns_match_directories() {
        let temp = TempDir::new().unwrap();
        let spec = WatchSpec::glob(temp.path().to_path_buf(), "**", all_watch_kinds()).unwrap();

        assert!(spec.matches(&temp.path().join("created-directory"), true));
    }

    #[test]
    fn absolute_patterns_match_only_the_requested_path() {
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
        let snapshot = snapshot(&specs).unwrap();

        assert!(snapshot.contains_key(&manifest));
        assert!(snapshot.contains_key(&config_dir));
        assert!(!snapshot.contains_key(&config));
    }

    #[test]
    fn registration_accepts_rust_analyzer_user_config_outside_workspace() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let config_dir = outside.path().join("rust-analyzer");
        fs::create_dir(&config_dir).unwrap();

        let specs = watch_specs(
            &[workspace.path().to_path_buf()],
            vec![
                RawWatcher {
                    glob_pattern: json!("**/*.rs"),
                    kind: all_watch_kinds(),
                },
                RawWatcher {
                    glob_pattern: json!(config_dir.to_string_lossy()),
                    kind: all_watch_kinds(),
                },
            ],
        )
        .unwrap();

        assert_eq!(specs.len(), 2);
        assert!(specs[0].matches(&workspace.path().join("src/lib.rs"), false));
        assert!(specs[1].matches(&config_dir, true));
    }

    #[test]
    fn registrations_share_directory_watches_and_unregister_cleans_up() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        let (signal_tx, _signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();

        registry
            .register(Some(&registration(
                "first",
                &json!("**/*.rs"),
                all_watch_kinds(),
            )))
            .unwrap();
        let first_directories = registry.watched_directories.clone();
        registry
            .register(Some(&registration(
                "second",
                &json!("**/*.rs"),
                all_watch_kinds(),
            )))
            .unwrap();

        assert_eq!(registry.watched_directories, first_directories);
        registry
            .unregister(Some(&json!({
                "unregisterations": [
                    {
                        "id": "first",
                        "method": "workspace/didChangeWatchedFiles"
                    },
                    {
                        "id": "second",
                        "method": "workspace/didChangeWatchedFiles"
                    }
                ]
            })))
            .unwrap();
        assert!(registry.watched_directories.is_empty());
    }

    #[test]
    fn watch_kind_masks_filter_each_change_type() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let existing = temp.path().join("existing.rs");
        fs::write(&existing, "a").unwrap();
        let (signal_tx, _signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();
        registry
            .register(Some(&registration(
                "change-only",
                &json!("**/*.rs"),
                WATCH_CHANGE,
            )))
            .unwrap();

        let created = temp.path().join("created.rs");
        fs::write(&created, "a").unwrap();
        assert!(registry.apply_changes(&[(created, 1)]).unwrap().is_empty());
        assert_eq!(
            changes(registry.apply_changes(&[(existing.clone(), 2)]).unwrap())[0]["type"],
            2
        );
        fs::remove_file(&existing).unwrap();
        assert!(registry.apply_changes(&[(existing, 3)]).unwrap().is_empty());
    }

    #[test]
    fn transient_create_delete_is_not_lost_after_the_path_disappears() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let (signal_tx, _signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();
        registry
            .register(Some(&registration(
                "rust-files",
                &json!("**/*.rs"),
                all_watch_kinds(),
            )))
            .unwrap();
        let transient = temp.path().join("transient.rs");

        let changes = changes(
            registry
                .apply_changes(&[(transient.clone(), 1), (transient, 3)])
                .unwrap(),
        );

        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0]["type"], 1);
        assert_eq!(changes[1]["type"], 3);
    }

    #[test]
    fn overflow_rescans_and_notifications_have_bounded_payloads() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let (signal_tx, _signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();
        registry
            .register(Some(&registration(
                "rust-files",
                &json!("**/*.rs"),
                all_watch_kinds(),
            )))
            .unwrap();
        let created = temp.path().join("created.rs");
        fs::write(&created, "").unwrap();
        registry.overflowed.store(true, Ordering::Release);

        let rescanned = changes(registry.handle_signal(other_event()).unwrap());
        assert_eq!(rescanned.len(), 1);
        assert_eq!(
            rescanned[0]["uri"],
            Url::from_file_path(created).unwrap().as_str()
        );

        let payload = (0..=MAX_CHANGES_PER_NOTIFICATION)
            .map(|index| json!({ "uri": format!("file:///tmp/{index}"), "type": 1 }))
            .collect();
        let events = watched_events("bounded", 1, payload);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].params["changes"].as_array().unwrap().len(),
            MAX_CHANGES_PER_NOTIFICATION
        );
        assert_eq!(events[1].params["changes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn deleted_and_recreated_roots_rescan_without_failing() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("repo");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("before.rs"), "").unwrap();
        let (signal_tx, _signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![root.clone()], signal_tx).unwrap();
        registry
            .register(Some(&registration("all", &json!("**"), all_watch_kinds())))
            .unwrap();

        fs::remove_dir_all(&root).unwrap();
        registry.overflowed.store(true, Ordering::Release);
        let deleted = changes(registry.handle_signal(other_event()).unwrap());
        assert!(deleted.iter().any(|change| change["type"] == 3));

        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("after.rs"), "").unwrap();
        registry.overflowed.store(true, Ordering::Release);
        let created = changes(registry.handle_signal(other_event()).unwrap());
        assert!(created.iter().any(|change| change["type"] == 1));
    }

    #[test]
    fn watcher_failures_are_explicit_even_when_the_signal_is_lost() {
        let temp = TempDir::new().unwrap();
        let (signal_tx, _signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();
        *registry
            .pending_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some("inotify failed".to_string());

        let error = registry.handle_signal(other_event()).unwrap_err();

        assert!(error.message.contains("inotify failed"));
    }

    #[test]
    fn unsupported_or_duplicate_dynamic_registrations_are_rejected() {
        let temp = TempDir::new().unwrap();
        let (signal_tx, _signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();

        assert!(
            registry
                .register(Some(&json!({
                    "registrations": [{
                        "id": "other",
                        "method": "workspace/other",
                        "registerOptions": {}
                    }]
                })))
                .is_err()
        );
        assert!(
            registry
                .register(Some(&json!({
                    "registrations": [
                        {
                            "id": "duplicate",
                            "method": "workspace/didChangeWatchedFiles",
                            "registerOptions": { "watchers": [] }
                        },
                        {
                            "id": "duplicate",
                            "method": "workspace/didChangeWatchedFiles",
                            "registerOptions": { "watchers": [] }
                        }
                    ]
                })))
                .is_err()
        );
    }

    #[tokio::test]
    async fn registration_emits_events_and_unregistration_stops_delivery() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        let (signal_tx, mut signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();
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
        let created = temp.path().join("src/created.rs");
        fs::write(&created, "pub fn created() {}\n").unwrap();

        let expected_uri = Url::from_file_path(&created).unwrap().to_string();
        timeout(Duration::from_secs(5), async {
            loop {
                let signal = signal_rx.recv().await.unwrap();
                let events = registry.handle_signal(signal).unwrap();
                if events.iter().any(|event| {
                    event.params["changes"].as_array().is_some_and(|changes| {
                        changes
                            .iter()
                            .any(|change| change["uri"] == expected_uri && change["type"] == 1)
                    })
                }) {
                    break;
                }
            }
        })
        .await
        .unwrap();

        fs::write(&created, "pub fn changed() {}\n").unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                let signal = signal_rx.recv().await.unwrap();
                let events = registry.handle_signal(signal).unwrap();
                if events.iter().any(|event| {
                    event.params["changes"].as_array().is_some_and(|changes| {
                        changes
                            .iter()
                            .any(|change| change["uri"] == expected_uri && change["type"] == 2)
                    })
                }) {
                    break;
                }
            }
        })
        .await
        .unwrap();

        registry
            .unregister(Some(&json!({
                "unregisterations": [{
                    "id": "rust-files",
                    "method": "workspace/didChangeWatchedFiles"
                }]
            })))
            .unwrap();
        while signal_rx.try_recv().is_ok() {}
        fs::write(temp.path().join("src/after-stop.rs"), "fn stopped() {}\n").unwrap();
        assert!(
            timeout(Duration::from_millis(500), signal_rx.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dropping_registry_stops_the_native_watcher() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let (signal_tx, mut signal_rx) = mpsc::channel(WATCH_EVENT_CHANNEL_CAPACITY);
        let mut registry = WatchRegistry::new(vec![temp.path().to_path_buf()], signal_tx).unwrap();
        registry
            .register(Some(&registration(
                "rust-files",
                &json!("**/*.rs"),
                all_watch_kinds(),
            )))
            .unwrap();
        while signal_rx.try_recv().is_ok() {}

        drop(registry);
        fs::write(temp.path().join("after-drop.rs"), "").unwrap();

        assert!(
            timeout(Duration::from_secs(1), async {
                while signal_rx.recv().await.is_some() {}
            })
            .await
            .is_ok()
        );
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
