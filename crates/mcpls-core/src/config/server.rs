//! LSP server configuration types.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use super::routing::{ServerId, ToolKind};

/// Default max depth for recursive marker search.
pub const DEFAULT_HEURISTICS_MAX_DEPTH: usize = 10;

/// Directories excluded from recursive marker search.
/// These are well-known directories that should never contain project markers.
const EXCLUDED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "build",
    "dist",
    ".cargo",
    ".rustup",
    "vendor",
    "coverage",
    ".next",
    ".nuxt",
    "_build",
    "deps",
    "third_party",
    "generated",
    "fixtures",
];

/// Heuristics for determining if an LSP server should be spawned.
///
/// Used to prevent spawning servers in projects where they are not applicable
/// (e.g., rust-analyzer in a Python-only project).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerHeuristics {
    /// Files or directories that indicate this server is applicable.
    ///
    /// The server will spawn if ANY of these markers exist anywhere in the workspace tree
    /// (searched recursively up to `heuristics_max_depth`). Well-known directories like
    /// `node_modules`, `target`, `.git` are excluded from the search.
    ///
    /// If empty, the server will always attempt to spawn.
    #[serde(default)]
    pub project_markers: Vec<String>,

    /// Source globs that make a profile applicable when no project marker is
    /// present. Patterns are relative to the workspace root.
    #[serde(default)]
    pub source_patterns: Vec<String>,

    /// Additional directory names excluded from recursive source detection.
    #[serde(default)]
    pub excluded_directories: Vec<String>,

    /// Content-level exclusions for ambiguous source extensions.
    #[serde(default)]
    pub content_exclusions: Vec<SourceContentExclusion>,
}

impl ServerHeuristics {
    /// Create heuristics with the given project markers.
    #[must_use]
    pub fn with_markers<I, S>(markers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            project_markers: markers.into_iter().map(Into::into).collect(),
            source_patterns: Vec::new(),
            excluded_directories: Vec::new(),
            content_exclusions: Vec::new(),
        }
    }

    /// Add source globs to this heuristic set.
    #[must_use]
    pub fn with_source_patterns<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.source_patterns = patterns.into_iter().map(Into::into).collect();
        self
    }

    /// Add profile-specific directory exclusions.
    #[must_use]
    pub fn with_excluded_directories<I, S>(mut self, directories: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.excluded_directories = directories.into_iter().map(Into::into).collect();
        self
    }

    /// Add content predicates that exclude ambiguous source files.
    #[must_use]
    pub fn with_content_exclusions(
        mut self,
        exclusions: impl IntoIterator<Item = SourceContentExclusion>,
    ) -> Self {
        self.content_exclusions = exclusions.into_iter().collect();
        self
    }

    /// Check if any configured marker or source predicate matches the workspace.
    ///
    /// Returns `true` if:
    /// - No markers or source patterns are defined (empty = always applicable)
    /// - At least one marker exists, or a source file matches the configured
    ///   predicates
    #[must_use]
    pub fn is_applicable(&self, workspace_root: &Path) -> bool {
        if self.project_markers.is_empty() && self.source_patterns.is_empty() {
            return true;
        }
        !self.matching_roots(workspace_root, None).is_empty()
    }

    /// Check if any marker or source predicate matches anywhere in the workspace tree.
    ///
    /// Recursively searches the workspace for project markers, excluding
    /// well-known directories like `node_modules`, `target`, `.git`, etc.
    ///
    /// # Arguments
    ///
    /// * `workspace_root` - Root directory to search from
    /// * `max_depth` - Maximum recursion depth (default: 10)
    ///
    /// # Returns
    ///
    /// `true` if any marker is found, `false` otherwise.
    #[must_use]
    pub fn is_applicable_recursive(&self, workspace_root: &Path, max_depth: Option<usize>) -> bool {
        !self.matching_roots(workspace_root, max_depth).is_empty()
    }

    /// Return workspace roots containing a configured marker.
    ///
    /// A container workspace may contain a nested language project without
    /// having a marker of its own. In that case, return the nested marker's
    /// parent so the language server receives a real project root.
    #[must_use]
    pub(crate) fn matching_roots(
        &self,
        workspace_root: &Path,
        max_depth: Option<usize>,
    ) -> Vec<PathBuf> {
        let marker_at_root = self
            .project_markers
            .iter()
            .any(|marker| workspace_root.join(marker).exists());
        if self.project_markers.is_empty() && self.source_patterns.is_empty() {
            return vec![workspace_root.to_path_buf()];
        }
        if self.source_patterns.is_empty() && marker_at_root {
            return vec![workspace_root.to_path_buf()];
        }

        let excluded_directories = self.excluded_directories.clone();
        let mut builder = WalkBuilder::new(workspace_root);
        builder
            .max_depth(max_depth.or(Some(DEFAULT_HEURISTICS_MAX_DEPTH)))
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .follow_links(false)
            .standard_filters(false)
            .filter_entry(move |entry| {
                if entry.file_type().is_some_and(|ft| ft.is_dir())
                    && let Some(name) = entry.file_name().to_str()
                    && (EXCLUDED_DIRECTORIES.contains(&name)
                        || excluded_directories.iter().any(|dir| dir == name))
                {
                    return false;
                }
                true
            });

        let source_patterns = source_glob_set(&self.source_patterns);

        let mut roots = builder
            .build()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                let file_name = path.file_name()?.to_str()?;
                let marker_match = self.content_exclusions.is_empty()
                    && self
                        .project_markers
                        .iter()
                        .any(|marker| marker == file_name);
                let source_match = source_patterns.as_ref().is_some_and(|patterns| {
                    path.strip_prefix(workspace_root)
                        .ok()
                        .is_some_and(|relative| {
                            patterns.is_match(relative)
                                && !content_excluded(path, &self.content_exclusions)
                        })
                });
                (marker_match || source_match)
                    .then_some(path)
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            })
            .collect::<Vec<_>>();
        roots.sort();
        roots.dedup();
        roots
    }
}

/// Content-level guards used by profiles with ambiguous file extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceContentExclusion {
    /// Qt Linguist translation XML stored in a `.ts` file.
    QtTranslationXml,
}

fn content_excluded(path: &Path, exclusions: &[SourceContentExclusion]) -> bool {
    exclusions.iter().any(|exclusion| match exclusion {
        SourceContentExclusion::QtTranslationXml => {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ts"))
                && is_qt_translation_xml(path)
        }
    })
}

fn is_qt_translation_xml(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut sample = String::new();
    file.take(8 * 1024).read_to_string(&mut sample).is_ok()
        && sample.trim_start().starts_with("<?xml")
        && sample.contains("<TS")
}

fn source_glob_set(patterns: &[String]) -> Option<GlobSet> {
    if patterns.is_empty() {
        return None;
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .ok()?;
        builder.add(glob);
    }
    builder.build().ok()
}

/// Stability of a built-in language-server candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinProfileStability {
    /// Candidate is suitable for the automatic default catalog.
    Stable,
    /// Candidate is recorded for explicit opt-in or future promotion.
    Experimental,
    /// No compatible semantic server is currently promoted for this profile.
    FallbackOnly,
}

/// Host platforms on which a built-in profile may be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinPlatform {
    /// Any supported host platform.
    Any,
    /// Linux hosts.
    Linux,
    /// macOS hosts.
    Macos,
    /// Windows hosts.
    Windows,
}

impl BuiltinPlatform {
    const fn is_supported(self) -> bool {
        match self {
            Self::Any => true,
            Self::Linux => cfg!(target_os = "linux"),
            Self::Macos => cfg!(target_os = "macos"),
            Self::Windows => cfg!(target_os = "windows"),
        }
    }
}

/// One ordered command candidate for a built-in profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinServerCandidate {
    /// Executable name or path.
    pub command: &'static str,
    /// Arguments passed before LSP stdio traffic begins.
    pub args: &'static [&'static str],
    /// Promotion status of this candidate.
    pub stability: BuiltinProfileStability,
}

/// Declarative metadata for one built-in language profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinLanguageProfile {
    /// LSP language identifier.
    pub language_id: &'static str,
    /// File patterns routed to this profile.
    pub file_patterns: &'static [&'static str],
    /// Project markers that take precedence over source-only detection.
    pub project_markers: &'static [&'static str],
    /// Source patterns used for markerless projects.
    pub source_patterns: &'static [&'static str],
    /// Ordered executable candidates.
    pub candidates: &'static [BuiltinServerCandidate],
    /// Host platform gates for this profile.
    pub platforms: &'static [BuiltinPlatform],
    /// Profile-specific directory exclusions.
    pub excluded_directories: &'static [&'static str],
    /// Content predicates used to disambiguate source files.
    pub content_exclusions: &'static [SourceContentExclusion],
    /// Profile language IDs superseded when this profile applies.
    pub supersedes: &'static [&'static str],
}

const fn candidate(
    command: &'static str,
    args: &'static [&'static str],
    stability: BuiltinProfileStability,
) -> BuiltinServerCandidate {
    BuiltinServerCandidate {
        command,
        args,
        stability,
    }
}

const fn profile(
    language_id: &'static str,
    file_patterns: &'static [&'static str],
    project_markers: &'static [&'static str],
    source_patterns: &'static [&'static str],
    candidates: &'static [BuiltinServerCandidate],
    supersedes: &'static [&'static str],
) -> BuiltinLanguageProfile {
    profile_with_options(
        language_id,
        file_patterns,
        project_markers,
        source_patterns,
        candidates,
        &[BuiltinPlatform::Any],
        &[],
        &[],
        supersedes,
    )
}

#[allow(clippy::too_many_arguments)]
const fn profile_with_options(
    language_id: &'static str,
    file_patterns: &'static [&'static str],
    project_markers: &'static [&'static str],
    source_patterns: &'static [&'static str],
    candidates: &'static [BuiltinServerCandidate],
    platforms: &'static [BuiltinPlatform],
    excluded_directories: &'static [&'static str],
    content_exclusions: &'static [SourceContentExclusion],
    supersedes: &'static [&'static str],
) -> BuiltinLanguageProfile {
    BuiltinLanguageProfile {
        language_id,
        file_patterns,
        project_markers,
        source_patterns,
        candidates,
        platforms,
        excluded_directories,
        content_exclusions,
        supersedes,
    }
}

const fn fallback_profile(
    language_id: &'static str,
    file_patterns: &'static [&'static str],
) -> BuiltinLanguageProfile {
    profile(language_id, file_patterns, &[], file_patterns, &[], &[])
}

static BUILTIN_LANGUAGE_PROFILES: &[BuiltinLanguageProfile] = &[
    profile(
        "rust",
        &["**/*.rs"],
        &["Cargo.toml", "rust-toolchain.toml"],
        &[],
        &[candidate(
            "rust-analyzer",
            &[],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "python",
        &["**/*.py"],
        &[
            "pyproject.toml",
            "setup.py",
            "requirements.txt",
            "pyrightconfig.json",
        ],
        &["**/*.py"],
        &[
            candidate("ty", &["server"], BuiltinProfileStability::Stable),
            candidate(
                "pyright-langserver",
                &["--stdio"],
                BuiltinProfileStability::Stable,
            ),
        ],
        &[],
    ),
    profile_with_options(
        "typescript",
        &["**/*.ts", "**/*.tsx"],
        &["package.json", "tsconfig.json", "jsconfig.json"],
        &["**/*.ts", "**/*.tsx"],
        &[
            candidate("vtsls", &["--stdio"], BuiltinProfileStability::Stable),
            candidate(
                "typescript-language-server",
                &["--stdio"],
                BuiltinProfileStability::Stable,
            ),
        ],
        &[BuiltinPlatform::Any],
        &["node_modules", "dist", "build"],
        &[SourceContentExclusion::QtTranslationXml],
        &[],
    ),
    profile(
        "javascript",
        &["**/*.js", "**/*.jsx", "**/*.mjs", "**/*.cjs"],
        &["package.json", "jsconfig.json"],
        &[],
        &[candidate(
            "typescript-language-server",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "vue",
        &["**/*.vue"],
        &[
            "package.json",
            "vite.config.js",
            "vite.config.ts",
            "vue.config.js",
        ],
        &["**/*.vue"],
        &[candidate(
            "vue-language-server",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &["typescript", "javascript", "html"],
    ),
    profile(
        "angular",
        &["**/*.ts", "**/*.html"],
        &["angular.json"],
        &["**/*.component.ts", "**/*.component.html"],
        &[candidate(
            "ngserver",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &["typescript", "javascript", "html"],
    ),
    profile(
        "java",
        &["**/*.java"],
        &["pom.xml", "build.gradle", "build.gradle.kts", ".project"],
        &["**/*.java"],
        &[candidate("jdtls", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "kotlin",
        &["**/*.kt", "**/*.kts"],
        &["build.gradle.kts", "settings.gradle.kts", "pom.xml"],
        &["**/*.kt", "**/*.kts"],
        &[candidate(
            "kotlin-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "groovy",
        &["**/*.groovy", "**/*.gradle"],
        &["build.gradle", "settings.gradle"],
        &["**/*.groovy", "**/*.gradle"],
        &[candidate(
            "groovy-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "gradle",
        &["**/*.gradle", "**/*.gradle.kts"],
        &[
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "settings.gradle.kts",
        ],
        &["**/*.gradle", "**/*.gradle.kts"],
        &[candidate(
            "gradle-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "elixir",
        &["**/*.ex", "**/*.exs"],
        &["mix.exs"],
        &["**/*.ex", "**/*.exs"],
        &[
            candidate(
                "expert",
                &["language-server"],
                BuiltinProfileStability::Experimental,
            ),
            candidate("elixir-ls", &[], BuiltinProfileStability::Stable),
        ],
        &[],
    ),
    profile(
        "erlang",
        &["**/*.erl", "**/*.hrl", "**/*.yrl"],
        &["rebar.config", "erlang.mk", "mix.exs"],
        &["**/*.erl", "**/*.hrl", "**/*.yrl"],
        &[
            candidate("elp", &["server"], BuiltinProfileStability::Stable),
            candidate("erlang_ls", &[], BuiltinProfileStability::Stable),
        ],
        &[],
    ),
    profile(
        "cpp",
        &[
            "**/*.c", "**/*.cc", "**/*.cpp", "**/*.cxx", "**/*.h", "**/*.hpp", "**/*.hh",
        ],
        &[
            "CMakeLists.txt",
            "compile_commands.json",
            "Makefile",
            ".clangd",
        ],
        &[
            "**/*.c", "**/*.cc", "**/*.cpp", "**/*.cxx", "**/*.h", "**/*.hpp", "**/*.hh",
        ],
        &[candidate("clangd", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "objective-c",
        &["**/*.m", "**/*.mm"],
        &["compile_commands.json", "CMakeLists.txt", "project.pbxproj"],
        &[],
        &[candidate("clangd", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile_with_options(
        "swift",
        &["**/*.swift"],
        &[
            "Package.swift",
            "project.pbxproj",
            "contents.xcworkspacedata",
        ],
        &["**/*.swift"],
        &[candidate(
            "sourcekit-lsp",
            &[],
            BuiltinProfileStability::Stable,
        )],
        &[BuiltinPlatform::Linux, BuiltinPlatform::Macos],
        &["DerivedData", "build"],
        &[],
        &[],
    ),
    profile(
        "go",
        &["**/*.go"],
        &["go.mod", "go.sum"],
        &["**/*.go"],
        &[candidate(
            "gopls",
            &["serve"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "zig",
        &["**/*.zig"],
        &["build.zig", "build.zig.zon"],
        &["**/*.zig"],
        &[candidate("zls", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "ruby",
        &["**/*.rb", "**/*.rake"],
        &["Gemfile", ".ruby-version", "Rakefile"],
        &["**/*.rb", "**/*.rake"],
        &[candidate("ruby-lsp", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "haskell",
        &["**/*.hs", "**/*.lhs"],
        &["*.cabal", "stack.yaml", "package.yaml", "cabal.project"],
        &["**/*.hs", "**/*.lhs"],
        &[candidate(
            "haskell-language-server-wrapper",
            &["--lsp"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "perl",
        &["**/*.pl", "**/*.pm", "**/*.t"],
        &["Makefile.PL", "Build.PL", "cpanfile"],
        &["**/*.pl", "**/*.pm"],
        &[candidate(
            "perl-language-server",
            &["--stdio"],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "powershell",
        &["**/*.ps1", "**/*.psm1", "**/*.psd1"],
        &["*.psd1", "*.psm1"],
        &["**/*.ps1", "**/*.psm1", "**/*.psd1"],
        &[candidate(
            "PowerShellEditorServices",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "nix",
        &["**/*.nix"],
        &[
            "flake.nix",
            "shell.nix",
            "default.nix",
            "configuration.nix",
            "home.nix",
        ],
        &["**/*.nix"],
        &[candidate("nixd", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "shellscript",
        &["**/*.sh", "**/*.bash", "**/*.zsh"],
        &[".bashrc", ".zshrc", "Makefile", "flake.nix"],
        &["**/*.sh", "**/*.bash", "**/*.zsh"],
        &[candidate(
            "bash-language-server",
            &["start"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "sql",
        &["**/*.sql"],
        &[".sqls.yml", "dbt_project.yml"],
        &["**/*.sql"],
        &[candidate(
            "sqls",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "html",
        &["**/*.html", "**/*.htm"],
        &["package.json", "vite.config.js", "index.html"],
        &[],
        &[candidate(
            "vscode-html-language-server",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "css",
        &["**/*.css"],
        &["package.json"],
        &["**/*.css"],
        &[candidate(
            "vscode-css-language-server",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "scss",
        &["**/*.scss", "**/*.sass"],
        &["package.json"],
        &["**/*.scss", "**/*.sass"],
        &[candidate(
            "some-sass-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "json",
        &["**/*.json", "**/*.jsonc"],
        &["package.json", "tsconfig.json", "flake.lock"],
        &[],
        &[candidate(
            "vscode-json-language-server",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "yaml",
        &["**/*.yaml", "**/*.yml"],
        &[".yamllint", ".github", "docker-compose.yml"],
        &[],
        &[candidate(
            "yaml-language-server",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "xml",
        &["**/*.xml", "**/*.plist"],
        &["pom.xml", "Info.plist", "project.pbxproj"],
        &[],
        &[candidate("lemminx", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "toml",
        &["**/*.toml"],
        &["Cargo.toml", "pyproject.toml", "package.json"],
        &[],
        &[candidate(
            "taplo",
            &["lsp", "stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "markdown",
        &["**/*.md", "**/*.markdown"],
        &["README.md", "mkdocs.yml", "book.toml"],
        &[],
        &[candidate("marksman", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "dockerfile",
        &["**/Dockerfile", "**/*.dockerfile"],
        &["Dockerfile", "docker-compose.yml", "docker-compose.yaml"],
        &[],
        &[candidate(
            "docker-langserver",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "cmake",
        &["**/CMakeLists.txt", "**/*.cmake"],
        &["CMakeLists.txt", "CMakePresets.json"],
        &[],
        &[candidate(
            "neocmakelsp",
            &["stdio"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "gherkin",
        &["**/*.feature"],
        &["cucumber.yml"],
        &["**/*.feature"],
        &[candidate(
            "cucumber-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "openscad",
        &["**/*.scad"],
        &[],
        &["**/*.scad"],
        &[candidate(
            "openscad-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "qml",
        &["**/*.qml", "**/*.qmltypes"],
        &["*.qmlproject", "CMakeLists.txt"],
        &["**/*.qml", "**/*.qmltypes"],
        &[candidate("qmlls", &[], BuiltinProfileStability::Stable)],
        &["typescript", "javascript"],
    ),
    profile(
        "ansible",
        &["**/*.yaml", "**/*.yml"],
        &["ansible.cfg", "galaxy.yml", "roles"],
        &[],
        &[candidate(
            "ansible-language-server",
            &["--stdio"],
            BuiltinProfileStability::Stable,
        )],
        &["yaml"],
    ),
    profile(
        "protobuf",
        &["**/*.proto"],
        &["buf.yaml", "buf.work"],
        &["**/*.proto"],
        &[candidate(
            "buf",
            &["lsp", "serve"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "asn1",
        &["**/*.asn1", "**/*.asn"],
        &["rebar.config", "asn1.config", "Makefile"],
        &["**/*.asn1", "**/*.asn"],
        &[candidate(
            "asn1-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "autotools",
        &["**/configure.ac", "**/configure.in", "**/*.m4"],
        &["configure.ac", "configure.in", "aclocal.m4"],
        &[],
        &[candidate(
            "autotools-language-server",
            &[],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "make",
        &["**/Makefile", "**/*.mk"],
        &["Makefile", "GNUmakefile", "makefile"],
        &[],
        &[candidate(
            "make-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "clojure",
        &["**/*.clj", "**/*.cljs", "**/*.cljc"],
        &["deps.edn", "project.clj", "shadow-cljs.edn"],
        &["**/*.clj", "**/*.cljs", "**/*.cljc"],
        &[candidate(
            "clojure-lsp",
            &[],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "dart",
        &["**/*.dart"],
        &["pubspec.yaml", "analysis_options.yaml"],
        &["**/*.dart"],
        &[candidate(
            "dart",
            &["language-server"],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "latex",
        &["**/*.tex", "**/*.bib"],
        &[".latexmkrc", "texmf.cnf"],
        &["**/*.tex", "**/*.bib"],
        &[candidate("texlab", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "ocaml",
        &["**/*.ml", "**/*.mli"],
        &["dune-project", "dune-workspace", "*.opam"],
        &["**/*.ml", "**/*.mli"],
        &[candidate("ocamllsp", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "php",
        &["**/*.php"],
        &["composer.json", "phpunit.xml", "artisan"],
        &["**/*.php"],
        &[
            candidate("intelephense", &[], BuiltinProfileStability::Experimental),
            candidate(
                "phpactor",
                &["language-server"],
                BuiltinProfileStability::Experimental,
            ),
        ],
        &[],
    ),
    profile(
        "scala",
        &["**/*.scala", "**/*.sc"],
        &["build.sbt", "project/build.properties"],
        &["**/*.scala", "**/*.sc"],
        &[candidate(
            "metals",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "terraform",
        &["**/*.tf", "**/*.tfvars"],
        &["*.tf", ".terraform.lock.hcl"],
        &["**/*.tf", "**/*.tfvars"],
        &[candidate(
            "terraform-ls",
            &[],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "vhdl",
        &["**/*.vhd", "**/*.vhdl"],
        &["*.vhd", "*.vhdl"],
        &["**/*.vhd", "**/*.vhdl"],
        &[candidate("vhdl_ls", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "csharp",
        &["**/*.cs"],
        &["*.sln", "*.csproj", "global.json"],
        &["**/*.cs"],
        &[
            candidate("csharp-ls", &[], BuiltinProfileStability::Experimental),
            candidate(
                "roslyn-language-server",
                &[],
                BuiltinProfileStability::Experimental,
            ),
        ],
        &[],
    ),
    profile(
        "d",
        &["**/*.d", "**/*.di"],
        &["dub.json", "dub.sdl"],
        &[],
        &[candidate(
            "serve-d",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "dtrace",
        &["**/*.d"],
        &["DTrace", "dtrace.conf"],
        &[],
        &[candidate("dls", &[], BuiltinProfileStability::Experimental)],
        &[],
    ),
    profile(
        "device-tree",
        &["**/*.dts", "**/*.dtsi", "**/*.dtso", "**/*.overlay"],
        &["Kconfig", "Makefile", "meson.build"],
        &["**/*.dts", "**/*.dtsi", "**/*.dtso", "**/*.overlay"],
        &[candidate("dts-lsp", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "ada",
        &["**/*.adb", "**/*.ads"],
        &["*.gpr", "alire.toml"],
        &["**/*.adb", "**/*.ads"],
        &[candidate(
            "ada-language-server",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "fortran",
        &["**/*.f", "**/*.f90", "**/*.f95", "**/*.f03"],
        &["fpm.toml", "CMakeLists.txt"],
        &["**/*.f", "**/*.f90", "**/*.f95", "**/*.f03"],
        &[candidate("fortls", &[], BuiltinProfileStability::Stable)],
        &[],
    ),
    profile(
        "lua",
        &["**/*.lua"],
        &[".luacheckrc", ".luarc.json", "stylua.toml"],
        &["**/*.lua"],
        &[candidate(
            "lua-language-server",
            &[],
            BuiltinProfileStability::Stable,
        )],
        &[],
    ),
    profile(
        "assembly",
        &["**/*.s", "**/*.S", "**/*.asm"],
        &["CMakeLists.txt", "Makefile"],
        &["**/*.s", "**/*.S", "**/*.asm"],
        &[candidate(
            "asm-lsp",
            &[],
            BuiltinProfileStability::Experimental,
        )],
        &[],
    ),
    profile(
        "kconfig",
        &["**/Kconfig", "**/Kconfig.*"],
        &["Kconfig", "Config.in", "Makefile"],
        &["**/Kconfig", "**/Kconfig.*"],
        &[],
        &[],
    ),
    profile(
        "smpl",
        &["**/*.cocci"],
        &[".cocciconfig"],
        &["**/*.cocci"],
        &[],
        &[],
    ),
    profile(
        "plantuml",
        &["**/*.puml", "**/*.plantuml"],
        &["plantuml.cfg"],
        &["**/*.puml", "**/*.plantuml"],
        &[],
        &[],
    ),
    // These profiles deliberately remain fallback-only until a candidate
    // passes the initialize/shutdown capability smoke test. Keeping their
    // source predicates in the registry makes classification explicit without
    // claiming semantic support or starting an incompatible server.
    fallback_profile("al", &["**/*.al"]),
    fallback_profile("apex", &["**/*.cls", "**/*.trigger"]),
    fallback_profile("bsl", &["**/*.bsl"]),
    fallback_profile("crystal", &["**/*.cr"]),
    fallback_profile("cue", &["**/*.cue"]),
    fallback_profile("elm", &["**/*.elm"]),
    fallback_profile("fsharp", &["**/*.fs", "**/*.fsi", "**/*.fsx"]),
    fallback_profile("gdscript", &["**/*.gd"]),
    fallback_profile("gleam", &["**/*.gleam"]),
    fallback_profile("graphql", &["**/*.graphql", "**/*.gql"]),
    fallback_profile("haxe", &["**/*.hx"]),
    fallback_profile("hlsl", &["**/*.hlsl", "**/*.shader"]),
    fallback_profile("julia", &["**/*.jl"]),
    fallback_profile("ksh", &["**/*.ksh"]),
    fallback_profile("lean", &["**/*.lean"]),
    fallback_profile("luau", &["**/*.luau"]),
    fallback_profile("matlab", &["**/*.m"]),
    fallback_profile("msl", &["**/*.metal"]),
    fallback_profile("pascal", &["**/*.pas", "**/*.pp"]),
    fallback_profile("r", &["**/*.r", "**/*.R"]),
    fallback_profile("rego", &["**/*.rego"]),
    fallback_profile("solidity", &["**/*.sol"]),
    fallback_profile("systemverilog", &["**/*.sv", "**/*.svh"]),
    fallback_profile("verilog", &["**/*.v", "**/*.vh"]),
    fallback_profile("linker-script", &["**/*.ld", "**/*.lds"]),
    fallback_profile("nu", &["**/*.nu"]),
    fallback_profile("lisp", &["**/*.lisp", "**/*.lsp", "**/*.cl"]),
    fallback_profile("lispbm", &["**/*.lispbm"]),
    fallback_profile("expect", &["**/*.exp"]),
];

/// Return the immutable built-in profile catalog.
#[must_use]
pub fn builtin_language_profiles() -> &'static [BuiltinLanguageProfile] {
    BUILTIN_LANGUAGE_PROFILES
}

/// Configuration for a single LSP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServerConfig {
    /// Language identifier (e.g., "rust", "python", "typescript").
    pub language_id: String,

    /// Command to start the LSP server.
    pub command: String,

    /// Arguments to pass to the LSP server command.
    #[serde(default)]
    pub args: Vec<String>,

    /// Environment variables for the LSP server process.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// File patterns this server handles (glob patterns).
    #[serde(default)]
    pub file_patterns: Vec<String>,

    /// LSP initialization options (server-specific).
    #[serde(default)]
    pub initialization_options: Option<serde_json::Value>,

    /// Handshake timeout in seconds: bounds the `initialize` request during
    /// server startup. Does not affect individual tool-call requests sent
    /// after initialization; see [`Self::request_timeout_seconds`] for that.
    /// The LSP server's `shutdown` request during teardown uses a separate,
    /// fixed 5-second timeout that is not configurable by this field.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,

    /// Per-request timeout in seconds, applied to each LSP request issued
    /// while translating an MCP tool call (hover, definition, references, etc.).
    ///
    /// This bounds a single request attempt, not a whole tool call: on a
    /// `-32802` (content modified) response, [`crate::lsp::LspClient::request`]
    /// retries up to 4 attempts with backoff, so the worst-case latency for one
    /// tool call is `4 * request_timeout_seconds + 3.5` seconds. Completion
    /// requests are further capped at 10 seconds regardless of this value; see
    /// [`crate::lsp::LspClient::completion_timeout`].
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,

    /// Heuristics for determining if this server should be spawned.
    /// If not specified, the server will always attempt to spawn.
    #[serde(default)]
    pub heuristics: Option<ServerHeuristics>,

    /// Human-readable server identity used as the routing key.
    ///
    /// Defaults to `language_id` when omitted (see [`Self::id`]). Must be
    /// unique across all applicable servers in a workspace, regardless of
    /// language: this is what lets two servers share one `language_id`
    /// (e.g. pyright and pylsp both for `python`) without one silently
    /// overwriting the other in the maps keyed by [`ServerId`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Tools this server handles.
    ///
    /// `None` means this server is a catch-all: it serves every tool not
    /// explicitly claimed by another server for the same language.
    /// `Some(list)` restricts the server to exactly those tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handles: Option<Vec<ToolKind>>,
}

const fn default_timeout() -> u64 {
    30
}

const fn default_request_timeout() -> u64 {
    30
}

/// Maximum allowed value, in seconds, for both [`LspServerConfig::timeout_seconds`]
/// and [`LspServerConfig::request_timeout_seconds`].
///
/// Both fields are passed straight into `Duration::from_secs` — `timeout_seconds`
/// in the `initialize` handshake (`lsp::lifecycle::LspServer::initialize`),
/// `request_timeout_seconds` in [`crate::lsp::LspClient::request_timeout`].
/// tokio's `timeout`/`sleep` fall back to `Instant::far_future()` for
/// astronomically large durations instead of panicking, so an unbounded value
/// on either field (misconfiguration or typo) would silently disable the
/// timeout rather than fail with a diagnosable error. One shared constant
/// bounds both, since the underlying defect and fix are identical for each.
///
/// Set to 900 (15 minutes), not a rounder 3600 (1 hour): [`LspClient::request`]
/// retries a request up to 4 times total on a `-32802` (`ServerCancelled`)
/// response, so the worst-case latency for a single call bounded by this
/// value is `4 * 900 + 3.5s` ≈ 1 hour, not 4 hours — this constant bounds one
/// attempt, so it is chosen such that the actually-experienced worst case
/// (the retried total) stays within about an hour.
///
/// [`LspClient::request`]: crate::lsp::LspClient::request
pub const MAX_TIMEOUT_SECONDS: u64 = 900;

impl LspServerConfig {
    /// Check if this server should be spawned for the given workspace.
    ///
    /// Uses recursive marker search to detect nested projects.
    ///
    /// # Arguments
    ///
    /// * `workspace_root` - Root directory of the workspace
    /// * `max_depth` - Maximum depth for recursive search (default: 10)
    #[must_use]
    pub fn should_spawn(&self, workspace_root: &Path, max_depth: Option<usize>) -> bool {
        self.heuristics
            .as_ref()
            .is_none_or(|h| h.is_applicable_recursive(workspace_root, max_depth))
    }

    /// The routing identity of this server: `name` if set, otherwise `language_id`.
    ///
    /// This is the key used across `Translator`'s client/server maps, so two
    /// servers for the same language must set distinct `name`s or they
    /// collide (see `ToolRouter::from_configs` for the enforcement).
    #[must_use]
    pub fn id(&self) -> ServerId {
        self.name
            .clone()
            .map_or_else(|| ServerId::from(self.language_id.clone()), ServerId::from)
    }

    /// Build a built-in server config, filling in every field not passed as
    /// a parameter.
    fn builtin(
        language_id: &str,
        command: &str,
        args: &[&str],
        file_patterns: &[&str],
        markers: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            language_id: language_id.to_string(),
            command: command.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            env: HashMap::new(),
            file_patterns: file_patterns.iter().map(ToString::to_string).collect(),
            initialization_options: None,
            timeout_seconds: default_timeout(),
            request_timeout_seconds: default_request_timeout(),
            heuristics: Some(ServerHeuristics::with_markers(markers)),
            name: None,
            handles: None,
        }
    }

    fn from_profile(profile: &BuiltinLanguageProfile) -> Option<Self> {
        let candidate = profile
            .candidates
            .iter()
            .find(|candidate| candidate.stability == BuiltinProfileStability::Stable)?;
        Some(Self {
            language_id: profile.language_id.to_string(),
            command: candidate.command.to_string(),
            args: candidate.args.iter().map(ToString::to_string).collect(),
            env: HashMap::new(),
            file_patterns: profile
                .file_patterns
                .iter()
                .map(ToString::to_string)
                .collect(),
            initialization_options: None,
            timeout_seconds: default_timeout(),
            request_timeout_seconds: default_request_timeout(),
            heuristics: Some(
                ServerHeuristics::with_markers(profile.project_markers.iter().copied())
                    .with_source_patterns(profile.source_patterns.iter().copied())
                    .with_excluded_directories(profile.excluded_directories.iter().copied())
                    .with_content_exclusions(profile.content_exclusions.iter().copied()),
            ),
            name: None,
            handles: None,
        })
    }

    /// Return the built-in profile that produced this configuration, if any.
    #[must_use]
    pub fn builtin_profile(&self) -> Option<&'static BuiltinLanguageProfile> {
        builtin_language_profiles().iter().find(|profile| {
            profile.language_id == self.language_id
                && profile.candidates.iter().any(|candidate| {
                    candidate.command == self.command
                        || Path::new(&self.command)
                            .file_name()
                            .is_some_and(|name| name == candidate.command)
                })
        })
    }

    /// Whether this configuration is an optional built-in profile.
    #[must_use]
    pub fn is_optional_builtin_profile(&self) -> bool {
        self.builtin_profile()
            .is_some_and(|profile| !profile.candidates.is_empty())
    }

    /// Create a default configuration for rust-analyzer.
    #[must_use]
    pub fn rust_analyzer() -> Self {
        Self::builtin(
            "rust",
            "rust-analyzer",
            &[],
            &["**/*.rs"],
            ["Cargo.toml", "rust-toolchain.toml"],
        )
    }

    /// Create a default configuration for pyright.
    #[must_use]
    pub fn pyright() -> Self {
        let mut config = Self::builtin(
            "python",
            "pyright-langserver",
            &["--stdio"],
            &["**/*.py"],
            [
                "pyproject.toml",
                "setup.py",
                "requirements.txt",
                "pyrightconfig.json",
            ],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns.push("**/*.py".to_string());
        }
        config
    }

    /// Create the preferred default configuration for ty.
    #[must_use]
    pub fn ty() -> Self {
        let mut config = Self::builtin(
            "python",
            "ty",
            &["server"],
            &["**/*.py"],
            [
                "pyproject.toml",
                "setup.py",
                "requirements.txt",
                "pyrightconfig.json",
            ],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns.push("**/*.py".to_string());
        }
        config
    }

    /// Create a default configuration for TypeScript language server.
    #[must_use]
    pub fn typescript() -> Self {
        let mut config = Self::builtin(
            "typescript",
            "typescript-language-server",
            &["--stdio"],
            &["**/*.ts", "**/*.tsx"],
            ["package.json", "tsconfig.json", "jsconfig.json"],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns = vec!["**/*.ts".to_string(), "**/*.tsx".to_string()];
            heuristics.excluded_directories = vec![
                "node_modules".to_string(),
                "dist".to_string(),
                "build".to_string(),
            ];
            heuristics.content_exclusions = vec![SourceContentExclusion::QtTranslationXml];
        }
        config
    }

    /// Create the preferred default configuration for vtsls.
    #[must_use]
    pub fn vtsls() -> Self {
        let mut config = Self::builtin(
            "typescript",
            "vtsls",
            &["--stdio"],
            &["**/*.ts", "**/*.tsx"],
            ["package.json", "tsconfig.json", "jsconfig.json"],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns = vec!["**/*.ts".to_string(), "**/*.tsx".to_string()];
            heuristics.excluded_directories = vec![
                "node_modules".to_string(),
                "dist".to_string(),
                "build".to_string(),
            ];
            heuristics.content_exclusions = vec![SourceContentExclusion::QtTranslationXml];
        }
        config
    }

    /// Create a default configuration for gopls.
    #[must_use]
    pub fn gopls() -> Self {
        let mut config = Self::builtin(
            "go",
            "gopls",
            &["serve"],
            &["**/*.go"],
            ["go.mod", "go.sum"],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns.push("**/*.go".to_string());
        }
        config
    }

    /// Create a default configuration for clangd.
    #[must_use]
    pub fn clangd() -> Self {
        let mut config = Self::builtin(
            "cpp",
            "clangd",
            &[],
            &["**/*.c", "**/*.cpp", "**/*.h", "**/*.hpp"],
            [
                "CMakeLists.txt",
                "compile_commands.json",
                "Makefile",
                ".clangd",
            ],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns = vec![
                "**/*.c".to_string(),
                "**/*.cc".to_string(),
                "**/*.cpp".to_string(),
                "**/*.cxx".to_string(),
                "**/*.h".to_string(),
                "**/*.hpp".to_string(),
                "**/*.hh".to_string(),
            ];
        }
        config
    }

    /// Create a default configuration for zls.
    #[must_use]
    pub fn zls() -> Self {
        let mut config = Self::builtin(
            "zig",
            "zls",
            &[],
            &["**/*.zig"],
            ["build.zig", "build.zig.zon"],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns.push("**/*.zig".to_string());
        }
        config
    }

    /// Create a default configuration for nixd.
    #[must_use]
    pub fn nixd() -> Self {
        let mut config = Self::builtin(
            "nix",
            "nixd",
            &[],
            &["**/*.nix"],
            [
                "flake.nix",
                "shell.nix",
                "default.nix",
                "configuration.nix",
                "home.nix",
            ],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns.push("**/*.nix".to_string());
        }
        config
    }

    /// Create a default configuration for sourcekit-lsp on Linux and macOS.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[must_use]
    pub fn sourcekit_lsp() -> Self {
        let mut config = Self::builtin(
            "swift",
            "sourcekit-lsp",
            &[],
            &["**/*.swift"],
            [
                "Package.swift",
                "project.pbxproj",
                "contents.xcworkspacedata",
            ],
        );
        if let Some(heuristics) = &mut config.heuristics {
            heuristics.source_patterns.push("**/*.swift".to_string());
        }
        config
    }
}

/// Build the automatic default server list from the built-in catalog.
#[must_use]
pub fn builtin_server_configs() -> Vec<LspServerConfig> {
    let mut configs = vec![
        LspServerConfig::rust_analyzer(),
        LspServerConfig::ty(),
        LspServerConfig::vtsls(),
        LspServerConfig::gopls(),
        LspServerConfig::clangd(),
        LspServerConfig::zls(),
        LspServerConfig::nixd(),
    ];
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    configs.push(LspServerConfig::sourcekit_lsp());

    for profile in builtin_language_profiles() {
        if !profile
            .platforms
            .iter()
            .copied()
            .any(BuiltinPlatform::is_supported)
        {
            continue;
        }
        if configs
            .iter()
            .any(|config| config.language_id == profile.language_id)
        {
            continue;
        }
        if let Some(config) = LspServerConfig::from_profile(profile) {
            configs.push(config);
        }
    }
    configs
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_rust_analyzer_defaults() {
        let config = LspServerConfig::rust_analyzer();

        assert_eq!(config.language_id, "rust");
        assert_eq!(config.command, "rust-analyzer");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.rs"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, 30);
    }

    #[test]
    fn test_pyright_defaults() {
        let config = LspServerConfig::pyright();

        assert_eq!(config.language_id, "python");
        assert_eq!(config.command, "pyright-langserver");
        assert_eq!(config.args, vec!["--stdio"]);
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.py"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, 30);
    }

    #[test]
    fn test_typescript_defaults() {
        let config = LspServerConfig::typescript();

        assert_eq!(config.language_id, "typescript");
        assert_eq!(config.command, "typescript-language-server");
        assert_eq!(config.args, vec!["--stdio"]);
        assert!(config.env.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.ts", "**/*.tsx"]);
        assert!(config.initialization_options.is_none());
        assert_eq!(config.timeout_seconds, 30);
    }

    #[test]
    fn test_nixd_defaults() {
        let config = LspServerConfig::nixd();

        assert_eq!(config.language_id, "nix");
        assert_eq!(config.command, "nixd");
        assert!(config.args.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.nix"]);
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"flake.nix".to_string()));
        assert!(markers.contains(&"configuration.nix".to_string()));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn test_sourcekit_lsp_defaults() {
        let config = LspServerConfig::sourcekit_lsp();

        assert_eq!(config.language_id, "swift");
        assert_eq!(config.command, "sourcekit-lsp");
        assert!(config.args.is_empty());
        assert_eq!(config.file_patterns, vec!["**/*.swift"]);
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"Package.swift".to_string()));
        assert!(markers.contains(&"project.pbxproj".to_string()));
    }

    #[test]
    fn test_default_timeout() {
        assert_eq!(default_timeout(), 30);
    }

    #[test]
    fn test_custom_config() {
        let mut env = HashMap::new();
        env.insert("RUST_LOG".to_string(), "debug".to_string());

        let config = LspServerConfig {
            language_id: "custom".to_string(),
            command: "custom-lsp".to_string(),
            args: vec!["--flag".to_string()],
            env: env.clone(),
            file_patterns: vec!["**/*.custom".to_string()],
            initialization_options: Some(serde_json::json!({"key": "value"})),
            timeout_seconds: 60,
            request_timeout_seconds: 45,
            heuristics: None,
            name: None,
            handles: None,
        };

        assert_eq!(config.language_id, "custom");
        assert_eq!(config.command, "custom-lsp");
        assert_eq!(config.args, vec!["--flag"]);
        assert_eq!(config.env.get("RUST_LOG"), Some(&"debug".to_string()));
        assert_eq!(config.file_patterns, vec!["**/*.custom"]);
        assert!(config.initialization_options.is_some());
        assert_eq!(config.timeout_seconds, 60);
    }

    #[test]
    fn test_serde_roundtrip() {
        let original = LspServerConfig::rust_analyzer();

        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: LspServerConfig = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.language_id, original.language_id);
        assert_eq!(deserialized.command, original.command);
        assert_eq!(deserialized.args, original.args);
        assert_eq!(deserialized.timeout_seconds, original.timeout_seconds);
        assert_eq!(
            deserialized.request_timeout_seconds,
            original.request_timeout_seconds
        );
    }

    #[test]
    fn test_default_request_timeout() {
        assert_eq!(default_request_timeout(), 30);
    }

    #[test]
    fn test_clone() {
        let config = LspServerConfig::rust_analyzer();
        let cloned = config.clone();

        assert_eq!(cloned.language_id, config.language_id);
        assert_eq!(cloned.command, config.command);
        assert_eq!(cloned.timeout_seconds, config.timeout_seconds);
    }

    #[test]
    fn test_empty_env() {
        let config = LspServerConfig::rust_analyzer();
        assert!(config.env.is_empty());
    }

    #[test]
    fn test_multiple_file_patterns() {
        let config = LspServerConfig::typescript();
        assert_eq!(config.file_patterns.len(), 2);
        assert!(config.file_patterns.contains(&"**/*.ts".to_string()));
        assert!(config.file_patterns.contains(&"**/*.tsx".to_string()));
    }

    #[test]
    fn test_initialization_options_none_by_default() {
        let configs = vec![
            LspServerConfig::rust_analyzer(),
            LspServerConfig::pyright(),
            LspServerConfig::typescript(),
        ];

        for config in configs {
            assert!(config.initialization_options.is_none());
        }
    }

    // Heuristics tests
    #[test]
    fn test_heuristics_empty_always_applicable() {
        let heuristics = ServerHeuristics::default();
        let tmp = TempDir::new().unwrap();
        assert!(heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_heuristics_marker_present() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_heuristics_marker_absent() {
        let tmp = TempDir::new().unwrap();
        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_source_pattern_matches_markerless_project() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.swift"), "import Foundation\n").unwrap();

        let heuristics = ServerHeuristics::default().with_source_patterns(["**/*.swift"]);

        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
        assert_eq!(
            heuristics.matching_roots(tmp.path(), None),
            vec![tmp.path()]
        );
    }

    #[test]
    fn test_typescript_content_guard_distinguishes_qt_translation_xml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}\n").unwrap();
        std::fs::write(
            tmp.path().join("translations.ts"),
            "<?xml version=\"1.0\"?><TS version=\"2.1\"></TS>\n",
        )
        .unwrap();

        assert!(!LspServerConfig::typescript().should_spawn(tmp.path(), None));

        std::fs::write(tmp.path().join("app.ts"), "export const answer = 42;\n").unwrap();
        assert!(LspServerConfig::typescript().should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_qml_source_is_not_claimed_by_typescript() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.qml"), "import QtQuick\nItem {}\n").unwrap();

        let qml = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "qml")
            .unwrap();
        let qml_heuristics = ServerHeuristics::with_markers(qml.project_markers.iter().copied())
            .with_source_patterns(qml.source_patterns.iter().copied());
        assert!(qml_heuristics.is_applicable_recursive(tmp.path(), None));
        assert!(!LspServerConfig::typescript().should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_objective_c_m_files_require_project_context() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("script.m"),
            "function y = f(x)\n y = x;\nend\n",
        )
        .unwrap();

        let objective_c = builtin_server_configs()
            .into_iter()
            .find(|config| config.language_id == "objective-c")
            .unwrap();
        assert!(!objective_c.should_spawn(tmp.path(), None));

        std::fs::write(tmp.path().join("compile_commands.json"), "[]\n").unwrap();
        assert!(objective_c.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_markerless_swift_and_openscad_source_detection() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.swift"), "print(\"hello\")\n").unwrap();
        std::fs::write(tmp.path().join("part.scad"), "cube(1);\n").unwrap();

        for language_id in ["swift", "openscad"] {
            let profile = builtin_language_profiles()
                .iter()
                .find(|profile| profile.language_id == language_id)
                .unwrap();
            let heuristics =
                ServerHeuristics::with_markers(profile.project_markers.iter().copied())
                    .with_source_patterns(profile.source_patterns.iter().copied())
                    .with_excluded_directories(profile.excluded_directories.iter().copied());
            assert!(
                heuristics.is_applicable_recursive(tmp.path(), None),
                "{language_id} source presence should activate markerless project"
            );
        }
    }

    #[test]
    fn test_monorepo_detection_returns_effective_language_roots() {
        let tmp = TempDir::new().unwrap();
        let frontend = tmp.path().join("frontend");
        let backend = tmp.path().join("backend");
        std::fs::create_dir_all(&frontend).unwrap();
        std::fs::create_dir_all(backend.join("src")).unwrap();
        std::fs::write(frontend.join("package.json"), "{}\n").unwrap();
        std::fs::write(frontend.join("app.ts"), "export const app = true;\n").unwrap();
        std::fs::write(backend.join("Cargo.toml"), "[package]\nname=\"backend\"\n").unwrap();
        std::fs::write(backend.join("src/main.rs"), "fn main() {}\n").unwrap();

        let typescript = LspServerConfig::typescript();
        let rust = LspServerConfig::rust_analyzer();
        assert_eq!(
            typescript
                .heuristics
                .as_ref()
                .unwrap()
                .matching_roots(tmp.path(), None),
            vec![frontend]
        );
        assert_eq!(
            rust.heuristics
                .as_ref()
                .unwrap()
                .matching_roots(tmp.path(), None),
            vec![backend]
        );
    }

    #[test]
    fn test_source_pattern_excludes_generated_tree() {
        let tmp = TempDir::new().unwrap();
        let generated = tmp.path().join("generated");
        std::fs::create_dir_all(&generated).unwrap();
        std::fs::write(generated.join("main.swift"), "generated").unwrap();

        let heuristics = ServerHeuristics::default().with_source_patterns(["**/*.swift"]);

        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_profile_with_no_matching_language_stays_inactive() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {}\n").unwrap();

        assert!(!LspServerConfig::typescript().should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_builtin_catalog_keeps_specialist_precedence_typed() {
        let vue = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "vue")
            .unwrap();
        assert!(vue.supersedes.contains(&"typescript"));
        assert!(vue.source_patterns.contains(&"**/*.vue"));

        let fallback_only = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "kconfig")
            .unwrap();
        assert!(fallback_only.candidates.is_empty());

        let swift = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "swift")
            .unwrap();
        assert!(swift.platforms.contains(&BuiltinPlatform::Linux));
        assert!(swift.platforms.contains(&BuiltinPlatform::Macos));
        assert!(swift.excluded_directories.contains(&"DerivedData"));

        let typescript = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "typescript")
            .unwrap();
        assert!(
            typescript
                .content_exclusions
                .contains(&SourceContentExclusion::QtTranslationXml)
        );
    }

    #[test]
    fn test_builtin_override_without_heuristics_keeps_optional_fallback() {
        let mut config = LspServerConfig::pyright();
        config.heuristics = None;
        config.command = "/definitely-missing/pyright-langserver".to_string();

        assert!(config.is_optional_builtin_profile());
    }

    #[test]
    fn test_builtin_catalog_covers_optional_language_surface() {
        let profiles = builtin_language_profiles();
        let mut language_ids = profiles
            .iter()
            .map(|profile| profile.language_id)
            .collect::<Vec<_>>();
        language_ids.sort_unstable();
        language_ids.dedup();
        assert_eq!(
            language_ids.len(),
            profiles.len(),
            "duplicate profile language ID"
        );

        for language in [
            "al",
            "apex",
            "asn1",
            "autotools",
            "bsl",
            "clojure",
            "crystal",
            "cue",
            "dart",
            "dtrace",
            "elm",
            "expect",
            "fsharp",
            "gdscript",
            "gleam",
            "graphql",
            "haxe",
            "hlsl",
            "julia",
            "ksh",
            "latex",
            "lean",
            "linker-script",
            "lisp",
            "lispbm",
            "luau",
            "matlab",
            "msl",
            "nu",
            "pascal",
            "php",
            "r",
            "rego",
            "scala",
            "solidity",
            "systemverilog",
            "terraform",
            "verilog",
            "vhdl",
        ] {
            assert!(
                profiles
                    .iter()
                    .any(|profile| profile.language_id == language),
                "missing built-in profile: {language}"
            );
        }

        let php = profiles
            .iter()
            .find(|profile| profile.language_id == "php")
            .unwrap();
        assert_eq!(php.candidates.len(), 2);
        assert!(
            php.candidates
                .iter()
                .all(|candidate| candidate.stability == BuiltinProfileStability::Experimental)
        );

        let terraform = profiles
            .iter()
            .find(|profile| profile.language_id == "terraform")
            .unwrap();
        assert_eq!(terraform.candidates[0].command, "terraform-ls");
    }

    #[test]
    fn test_builtin_profiles_have_complete_typed_metadata() {
        for profile in builtin_language_profiles() {
            assert!(!profile.language_id.is_empty());
            assert!(
                !profile.file_patterns.is_empty(),
                "{} has no file patterns",
                profile.language_id
            );
            assert!(
                !profile.platforms.is_empty(),
                "{} has no platform gate",
                profile.language_id
            );
            for candidate in profile.candidates {
                assert!(
                    !candidate.command.is_empty(),
                    "{} has an empty candidate",
                    profile.language_id
                );
            }
            assert!(
                profile
                    .supersedes
                    .iter()
                    .all(|language| !language.eq_ignore_ascii_case(profile.language_id)),
                "{} supersedes itself",
                profile.language_id
            );
        }
    }

    #[test]
    fn test_builtin_profile_commands_use_lsp_stdio_modes_and_smoked_erlang_default() {
        let fortls = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "fortran")
            .unwrap();
        assert!(
            fortls
                .candidates
                .iter()
                .any(|candidate| candidate.command == "fortls" && candidate.args.is_empty())
        );

        let neocmakelsp = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "cmake")
            .unwrap();
        assert!(neocmakelsp.candidates.iter().any(|candidate| {
            candidate.command == "neocmakelsp" && candidate.args == ["stdio"]
        }));

        let erlang = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "erlang")
            .unwrap();
        assert_eq!(
            erlang.candidates[0].stability,
            BuiltinProfileStability::Stable
        );
        assert!(
            erlang
                .candidates
                .iter()
                .any(|candidate| candidate.command == "elp" && candidate.args == ["server"])
        );
        let default = builtin_server_configs()
            .into_iter()
            .find(|config| config.language_id == "erlang")
            .unwrap();
        assert_eq!(default.command, "elp");
        assert_eq!(default.args, vec!["server"]);

        let python = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "python")
            .unwrap();
        assert_eq!(
            python.candidates[0].stability,
            BuiltinProfileStability::Stable
        );
        let default = builtin_server_configs()
            .into_iter()
            .find(|config| config.language_id == "python")
            .unwrap();
        assert_eq!(default.command, "ty");
        assert_eq!(default.args, vec!["server"]);

        let typescript = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "typescript")
            .unwrap();
        assert_eq!(
            typescript.candidates[0].stability,
            BuiltinProfileStability::Stable
        );
        let default = builtin_server_configs()
            .into_iter()
            .find(|config| config.language_id == "typescript")
            .unwrap();
        assert_eq!(default.command, "vtsls");
        assert_eq!(default.args, vec!["--stdio"]);

        for language_id in ["fortran", "cmake", "device-tree", "autotools"] {
            let profile = builtin_language_profiles()
                .iter()
                .find(|profile| profile.language_id == language_id)
                .unwrap();
            assert_eq!(
                profile.candidates[0].stability,
                BuiltinProfileStability::Stable,
                "{language_id} was not promoted after capability smoke"
            );
        }

        let xml = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "xml")
            .unwrap();
        assert_eq!(xml.candidates[0].command, "lemminx");
        assert_eq!(xml.candidates[0].stability, BuiltinProfileStability::Stable);

        let ada = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "ada")
            .unwrap();
        assert_eq!(
            ada.candidates[0].stability,
            BuiltinProfileStability::Experimental
        );
    }

    #[test]
    fn test_markerless_openscad_profile_is_source_gated() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("model.scad"), "cube(1);\n").unwrap();
        let profile = builtin_language_profiles()
            .iter()
            .find(|profile| profile.language_id == "openscad")
            .unwrap();
        let heuristics = ServerHeuristics::default()
            .with_source_patterns(profile.source_patterns.iter().copied());
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
        assert!(
            profile
                .candidates
                .iter()
                .all(|candidate| { candidate.stability != BuiltinProfileStability::Stable })
        );
    }

    #[test]
    fn test_heuristics_any_marker_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("setup.py"), "").unwrap();

        let heuristics =
            ServerHeuristics::with_markers(["pyproject.toml", "setup.py", "requirements.txt"]);
        assert!(heuristics.is_applicable(tmp.path()));
    }

    #[test]
    fn test_should_spawn_without_heuristics() {
        let config = LspServerConfig {
            language_id: "test".to_string(),
            command: "test-lsp".to_string(),
            args: vec![],
            env: HashMap::new(),
            file_patterns: vec![],
            initialization_options: None,
            timeout_seconds: 30,
            request_timeout_seconds: 30,
            heuristics: None,
            name: None,
            handles: None,
        };

        let tmp = TempDir::new().unwrap();
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_should_spawn_with_heuristics() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let config = LspServerConfig::rust_analyzer();
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_should_not_spawn_without_markers() {
        let tmp = TempDir::new().unwrap();
        let config = LspServerConfig::rust_analyzer();
        assert!(!config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_heuristics_serde_roundtrip() {
        let heuristics = ServerHeuristics::with_markers(["Cargo.toml", "rust-toolchain.toml"]);
        let json = serde_json::to_string(&heuristics).unwrap();
        let deserialized: ServerHeuristics = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.project_markers, heuristics.project_markers);
    }

    #[test]
    fn test_default_rust_analyzer_heuristics() {
        let config = LspServerConfig::rust_analyzer();
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"Cargo.toml".to_string()));
    }

    #[test]
    fn test_gopls_defaults() {
        let config = LspServerConfig::gopls();

        assert_eq!(config.language_id, "go");
        assert_eq!(config.command, "gopls");
        assert_eq!(config.args, vec!["serve"]);
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"go.mod".to_string()));
        assert!(markers.contains(&"go.sum".to_string()));
    }

    #[test]
    fn test_clangd_defaults() {
        let config = LspServerConfig::clangd();

        assert_eq!(config.language_id, "cpp");
        assert_eq!(config.command, "clangd");
        assert!(config.args.is_empty());
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"CMakeLists.txt".to_string()));
        assert!(markers.contains(&"compile_commands.json".to_string()));
    }

    #[test]
    fn test_zls_defaults() {
        let config = LspServerConfig::zls();

        assert_eq!(config.language_id, "zig");
        assert_eq!(config.command, "zls");
        assert!(config.args.is_empty());
        assert!(config.heuristics.is_some());
        let markers = &config.heuristics.unwrap().project_markers;
        assert!(markers.contains(&"build.zig".to_string()));
        assert!(markers.contains(&"build.zig.zon".to_string()));
    }

    // Recursive scanning tests
    #[test]
    fn test_recursive_empty_markers_always_applicable() {
        let heuristics = ServerHeuristics::default();
        let tmp = TempDir::new().unwrap();
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_marker_at_root() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_nested_python_project() {
        let tmp = TempDir::new().unwrap();
        // Create Rust project at root
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        // Create nested Python project
        let python_dir = tmp.path().join("python");
        std::fs::create_dir(&python_dir).unwrap();
        std::fs::write(python_dir.join("pyproject.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["pyproject.toml", "setup.py"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_deeply_nested_marker() {
        let tmp = TempDir::new().unwrap();
        // Create a deeply nested structure
        let deep_path = tmp.path().join("level1").join("level2").join("level3");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("go.mod"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["go.mod"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_no_marker_found() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("main.rs"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_max_depth_respected() {
        let tmp = TempDir::new().unwrap();
        // Create marker at depth 5
        let deep_path = tmp.path().join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        // With max_depth=3, should not find marker at depth 5
        assert!(!heuristics.is_applicable_recursive(tmp.path(), Some(3)));
        // With max_depth=10 (default), should find it
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_node_modules() {
        let tmp = TempDir::new().unwrap();
        // Create package.json inside node_modules (should be ignored)
        let node_modules = tmp.path().join("node_modules").join("some-package");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("package.json"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["package.json"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_target_directory() {
        let tmp = TempDir::new().unwrap();
        // Create Cargo.toml inside target (should be ignored)
        let target = tmp.path().join("target").join("debug");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_git_directory() {
        let tmp = TempDir::new().unwrap();
        let git_dir = tmp.path().join(".git").join("hooks");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("Cargo.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_pycache() {
        let tmp = TempDir::new().unwrap();
        let pycache = tmp.path().join("__pycache__");
        std::fs::create_dir_all(&pycache).unwrap();
        std::fs::write(pycache.join("pyproject.toml"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["pyproject.toml"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_excludes_venv() {
        let tmp = TempDir::new().unwrap();
        let venv = tmp.path().join(".venv").join("lib");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("setup.py"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["setup.py"]);
        assert!(!heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_finds_marker_outside_excluded() {
        let tmp = TempDir::new().unwrap();
        // Create excluded dir with marker
        let node_modules = tmp.path().join("node_modules");
        std::fs::create_dir_all(&node_modules).unwrap();
        std::fs::write(node_modules.join("package.json"), "").unwrap();
        // Create valid marker in src
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("package.json"), "").unwrap();

        let heuristics = ServerHeuristics::with_markers(["package.json"]);
        assert!(heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_recursive_monorepo_structure() {
        let tmp = TempDir::new().unwrap();
        // Create monorepo with multiple language projects
        let rust_pkg = tmp.path().join("packages").join("rust-lib");
        let python_pkg = tmp.path().join("packages").join("python-bindings");
        let ts_pkg = tmp.path().join("packages").join("typescript-client");

        std::fs::create_dir_all(&rust_pkg).unwrap();
        std::fs::create_dir_all(&python_pkg).unwrap();
        std::fs::create_dir_all(&ts_pkg).unwrap();

        std::fs::write(rust_pkg.join("Cargo.toml"), "").unwrap();
        std::fs::write(python_pkg.join("pyproject.toml"), "").unwrap();
        std::fs::write(ts_pkg.join("package.json"), "").unwrap();

        // All should be detected
        let rust_heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
        let python_heuristics = ServerHeuristics::with_markers(["pyproject.toml"]);
        let ts_heuristics = ServerHeuristics::with_markers(["package.json"]);

        assert!(rust_heuristics.is_applicable_recursive(tmp.path(), None));
        assert!(python_heuristics.is_applicable_recursive(tmp.path(), None));
        assert!(ts_heuristics.is_applicable_recursive(tmp.path(), None));
    }

    #[test]
    fn test_should_spawn_recursive() {
        let tmp = TempDir::new().unwrap();
        // Create nested Python project in Rust workspace
        let python_dir = tmp.path().join("bindings").join("python");
        std::fs::create_dir_all(&python_dir).unwrap();
        std::fs::write(python_dir.join("pyproject.toml"), "").unwrap();

        let config = LspServerConfig::pyright();
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_should_spawn_with_custom_max_depth() {
        let tmp = TempDir::new().unwrap();
        let deep_path = tmp.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep_path).unwrap();
        std::fs::write(deep_path.join("Cargo.toml"), "").unwrap();

        let config = LspServerConfig::rust_analyzer();
        // Shallow depth should not find it
        assert!(!config.should_spawn(tmp.path(), Some(2)));
        // Default depth should find it
        assert!(config.should_spawn(tmp.path(), None));
    }

    #[test]
    fn test_default_heuristics_max_depth() {
        assert_eq!(DEFAULT_HEURISTICS_MAX_DEPTH, 10);
    }

    #[test]
    fn test_excluded_directories_constant() {
        assert!(EXCLUDED_DIRECTORIES.contains(&"node_modules"));
        assert!(EXCLUDED_DIRECTORIES.contains(&"target"));
        assert!(EXCLUDED_DIRECTORIES.contains(&".git"));
        assert!(EXCLUDED_DIRECTORIES.contains(&"__pycache__"));
        assert!(EXCLUDED_DIRECTORIES.contains(&".venv"));
    }
}
