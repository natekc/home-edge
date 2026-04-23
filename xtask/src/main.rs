//! Home Edge build and deployment tasks.
//!
//! Run via the `cargo xtask` alias (defined in `.cargo/config.toml`):
//!
//!   cargo xtask package                                        # build tarball for default target
//!   cargo xtask package --target aarch64-unknown-linux-gnu    # different board
//!   cargo xtask push --tarball home-edge-*.tar.gz             # deploy pre-built release (no Rust needed)
//!   cargo xtask push --tarball … --host ubuntu@myboard.local  # different host
//!   cargo xtask deploy                                        # build + package + push
//!   cargo xtask rollback                                      # restore previous binary on device
//!
//! All commands support `--help` for details.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

const DEFAULT_TARGET: &str = "arm-unknown-linux-gnueabihf";
const DEFAULT_HOST: &str = "pi@raspberrypi.local";

#[derive(Parser)]
#[command(
    name = "cargo xtask",
    about = "Home Edge build and deployment tasks",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Cross-compile home-edge and assemble a self-contained deployment tarball.
    ///
    /// Produces home-edge-<target>.tar.gz containing the binary, config,
    /// systemd unit, and install/upgrade scripts — everything needed to deploy
    /// on a device with no internet access.
    Package {
        /// Rust target triple.
        ///
        /// Common values:
        ///   arm-unknown-linux-gnueabihf   Pi Zero W / Pi 1 (ARMv6)
        ///   armv7-unknown-linux-gnueabihf Pi 2/3/4 32-bit
        ///   aarch64-unknown-linux-gnu     Pi 3/4/5 64-bit, most SBCs
        ///   riscv64gc-unknown-linux-gnu   RISC-V 64-bit
        #[arg(long, default_value = DEFAULT_TARGET)]
        target: String,
    },

    /// Push a release tarball to a device via SSH and run the upgrade script.
    ///
    /// No Rust toolchain required — works with pre-built tarballs downloaded
    /// from the releases page.
    ///
    /// Examples:
    ///
    ///   # First-time install or upgrade from a downloaded release:
    ///   cargo xtask push --tarball ~/Downloads/home-edge-arm-unknown-linux-gnueabihf.tar.gz
    ///
    ///   # Override host:
    ///   cargo xtask push --tarball … --host pi@192.168.1.50
    Push {
        /// Path to the release tarball.
        #[arg(long)]
        tarball: PathBuf,
        /// SSH destination (user@host).
        #[arg(long, default_value = DEFAULT_HOST)]
        host: String,
    },

    /// Build from source, package, and push to a device (combines `package` + `push`).
    Deploy {
        /// Rust target triple (see `package --help` for values).
        #[arg(long, default_value = DEFAULT_TARGET)]
        target: String,
        /// SSH destination (user@host).
        #[arg(long, default_value = DEFAULT_HOST)]
        host: String,
    },

    /// Restore the previous binary on a device, rolling back the last upgrade.
    ///
    /// Requires that at least one `push` or `deploy` has been run before
    /// (upgrade.sh saves the old binary as home-edge.bak).
    Rollback {
        /// SSH destination (user@host).
        #[arg(long, default_value = DEFAULT_HOST)]
        host: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Package { target } => {
            package(&target)?;
        }
        Cmd::Push { tarball, host } => push(&tarball, &host)?,
        Cmd::Deploy { target, host } => {
            let tarball = package(&target)?;
            push(&tarball, &host)?;
        }
        Cmd::Rollback { host } => rollback(&host)?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Task implementations
// ---------------------------------------------------------------------------

/// Cross-compiles home-edge for `target`, assembles a deployment tarball,
/// and returns its path.
fn package(target: &str) -> Result<PathBuf> {
    let root = workspace_root();

    // 1. Cross-compile.
    eprintln!("Building home-edge for {target}...");
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--release", "--target", target, "-p", "home-edge"])
        .current_dir(&root);
    run(&mut cmd)?;

    // 2. Assemble dist/.
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).context("creating dist/")?;

    let bin = root.join("target").join(target).join("release").join("home-edge");
    copy(&bin,                                               &dist.join("home-edge"))?;
    copy(&root.join("config/default.toml"),                  &dist.join("default.toml"))?;
    copy(&root.join("systemd/home-edge.service"),            &dist.join("home-edge.service"))?;
    copy(&root.join("scripts/install.sh"),                   &dist.join("install.sh"))?;
    copy(&root.join("scripts/upgrade.sh"),                   &dist.join("upgrade.sh"))?;
    set_executable(&dist.join("install.sh"))?;
    set_executable(&dist.join("upgrade.sh"))?;

    // 3. Pack tarball.
    let tarball = root.join(format!("home-edge-{target}.tar.gz"));
    eprintln!("Assembling {}...", tarball.display());
    let mut cmd = Command::new("tar");
    cmd.args([
        "-czf",
        tarball.to_str().context("tarball path is not valid UTF-8")?,
        "-C",
        dist.to_str().context("dist path is not valid UTF-8")?,
        ".",
    ]);
    run(&mut cmd)?;

    eprintln!("Package ready: {}", tarball.display());
    Ok(tarball)
}

/// Transfer `tarball` to `host` via SCP and run the upgrade script there.
fn push(tarball: &Path, host: &str) -> Result<()> {
    if !tarball.exists() {
        bail!(
            "tarball not found: {}\n\n\
             Download a pre-built release from the releases page, then:\n\n  \
             cargo xtask push --tarball path/to/home-edge-<target>.tar.gz --host {host}",
            tarball.display()
        );
    }

    eprintln!("Copying {} → {host}:/tmp/home-edge-update.tar.gz...", tarball.display());
    let mut cmd = Command::new("scp");
    cmd.arg(tarball).arg(format!("{host}:/tmp/home-edge-update.tar.gz"));
    run(&mut cmd)?;

    eprintln!("Running upgrade on {host}...");
    let mut cmd = Command::new("ssh");
    cmd.arg(host).arg(
        "mkdir -p /tmp/home-edge-update && \
         tar -xzf /tmp/home-edge-update.tar.gz -C /tmp/home-edge-update && \
         sudo sh /tmp/home-edge-update/upgrade.sh && \
         rm -rf /tmp/home-edge-update /tmp/home-edge-update.tar.gz",
    );
    run(&mut cmd)?;

    Ok(())
}

/// SSH into `host` and restore the `.bak` binary saved by `upgrade.sh`.
fn rollback(host: &str) -> Result<()> {
    eprintln!("Rolling back on {host}...");
    let mut cmd = Command::new("ssh");
    cmd.arg(host).arg(
        "if [ -f /usr/local/bin/home-edge.bak ]; then \
           sudo systemctl stop home-edge.service 2>/dev/null || true; \
           sudo cp /usr/local/bin/home-edge.bak /usr/local/bin/home-edge; \
           sudo systemctl start home-edge.service 2>/dev/null || true; \
           echo 'Rolled back to previous binary'; \
         else \
           echo 'No backup found — rollback not possible'; exit 1; \
         fi",
    );
    run(&mut cmd)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Returns the workspace root directory.
///
/// `CARGO_MANIFEST_DIR` is set by cargo to the xtask crate directory
/// (`<workspace>/xtask`); its parent is the workspace root.
fn workspace_root() -> PathBuf {
    std::env::var("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR not set — run via `cargo xtask`")
        .parent()
        .expect("xtask manifest has no parent directory")
        .to_owned()
}

fn copy(src: &Path, dst: &Path) -> Result<()> {
    std::fs::copy(src, dst)
        .with_context(|| format!("copy {} → {}", src.display(), dst.display()))?;
    Ok(())
}

fn set_executable(_path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(_path)
            .with_context(|| format!("stat {}", _path.display()))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(_path, perms)
            .with_context(|| format!("chmod +x {}", _path.display()))?;
    }
    Ok(())
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to launch {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} exited with {status}", cmd.get_program());
    }
    Ok(())
}
