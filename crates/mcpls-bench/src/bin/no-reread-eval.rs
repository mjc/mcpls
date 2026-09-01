//! Privacy-preserving no-reread trace and local history evaluation.

use anyhow::{Context, Result, bail};
use clap::Parser;
use mcpls_bench::no_reread::{TraceEvent, evaluate, parse_history};
use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Parser)]
#[command(about = "Report privacy-preserving MCPLS no-reread trace metrics")]
struct Args {
    #[arg(long, conflicts_with = "history")]
    trace: Option<PathBuf>,
    #[arg(long, conflicts_with = "trace")]
    history: Option<PathBuf>,
    #[arg(long)]
    output: Option<PathBuf>,
}

fn history_files(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    if root.is_file() {
        files.push(root.to_owned());
        return Ok(());
    }
    for entry in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            history_files(&path, files)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn run(args: &Args) -> Result<Vec<u8>> {
    let mut events = Vec::<TraceEvent>::new();
    if let Some(trace) = &args.trace {
        events = serde_json::from_slice(
            &fs::read(trace).with_context(|| format!("reading {}", trace.display()))?,
        )
        .with_context(|| format!("parsing {}", trace.display()))?;
    } else if let Some(history) = &args.history {
        let mut files = Vec::new();
        history_files(history, &mut files)?;
        files.sort();
        for path in files {
            events.extend(parse_history(BufReader::new(
                File::open(&path).with_context(|| format!("opening {}", path.display()))?,
            ))?);
        }
    } else {
        bail!("pass --trace or --history");
    }
    serde_json::to_vec_pretty(&evaluate(&events)).map_err(Into::into)
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
        let output = temp.path().join("reports/no-reread.json");

        write_report(&output, b"{}").unwrap();

        assert_eq!(std::fs::read(output).unwrap(), b"{}");
    }
}
