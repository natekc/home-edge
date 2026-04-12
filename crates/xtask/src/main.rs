use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

const IMAGE: &str = "home-edge-build";
const IMAGE_SMOKE: &str = "home-edge-build-smoke";
const CONTAINER: &str = "home-edge-dist";
const TARGET: &str = "arm-unknown-linux-gnueabihf";
const CROSS_LINKER: &str = "arm-linux-gnueabihf-gcc";

/// home-edge build tasks
#[derive(Parser)]
#[command(name = "xtask", about = "home-edge build tasks")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cross-compile for arm-unknown-linux-gnueabihf.
    /// Uses native cargo on Linux when the cross-linker is available,
    /// otherwise falls back to Docker.
    Cross,
    /// Build via Docker and extract dist artifacts to ./dist/
    Dist,
    /// Build the smoke-test Docker image and run it
    DockerSmoke,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Cmd::Cross => cmd_cross(),
        Cmd::Dist => cmd_dist(),
        Cmd::DockerSmoke => cmd_docker_smoke(),
    }
}

// ---------------------------------------------------------------------------
// Task implementations
// ---------------------------------------------------------------------------

fn cmd_cross() -> Result<()> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    if std::env::consts::OS == "linux" && which::which(CROSS_LINKER).is_ok() {
        eprintln!("[xtask] {CROSS_LINKER} found — using native cargo cross-compile");
        run(&cargo, &["build", "--release", "--target", TARGET, "-p", "home-edge"])
    } else {
        eprintln!(
            "[xtask] {CROSS_LINKER} not available on this host — building via Docker instead"
        );
        docker_cross()
    }
}

fn cmd_dist() -> Result<()> {
    docker_cross()?;
    fs::create_dir_all("dist").context("failed to create dist/")?;

    // Evict any stale container from a prior aborted run (ignore failure; nothing may exist).
    let _ = Command::new("docker")
        .args(["rm", "-f", CONTAINER])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // Always remove the staging container, even on error.
    let copy_result = (|| -> Result<()> {
        run("docker", &["create", "--name", CONTAINER, IMAGE])?;
        run("docker", &["cp", &format!("{CONTAINER}:/out/."), "dist/"])
    })();

    let rm_result = run("docker", &["rm", CONTAINER]);

    // copy error takes priority; rm error only surfaces if copy succeeded.
    copy_result?;
    eprintln!("[xtask] artifacts written to dist/");
    rm_result.context("container copy succeeded; cleanup of staging container failed")?;
    Ok(())
}

fn cmd_docker_smoke() -> Result<()> {
    run(
        "docker",
        &[
            "build",
            "-f",
            "docker/Dockerfile.build",
            "--target",
            "smoke",
            "-t",
            IMAGE_SMOKE,
            ".",
        ],
    )?;
    run("docker", &["run", "--rm", IMAGE_SMOKE])
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn docker_cross() -> Result<()> {
    run(
        "docker",
        &[
            "build",
            "-f",
            "docker/Dockerfile.build",
            "--target",
            "build",
            "-t",
            IMAGE,
            ".",
        ],
    )
}

/// Run `program` with `args`, inheriting stdio. Returns an error if the
/// process exits with a non-zero status.
fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `{program}`"))?;

    if !status.success() {
        bail!(
            "`{program} {}` exited with status {status}",
            args.join(" ")
        );
    }
    Ok(())
}
