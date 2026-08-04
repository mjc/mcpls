use std::path::PathBuf;

/// Returns the configured rust-analyzer binary.
#[must_use]
pub fn rust_analyzer_path() -> PathBuf {
    std::env::var_os("MCPLS_RUST_ANALYZER")
        .map_or_else(|| PathBuf::from("rust-analyzer"), PathBuf::from)
}

/// Checks if rust-analyzer is available in the system.
///
/// Returns true if rust-analyzer can be executed.
#[must_use]
pub fn rust_analyzer_available() -> bool {
    std::process::Command::new(rust_analyzer_path())
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Returns the path to the Rust workspace test fixture.
pub fn rust_workspace_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_workspace")
}

/// Returns the path to a configuration fixture.
pub fn config_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/configs")
        .join(name)
}

/// Macro to skip tests if rust-analyzer is not available.
#[macro_export]
macro_rules! skip_if_no_rust_analyzer {
    () => {
        if !$crate::common::test_utils::rust_analyzer_available() {
            eprintln!("Skipping test: rust-analyzer not available");
            return;
        }
    };
}
