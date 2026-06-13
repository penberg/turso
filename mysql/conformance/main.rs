// Copyright 2023-2026 the Turso authors. All rights reserved. MIT license.

//! Conformance test runner for the Turso MySQL front-end.
//!
//! It reads `.test` files (see [`runner`] for the format) and executes them
//! against a MySQL endpoint over the wire using the `mysql` client crate. Point
//! it at the Turso MySQL server or at a real `mysqld` with `--url` (or the
//! `MYSQL_TEST_URL` environment variable):
//!
//! ```text
//! turso-mysql-conformance --url mysql://root@127.0.0.1:3306/ mysql/conformance/tests
//! ```

mod runner;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use mysql::{Conn, Opts};

use runner::Outcome;

/// Default endpoint when neither `--url` nor `MYSQL_TEST_URL` is set.
const DEFAULT_URL: &str = "mysql://root@127.0.0.1:3306/";

#[derive(Parser, Debug)]
#[command(
    name = "turso-mysql-conformance",
    about = "Run MySQL conformance tests over the wire"
)]
struct Args {
    /// Test files or directories to run (directories are searched for `*.test`).
    #[arg(default_value = "mysql/conformance/tests")]
    paths: Vec<String>,

    /// MySQL connection URL. Falls back to $MYSQL_TEST_URL, then a localhost default.
    #[arg(long)]
    url: Option<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Runs every discovered test file; returns `true` if all of them passed.
fn run(args: &Args) -> Result<bool> {
    let url = args
        .url
        .clone()
        .or_else(|| std::env::var("MYSQL_TEST_URL").ok())
        .unwrap_or_else(|| DEFAULT_URL.to_string());

    let mut files = Vec::new();
    for path in &args.paths {
        collect_tests(Path::new(path), &mut files)
            .with_context(|| format!("discovering tests under {path}"))?;
    }
    files.sort();
    if files.is_empty() {
        anyhow::bail!("no .test files found in: {}", args.paths.join(", "));
    }

    println!("running {} test file(s) against {url}\n", files.len());

    let opts = Opts::from_url(&url).with_context(|| format!("parsing connection URL {url}"))?;

    let mut total_pass = 0usize;
    let mut total_fail = 0usize;
    for file in &files {
        let (passed, failed) = run_file(file, &opts);
        total_pass += passed;
        total_fail += failed;
    }

    println!("\n{total_pass} passed, {total_fail} failed");
    Ok(total_fail == 0)
}

/// Runs a single file and returns `(passed, failed)` record counts.
fn run_file(file: &Path, opts: &Opts) -> (usize, usize) {
    let display = file.display();
    let content = match std::fs::read_to_string(file) {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL {display}: cannot read file: {e}");
            return (0, 1);
        }
    };
    let records = match runner::parse(&content) {
        Ok(r) => r,
        Err(e) => {
            println!("FAIL {display}: parse error: {e}");
            return (0, 1);
        }
    };

    // A fresh connection per file keeps session state isolated.
    let mut conn = match Conn::new(opts.clone()) {
        Ok(c) => c,
        Err(e) => {
            println!("FAIL {display}: cannot connect: {e}");
            return (0, records.len().max(1));
        }
    };

    let outcomes = runner::run(&mut conn, &records);
    let mut passed = 0;
    let mut failed = 0;
    let mut failures = Vec::new();
    for outcome in &outcomes {
        match outcome {
            Outcome::Pass => passed += 1,
            Outcome::Fail { line, message } => {
                failed += 1;
                failures.push((*line, message.clone()));
            }
        }
    }

    if failed == 0 {
        println!("ok   {display} ({passed} passed)");
    } else {
        println!("FAIL {display} ({passed} passed, {failed} failed)");
        for (line, message) in failures {
            // Indent the (possibly multi-line) message under its location.
            let indented = message.replace('\n', "\n      ");
            println!("  {display}:{line}: {indented}");
        }
    }
    (passed, failed)
}

/// Recursively collects `*.test` files under `path` (or `path` itself if it is one).
fn collect_tests(path: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let meta =
        std::fs::metadata(path).with_context(|| format!("{} does not exist", path.display()))?;
    if meta.is_file() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        if child.is_dir() {
            collect_tests(&child, out)?;
        } else if child.extension().is_some_and(|e| e == "test") {
            out.push(child);
        }
    }
    Ok(())
}
