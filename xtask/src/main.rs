//! Home Edge build and deployment tasks.
//!
//! Run via the `cargo xtask` alias (defined in `.cargo/config.toml`):
//!
//!   cargo xtask package                                        # build tarball for default target
//!   cargo xtask package --target aarch64-unknown-linux-gnu    # different board
//!   cargo xtask package --zigbee                              # build with Zigbee support
//!   cargo xtask push --tarball home-edge-*.tar.gz             # deploy pre-built release (no Rust needed)
//!   cargo xtask push --tarball … --host ubuntu@myboard.local  # different host
//!   cargo xtask deploy --zigbee --host pi@192.168.1.50        # build with Zigbee + deploy
//!   cargo xtask rollback                                      # restore previous binary on device
//!   cargo xtask screenshot                                    # capture mobile layout screenshots
//!
//! All commands support `--help` for details.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

const DEFAULT_TARGET: &str = "arm-unknown-linux-gnueabihf";
/// SSH ConnectTimeout in seconds (how long to wait for the initial TCP handshake).
const DEFAULT_CONNECT_TIMEOUT: u32 = 10;
/// ServerAliveInterval in seconds — SSH sends a keepalive probe every N seconds.
const DEFAULT_ALIVE_INTERVAL: u32 = 5;
/// How many unanswered keepalive probes before declaring the connection dead.
/// Total stall tolerance = ALIVE_INTERVAL × ALIVE_COUNT  (default: 5×6 = 30 s).
const DEFAULT_ALIVE_COUNT: u32 = 6;

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
        /// Enable Zigbee support (compiles with `--features zigbee`).
        #[arg(long)]
        zigbee: bool,
    },

    /// Push a release tarball to a device via SSH and run the upgrade script.
    ///
    /// No Rust toolchain required — works with pre-built tarballs downloaded
    /// from the releases page.
    ///
    /// Examples:
    ///
    ///   # First-time install or upgrade from a downloaded release:
    ///   cargo xtask push --tarball ~/Downloads/home-edge-arm-unknown-linux-gnueabihf.tar.gz --host pi@192.168.1.50
    Push {
        /// Path to the release tarball.
        #[arg(long)]
        tarball: PathBuf,
        /// SSH destination (user@host), e.g. pi@192.168.1.50 or pi@raspberrypi.local.
        #[arg(long)]
        host: String,
        /// Seconds to wait for the initial SSH/SCP connection to succeed.
        #[arg(long, default_value_t = DEFAULT_CONNECT_TIMEOUT)]
        connect_timeout: u32,
        /// Seconds between SSH keepalive probes once connected.
        /// The transfer is aborted after connect_timeout + alive_interval × alive_count
        /// seconds of total stall.
        #[arg(long, default_value_t = DEFAULT_ALIVE_INTERVAL)]
        alive_interval: u32,
        /// Number of unanswered keepalive probes before the connection is killed.
        #[arg(long, default_value_t = DEFAULT_ALIVE_COUNT)]
        alive_count: u32,
    },

    /// Build from source, package, and push to a device (combines `package` + `push`).
    Deploy {
        /// Rust target triple (see `package --help` for values).
        #[arg(long, default_value = DEFAULT_TARGET)]
        target: String,
        /// Enable Zigbee support (compiles with `--features zigbee`).
        #[arg(long)]
        zigbee: bool,
        /// SSH destination (user@host), e.g. pi@192.168.1.50 or pi@raspberrypi.local.
        #[arg(long)]
        host: String,
        /// Seconds to wait for the initial SSH/SCP connection.
        #[arg(long, default_value_t = DEFAULT_CONNECT_TIMEOUT)]
        connect_timeout: u32,
        /// Seconds between SSH keepalive probes once connected.
        #[arg(long, default_value_t = DEFAULT_ALIVE_INTERVAL)]
        alive_interval: u32,
        /// Number of unanswered keepalive probes before the connection is killed.
        #[arg(long, default_value_t = DEFAULT_ALIVE_COUNT)]
        alive_count: u32,
    },

    /// Restore the previous binary on a device, rolling back the last upgrade.
    ///
    /// Requires that at least one `push` or `deploy` has been run before
    /// (upgrade.sh saves the old binary as home-edge.bak).
    Rollback {
        /// SSH destination (user@host), e.g. pi@192.168.1.50 or pi@raspberrypi.local.
        #[arg(long)]
        host: String,
        /// Seconds to wait for the initial SSH connection.
        #[arg(long, default_value_t = DEFAULT_CONNECT_TIMEOUT)]
        connect_timeout: u32,
        /// Seconds between SSH keepalive probes once connected.
        #[arg(long, default_value_t = DEFAULT_ALIVE_INTERVAL)]
        alive_interval: u32,
        /// Number of unanswered keepalive probes before the connection is killed.
        #[arg(long, default_value_t = DEFAULT_ALIVE_COUNT)]
        alive_count: u32,
    },

    /// Start home-edge with demo data and take Playwright screenshots of every page.
    ///
    /// Requires Node.js and `npx playwright` to be available on PATH.
    /// On first run, install the browser with:
    ///
    ///   npx playwright install chromium
    ///
    /// Screenshots are saved to `--out-dir` (default: screenshots/) along with
    /// an `index.html` gallery.
    Screenshot {
        /// Port the ephemeral demo server will listen on.
        #[arg(long, default_value_t = 8199)]
        port: u16,
        /// Directory to write PNG screenshots and the index.html gallery.
        #[arg(long, default_value = "screenshots")]
        out_dir: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Package { target, zigbee } => {
            package(&target, zigbee)?;
        }
        Cmd::Push { tarball, host, connect_timeout, alive_interval, alive_count } => {
            let opts = SshOpts { connect_timeout, alive_interval, alive_count };
            push(&tarball, &host, &opts)?;
        }
        Cmd::Deploy { target, zigbee, host, connect_timeout, alive_interval, alive_count } => {
            let opts = SshOpts { connect_timeout, alive_interval, alive_count };
            let tarball = package(&target, zigbee)?;
            push(&tarball, &host, &opts)?;
        }
        Cmd::Rollback { host, connect_timeout, alive_interval, alive_count } => {
            let opts = SshOpts { connect_timeout, alive_interval, alive_count };
            rollback(&host, &opts)?;
        }
        Cmd::Screenshot { port, out_dir } => {
            screenshot(port, &out_dir)?;
        }
    }
    Ok(())
}

/// SSH connection and keepalive settings, applied uniformly to scp and ssh.
struct SshOpts {
    /// `-o ConnectTimeout=N` — seconds before the initial TCP handshake fails.
    connect_timeout: u32,
    /// `-o ServerAliveInterval=N` — keepalive probe cadence (seconds).
    alive_interval: u32,
    /// `-o ServerAliveCountMax=N` — probes before declaring the connection dead.
    alive_count: u32,
}

impl SshOpts {
    /// Returns the `-o Key=Value` arguments to pass to ssh or scp.
    fn args(&self) -> Vec<String> {
        vec![
            format!("-o ConnectTimeout={}", self.connect_timeout),
            format!("-o ServerAliveInterval={}", self.alive_interval),
            format!("-o ServerAliveCountMax={}", self.alive_count),
        ]
    }
}

// ---------------------------------------------------------------------------
// Task implementations
// ---------------------------------------------------------------------------

/// Cross-compiles home-edge for `target`, assembles a deployment tarball,
/// and returns its path.
fn package(target: &str, zigbee: bool) -> Result<PathBuf> {
    let root = workspace_root();

    // 1. Cross-compile.
    let feature_label = if zigbee { " +zigbee" } else { "" };
    eprintln!("Building home-edge for {target}{feature_label}...");
    let mut cmd = Command::new("cargo");
    let mut args = vec!["build", "--release", "--target", target, "-p", "home-edge"];
    if zigbee {
        args.extend(["--features", "zigbee"]);
    }
    cmd.args(&args).current_dir(&root);
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
fn push(tarball: &Path, host: &str, opts: &SshOpts) -> Result<()> {
    if !tarball.exists() {
        bail!(
            "tarball not found: {}\n\n\
             Download a pre-built release from the releases page, then:\n\n  \
             cargo xtask push --tarball path/to/home-edge-<target>.tar.gz --host {host}",
            tarball.display()
        );
    }

    eprintln!("Copying {} → {host}:/var/tmp/home-edge-update.tar.gz...", tarball.display());
    let mut cmd = Command::new("scp");
    for o in opts.args() {
        cmd.arg(o);
    }
    cmd.arg(tarball).arg(format!("{host}:/var/tmp/home-edge-update.tar.gz"));
    run(&mut cmd)?;

    eprintln!("Running upgrade on {host}...");
    let mut cmd = Command::new("ssh");
    for o in opts.args() {
        cmd.arg(o);
    }
    cmd.arg(host).arg(
        "mkdir -p /var/tmp/home-edge-update && \
         tar -xzf /var/tmp/home-edge-update.tar.gz -C /var/tmp/home-edge-update && \
         sudo sh /var/tmp/home-edge-update/upgrade.sh && \
         rm -rf /var/tmp/home-edge-update /var/tmp/home-edge-update.tar.gz",
    );
    run(&mut cmd)?;

    Ok(())
}

/// SSH into `host` and restore the `.bak` binary saved by `upgrade.sh`.
fn rollback(host: &str, opts: &SshOpts) -> Result<()> {
    eprintln!("Rolling back on {host}...");
    let mut cmd = Command::new("ssh");
    for o in opts.args() {
        cmd.arg(o);
    }
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
// Screenshot
// ---------------------------------------------------------------------------

/// Build home-edge with `--features zigbee`, start it with `--demo`, run the
/// Playwright script, and kill the server when done.
fn screenshot(port: u16, out_dir: &Path) -> Result<()> {
    let root = workspace_root();

    // 1. Build a debug binary (faster than release for local use).
    eprintln!("Building home-edge (debug, +zigbee)...");
    let mut build = Command::new("cargo");
    build
        .args(["build", "--features", "zigbee", "-p", "home-edge"])
        .current_dir(&root);
    run(&mut build)?;

    // 2. Write a temp config that uses a throwaway data dir and the chosen port.
    let tmp_dir = std::env::temp_dir().join("home-edge-screenshot");
    std::fs::create_dir_all(&tmp_dir).context("creating temp data dir")?;
    let cfg_path = tmp_dir.join("config.toml");
    let data_dir = tmp_dir.join("data");
    std::fs::create_dir_all(&data_dir).context("creating temp data dir")?;
    let cfg_content = format!(
        "[server]\nhost = \"127.0.0.1\"\nport = {port}\n\n[storage]\ndata_dir = \"{}\"\n\n[ui]\nproduct_name = \"Home Edge\"\n",
        data_dir.display()
    );
    std::fs::write(&cfg_path, cfg_content).context("writing temp config")?;

    // 3. Start the server in the background.
    let binary = root.join("target/debug/home-edge");
    eprintln!("Starting demo server on port {port}...");
    let mut server = std::process::Command::new(&binary)
        .arg("--config")
        .arg(&cfg_path)
        .arg("--demo")
        .spawn()
        .with_context(|| format!("failed to spawn {}", binary.display()))?;

    // 4. Wait until the server is ready (poll /health, up to 15 s).
    let base = format!("http://127.0.0.1:{port}");
    eprintln!("Waiting for server to be ready...");
    let ready = wait_for_server(&base, 15);

    // 5. Run the Playwright script (even if server didn't respond, to clean up).
    let script_result = if ready {
        let abs_out = if out_dir.is_relative() { root.join(out_dir) } else { out_dir.to_owned() };
        std::fs::create_dir_all(&abs_out).context("creating out-dir")?;
        let script = root.join("scripts/screenshot.js");
        eprintln!("Running Playwright screenshot script...");
        let mut node = Command::new("node");
        node.arg(&script)
            .env("SCREENSHOT_BASE_URL", &base)
            .env("SCREENSHOT_OUT_DIR", abs_out.to_str().context("out-dir not valid UTF-8")?);
        let status = node.status().context("failed to launch node")?;
        if status.success() {
            eprintln!("Screenshots written to {}", out_dir.display());
            eprintln!("Open {}/index.html to view the gallery.", out_dir.display());
            Ok(())
        } else {
            bail!("Playwright script exited with {status}");
        }
    } else {
        bail!("Server did not become ready within 15 seconds");
    };

    // 6. Kill the server regardless of outcome.
    let _ = server.kill();
    let _ = server.wait();

    script_result
}

/// Poll `{base}/` until an HTTP 200 (or 3xx) is received, or `timeout_secs` elapses.
fn wait_for_server(base: &str, timeout_secs: u64) -> bool {
    let url = format!("{base}/");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        let Ok(status) = ureq::get(&url).call().map(|r| r.status()).or_else(|e| {
            // ureq treats redirects as errors in some versions; treat 3xx as ready.
            if let ureq::Error::Status(code, _) = &e {
                Ok(*code)
            } else {
                Err(e)
            }
        }) else {
            std::thread::sleep(std::time::Duration::from_millis(300));
            continue;
        };
        if status < 500 {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    false
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
