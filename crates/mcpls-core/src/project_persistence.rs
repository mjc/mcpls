//! Versioned, atomic persistence for dynamic project registrations.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::ProjectConfig;
use crate::project::{ProjectId, ProjectIdentity};

/// Current on-disk schema for the dynamic project registration store.
pub const SCHEMA_VERSION: u32 = 1;

/// A project registration represented without requiring its root to exist.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistedProject {
    /// Stable project identifier.
    pub project_id: String,
    /// Canonical root at the time the project was registered.
    pub root: PathBuf,
    /// Additional linked worktree roots owned by the logical project.
    #[serde(default)]
    pub additional_roots: Vec<PathBuf>,
    /// Optional project-specific translator override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<ProjectConfig>,
}

impl PersistedProject {
    /// Capture the durable fields of a live project identity.
    #[must_use]
    pub fn from_identity(identity: &ProjectIdentity) -> Self {
        Self {
            project_id: identity.id().as_str().to_string(),
            root: identity.root().as_path().to_path_buf(),
            additional_roots: identity
                .roots()
                .iter()
                .skip(1)
                .map(|root| root.as_path().to_path_buf())
                .collect(),
            config: None,
        }
    }

    /// Capture a durable registration and its project-specific override.
    #[must_use]
    pub fn from_identity_with_config(
        identity: &ProjectIdentity,
        config: Option<ProjectConfig>,
    ) -> Self {
        let mut persisted = Self::from_identity(identity);
        persisted.config = config.map(|config| config.for_persistence());
        persisted
    }

    /// Return the representation safe to write to the registration store.
    #[must_use]
    fn for_persistence(&self) -> Self {
        let mut persisted = self.clone();
        persisted.config = persisted
            .config
            .as_ref()
            .map(ProjectConfig::for_persistence);
        persisted
    }

    /// Parse the stable ID without canonicalizing the possibly moved root.
    ///
    /// # Errors
    ///
    /// Returns the identity validation error for an empty ID.
    pub fn project_id(&self) -> Result<ProjectId, crate::project::ProjectIdentityError> {
        ProjectId::new(self.project_id.clone())
    }
}

/// Versioned state written to the registration store.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectRegistrationState {
    /// On-disk schema version.
    pub schema_version: u32,
    /// Persisted dynamic registrations.
    pub projects: Vec<PersistedProject>,
}

impl ProjectRegistrationState {
    /// Construct an empty state using the current schema.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            projects: Vec::new(),
        }
    }
}

/// Errors returned by the registration store.
#[derive(Debug, thiserror::Error)]
pub enum ProjectPersistenceError {
    /// The state file could not be read or atomically replaced.
    #[error("project state I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The state file was not valid JSON.
    #[error("project state JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    /// The state file was written by a newer daemon.
    #[error("unsupported project state schema {found}; current schema is {SCHEMA_VERSION}")]
    UnsupportedSchema {
        /// Schema version found on disk.
        found: u32,
    },
}

/// Atomic JSON store for dynamic project registrations.
#[derive(Debug, Clone)]
pub struct ProjectRegistrationStore {
    path: PathBuf,
}

impl ProjectRegistrationStore {
    /// Use a specific state-file path.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Return the configured state-file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load registrations, treating a missing state file as empty state.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, unsupported schema, or I/O other
    /// than a missing file.
    pub fn load(&self) -> Result<ProjectRegistrationState, ProjectPersistenceError> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ProjectRegistrationState::empty());
            }
            Err(error) => return Err(error.into()),
        };
        let state: ProjectRegistrationState = serde_json::from_str(&contents)?;
        if state.schema_version > SCHEMA_VERSION {
            return Err(ProjectPersistenceError::UnsupportedSchema {
                found: state.schema_version,
            });
        }
        Ok(state)
    }

    /// Atomically replace the state file with the supplied registrations.
    ///
    /// The temporary file is created beside the primary file, flushed and
    /// synced before rename, so a crash cannot leave a partial primary file.
    ///
    /// # Errors
    ///
    /// Returns an I/O or serialization error when the replacement fails.
    pub fn save(&self, projects: &[PersistedProject]) -> Result<(), ProjectPersistenceError> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let state = ProjectRegistrationState {
            schema_version: SCHEMA_VERSION,
            projects: projects
                .iter()
                .map(PersistedProject::for_persistence)
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&state)?;
        let temporary = temporary_path(&self.path);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        if let Err(error) = write_and_sync(&mut file, &bytes) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        drop(file);
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
        sync_parent(self.path.parent())?;
        Ok(())
    }
}

fn write_and_sync(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

fn temporary_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    path.with_file_name(format!(".{file_name}.{nanos}.tmp"))
}

fn sync_parent(parent: Option<&Path>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(parent) = parent {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tempfile::TempDir;

    use crate::config::{LspServerConfig, ProjectConfig};

    use super::*;

    #[test]
    fn missing_store_loads_as_empty_current_schema() {
        let directory = TempDir::new().unwrap();
        let state = ProjectRegistrationStore::new(directory.path().join("projects.json"))
            .load()
            .unwrap();
        assert_eq!(state, ProjectRegistrationState::empty());
    }

    #[test]
    fn save_round_trips_versioned_project_registrations() {
        let directory = TempDir::new().unwrap();
        let store = ProjectRegistrationStore::new(directory.path().join("nested/projects.json"));
        let projects = vec![PersistedProject {
            project_id: "demo".to_string(),
            root: PathBuf::from("/workspace/demo"),
            additional_roots: Vec::new(),
            config: None,
        }];

        store.save(&projects).unwrap();

        assert_eq!(store.load().unwrap().projects, projects);
    }

    #[test]
    fn newer_schema_is_rejected_without_destroying_state() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("projects.json");
        fs::write(
            &path,
            serde_json::to_vec(&ProjectRegistrationState {
                schema_version: SCHEMA_VERSION + 1,
                projects: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();

        assert!(matches!(
            ProjectRegistrationStore::new(path).load(),
            Err(ProjectPersistenceError::UnsupportedSchema { found })
                if found == SCHEMA_VERSION + 1
        ));
    }

    #[test]
    fn corrupt_or_truncated_store_is_rejected() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("projects.json");
        fs::write(&path, br#"{"schema_version":1,"projects":["#).unwrap();

        assert!(matches!(
            ProjectRegistrationStore::new(path).load(),
            Err(ProjectPersistenceError::Json(_))
        ));
    }

    #[test]
    fn project_lsp_environment_is_not_persisted_without_explicit_opt_in() {
        let directory = TempDir::new().unwrap();
        let store = ProjectRegistrationStore::new(directory.path().join("projects.json"));
        let mut server = LspServerConfig::rust_analyzer();
        server
            .env
            .insert("MCPLS_TEST_SECRET".to_string(), "do-not-write".to_string());
        let project = PersistedProject {
            project_id: "demo".to_string(),
            root: PathBuf::from("/workspace/demo"),
            additional_roots: Vec::new(),
            config: Some(ProjectConfig {
                lsp_servers: Some(vec![server]),
                ..ProjectConfig::default()
            }),
        };

        store.save(&[project]).unwrap();

        let contents = fs::read_to_string(store.path()).unwrap();
        assert!(!contents.contains("do-not-write"));
        let persisted = store.load().unwrap();
        let env = &persisted.projects[0]
            .config
            .as_ref()
            .unwrap()
            .lsp_servers
            .as_ref()
            .unwrap()[0]
            .env;
        assert!(env.is_empty());
    }

    #[test]
    fn project_lsp_environment_persists_with_explicit_opt_in() {
        let directory = TempDir::new().unwrap();
        let store = ProjectRegistrationStore::new(directory.path().join("projects.json"));
        let mut server = LspServerConfig::rust_analyzer();
        server.env.insert(
            "MCPLS_TEST_SAFE_VALUE".to_string(),
            "safe-to-write".to_string(),
        );
        let project = PersistedProject {
            project_id: "demo".to_string(),
            root: PathBuf::from("/workspace/demo"),
            additional_roots: Vec::new(),
            config: Some(ProjectConfig {
                lsp_servers: Some(vec![server]),
                persist_environment: true,
                ..ProjectConfig::default()
            }),
        };

        store.save(&[project]).unwrap();

        let contents = fs::read_to_string(store.path()).unwrap();
        assert!(contents.contains("safe-to-write"));
        let persisted = store.load().unwrap();
        let env = &persisted.projects[0]
            .config
            .as_ref()
            .unwrap()
            .lsp_servers
            .as_ref()
            .unwrap()[0]
            .env;
        assert_eq!(
            env.get("MCPLS_TEST_SAFE_VALUE"),
            Some(&"safe-to-write".to_string())
        );
    }
}
