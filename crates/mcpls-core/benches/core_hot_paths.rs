//! Deterministic instruction and allocation benchmarks for MCPLS hot paths.

#![allow(missing_docs, clippy::expect_used)]

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;

use gungraun::{
    Callgrind, Dhat, DhatMetric, EventKind, LibraryBenchmarkConfig, library_benchmark,
    library_benchmark_group, main,
};
use mcpls_core::bench_support::{
    desired_watch_directory_count, rust_analyzer_initialization_options,
};
use mcpls_core::config::ServerHeuristics;
use mcpls_core::project::longest_matching_root;
use tempfile::TempDir;

struct WorkspaceFixture {
    temp: TempDir,
    roots: Vec<PathBuf>,
    nested_file: PathBuf,
}

fn workspace_fixture() -> &'static WorkspaceFixture {
    let temp = TempDir::new().expect("create benchmark workspace");
    let mut roots = Vec::with_capacity(5);
    for project in 0..5 {
        let root = temp.path().join(format!("project-{project}"));
        fs::create_dir_all(root.join("src/nested")).expect("create source tree");
        fs::create_dir_all(root.join("target/debug/incremental")).expect("create target tree");
        fs::create_dir_all(root.join("node_modules/package")).expect("create dependency tree");
        fs::write(
            root.join("Cargo.toml"),
            format!("[package]\nname = \"project-{project}\"\nversion = \"0.1.0\"\n"),
        )
        .expect("write manifest");
        fs::write(root.join("src/lib.rs"), "pub fn value() -> usize { 1 }\n")
            .expect("write source");
        for directory in 0..20 {
            let directory = root.join(format!("src/module-{directory}"));
            fs::create_dir_all(&directory).expect("create module");
            fs::write(directory.join("mod.rs"), "pub fn value() -> usize { 1 }\n")
                .expect("write module");
        }
        roots.push(root);
    }
    let nested_file = roots[4].join("src/nested/file.rs");
    Box::leak(Box::new(WorkspaceFixture {
        temp,
        roots,
        nested_file,
    }))
}

#[library_benchmark]
#[bench::one_root(setup = workspace_fixture)]
fn watch_directory_scan(fixture: &WorkspaceFixture) -> Result<usize, String> {
    black_box(desired_watch_directory_count(
        black_box(&fixture.roots[0]),
        black_box(&[
            "**/*.rs",
            "**/Cargo.toml",
            "**/Cargo.lock",
            "**/rust-toolchain.toml",
            "**/.cargo/config.toml",
            "**/*.json",
            "**/*.yaml",
            "**/*.yml",
        ]),
    ))
}

#[library_benchmark]
#[bench::one_root(setup = workspace_fixture)]
fn rust_analyzer_options_one_root(fixture: &WorkspaceFixture) -> Result<serde_json::Value, String> {
    black_box(rust_analyzer_initialization_options(black_box(
        &fixture.roots[..1],
    )))
}

#[library_benchmark]
#[bench::five_linked_roots(setup = workspace_fixture)]
fn rust_analyzer_options_five_roots(
    fixture: &WorkspaceFixture,
) -> Result<serde_json::Value, String> {
    black_box(rust_analyzer_initialization_options(black_box(
        &fixture.roots,
    )))
}

#[library_benchmark]
#[bench::five_roots(setup = workspace_fixture)]
fn route_to_longest_root(fixture: &WorkspaceFixture) -> PathBuf {
    black_box(
        longest_matching_root(black_box(&fixture.nested_file), black_box(&fixture.roots))
            .expect("nested file should match a root")
            .to_path_buf(),
    )
}

#[library_benchmark]
#[bench::nested_manifest(setup = workspace_fixture)]
fn recursive_marker_detection(fixture: &WorkspaceFixture) -> bool {
    let heuristics = ServerHeuristics::with_markers(["Cargo.toml"]);
    black_box(
        heuristics.is_applicable_recursive(black_box(fixture.temp.path()), black_box(Some(10))),
    )
}

library_benchmark_group!(
    name = core_hot_paths,
    config = LibraryBenchmarkConfig::default()
        .tool(Callgrind::default().soft_limits([
            (EventKind::Ir, 10.0),
            (EventKind::EstimatedCycles, 10.0),
            (EventKind::RamHits, 10.0)
        ]))
        .tool(Dhat::default().soft_limits([
            (DhatMetric::TotalBytes, 10.0),
            (DhatMetric::TotalBlocks, 10.0),
            (DhatMetric::AtTGmaxBytes, 10.0),
            (DhatMetric::AtTGmaxBlocks, 10.0)
        ])),
    benchmarks = [
        watch_directory_scan,
        rust_analyzer_options_one_root,
        rust_analyzer_options_five_roots,
        route_to_longest_root,
        recursive_marker_detection
    ]
);

main!(library_benchmark_groups = core_hot_paths);
