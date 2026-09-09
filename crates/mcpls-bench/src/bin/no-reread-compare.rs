//! Compare two privacy-preserving no-reread evaluation reports.

use anyhow::{Context, Result, bail};
use clap::Parser;
use mcpls_bench::no_reread::{EvaluationComparison, EvaluationReport, compare_evaluations};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(about = "Compare privacy-preserving MCPLS no-reread reports")]
struct Args {
    #[arg(long)]
    before: PathBuf,
    #[arg(long)]
    after: PathBuf,
    #[arg(long)]
    output: Option<PathBuf>,
    /// Exit unsuccessfully unless report schemas, run-contract hashes, and
    /// task counts match and both MCPLS calls and context bytes decrease.
    #[arg(long)]
    require_reduction: bool,
}

fn read_report(path: &Path) -> Result<EvaluationReport> {
    serde_json::from_slice(
        &fs::read(path).with_context(|| format!("reading report {}", path.display()))?,
    )
    .with_context(|| format!("parsing report {}", path.display()))
}

fn write_report(path: &Path, report: &[u8]) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, report).with_context(|| format!("writing {}", path.display()))
}

fn run(args: &Args) -> Result<Vec<u8>> {
    let before = read_report(&args.before)?;
    let after = read_report(&args.after)?;
    let comparison: EvaluationComparison = compare_evaluations(&before, &after);
    if args.require_reduction && !comparison.accepted {
        bail!(
            "evaluation reports are not comparable or did not reduce both MCPLS calls and model-visible context bytes"
        );
    }
    serde_json::to_vec_pretty(&comparison).map_err(Into::into)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let report = run(&args)?;
    if let Some(path) = &args.output {
        write_report(path, &report)?;
    } else {
        std::io::stdout().write_all(&report)?;
        println!();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::write_report;

    #[test]
    fn output_creates_missing_parent_directories() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("reports/no-reread-comparison.json");

        write_report(&output, b"{}").unwrap();

        assert_eq!(std::fs::read(output).unwrap(), b"{}");
    }
}
