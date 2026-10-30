//! Bump the version in `Cargo.toml` by the project's rules (see `src/version.rs`).
//!
//! Usage: cargo run --example bump_version -- small|major
//!
//! Rewrites the `[package]` version, then refreshes `Cargo.lock` so the
//! two files land in the same commit.

use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{bail, Context, Result};
use rmsync::version::{bump, Bump};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kind = match args.as_slice() {
        [k] => Bump::parse(k),
        _ => None,
    };
    let Some(kind) = kind else {
        eprintln!("usage: cargo run --example bump_version -- small|major");
        return ExitCode::from(2);
    };
    match run(kind) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(kind: Bump) -> Result<()> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = manifest_dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).context("reading Cargo.toml")?;

    // The package version is the only top-level `version = "..."` line;
    // dependency versions sit inside inline tables. `[package]` comes
    // first, so the first match of that line's text is the line itself.
    let Some(current) = text
        .lines()
        .find_map(|line| line.strip_prefix("version = \"")?.strip_suffix('"'))
    else {
        bail!("no package version line found in {}", manifest.display());
    };

    let next = bump(current, kind)?;
    let updated = text.replacen(
        &format!("version = \"{current}\""),
        &format!("version = \"{next}\""),
        1,
    );
    std::fs::write(&manifest, updated).context("writing Cargo.toml")?;

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["update", "--workspace", "--offline"])
        .current_dir(manifest_dir)
        .status()
        .context("running cargo update")?;
    if !status.success() {
        bail!("Cargo.toml now says {next}, but refreshing Cargo.lock failed; run `cargo build` to update it");
    }

    println!("{current} -> {next}");
    Ok(())
}
