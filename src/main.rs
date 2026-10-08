//! WhatsApp MCP server built on whatsapp-rust: links as a companion device,
//! archives messages to a local SQLite file, and exposes them as MCP tools.
//!
//! One binary, three faces: started by a person (terminal / double-click) it
//! is an interactive installer; `daemon` is the single background process that
//! owns the WhatsApp connection and serves MCP over local HTTP; spawned by an
//! MCP client over stdio (`serve`) it relays to that daemon.

mod daemon;
mod ingest;
mod link;
mod proxy;
mod server;
mod setup;
mod store;

use std::fs::File;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use log::{info, warn};
use whatsapp_rust::prelude::*;

use crate::store::Store;

#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "Without a command: runs `setup` when started from a terminal, \
                  `serve` when started by an MCP client."
)]
struct Cli {
    /// Where the session keys and message archive live
    /// (default: ~/.zap-mcp).
    #[arg(long, env = "ZAP_MCP_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive installer: disclaimer, install, background service, add to
    /// Claude, link WhatsApp.
    Setup,
    /// Only link WhatsApp (QR code or pairing code) and download history.
    Login {
        /// Link with a pairing code for this number (with country code)
        /// instead of asking.
        #[arg(long, short)]
        phone: Option<String>,
        /// Link by scanning a QR code instead of asking.
        #[arg(long, conflicts_with = "phone")]
        qr: bool,
    },
    /// Background service: WhatsApp connection + MCP over HTTP on 127.0.0.1.
    Daemon {
        /// Expose the send_message tool (also configurable in settings.json).
        #[arg(long)]
        allow_send: bool,
        /// Start the daemon in the background and return immediately.
        #[arg(long)]
        detach: bool,
    },
    /// MCP over stdio, relayed to the daemon (for clients like Claude Desktop).
    Serve {
        /// Deprecated: stored in settings.json and applied by the daemon.
        #[arg(long, hide = true)]
        allow_send: bool,
    },
    /// Show whether the background service runs and WhatsApp is linked.
    Status,
    /// Stop the background service, remove start-at-login and the Claude
    /// registrations. Keeps your data.
    Uninstall,
}

pub(crate) struct Paths {
    dir: PathBuf,
    /// Whether the user chose `dir` (--data-dir / env). Only then is it
    /// written into launch configs; otherwise they resolve the default.
    custom: bool,
    /// Held by the one daemon process (separate from the WhatsApp session
    /// lock, which it may still be waiting for).
    daemon_lock: PathBuf,
    /// Present while the daemon waits for another process to release the
    /// WhatsApp session.
    waiting: PathBuf,
    session_db: PathBuf,
    archive_db: PathBuf,
    lock: PathBuf,
    terms: PathBuf,
    settings: PathBuf,
    pid: PathBuf,
    log: PathBuf,
}

impl Paths {
    fn new(dir: Option<PathBuf>) -> Result<Self> {
        let home = dirs::home_dir();
        // Launch configs written before the rename may still name the old
        // default directory explicitly; treat that as "default" so the
        // migration applies to them too.
        let legacy = home.as_ref().map(|h| h.join(".whatsapp-rust-mcp"));
        let (dir, custom) = match dir {
            Some(d) if Some(&d) == legacy.as_ref() => (default_data_dir()?, false),
            Some(d) => (d, true),
            None => (default_data_dir()?, false),
        };
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        restrict_permissions(&dir);
        Ok(Self {
            session_db: dir.join("session.db"),
            archive_db: dir.join("messages.db"),
            lock: dir.join("session.lock"),
            terms: dir.join("terms-accepted.txt"),
            settings: dir.join("settings.json"),
            pid: dir.join("daemon.pid"),
            log: dir.join("daemon.log"),
            daemon_lock: dir.join("daemon.lock"),
            waiting: dir.join("daemon.waiting"),
            dir,
            custom,
        })
    }
}

/// `~/.zap-mcp`, moving the data over from the pre-rename `~/.whatsapp-rust-mcp`
/// the first time. The move only happens while no process holds the old
/// session lock (moving files out from under a running server would make it
/// recreate empty databases); until then the old directory keeps being used.
fn default_data_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine home directory; pass --data-dir")?;
    let dir = home.join(".zap-mcp");
    let legacy = home.join(".whatsapp-rust-mcp");
    if dir.exists() || !legacy.exists() {
        return Ok(dir);
    }
    match try_lock(&legacy.join("session.lock")) {
        Ok(Some(lock)) => {
            drop(lock);
            match std::fs::rename(&legacy, &dir) {
                Ok(()) => {
                    info!("moved data from {} to {}", legacy.display(), dir.display());
                    Ok(dir)
                }
                Err(e) => {
                    warn!("could not move {} to {}: {e}", legacy.display(), dir.display());
                    Ok(legacy)
                }
            }
        }
        _ => Ok(legacy),
    }
}

/// The directory holds the device's encryption keys and private messages.
#[cfg(unix)]
fn restrict_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// On Windows the user profile directory is already private to the user.
#[cfg(not(unix))]
fn restrict_permissions(_dir: &Path) {}

/// Only one process may drive the WhatsApp session at a time; a second
/// connection would kick the first one off ("stream replaced").
pub(crate) fn try_lock(path: &Path) -> Result<Option<File>> {
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

pub(crate) async fn session_store(paths: &Paths) -> Result<SqliteStore> {
    let url = paths
        .session_db
        .to_str()
        .context("data dir path is not valid UTF-8")?;
    SqliteStore::new(url)
        .await
        .map_err(|e| anyhow!("opening session store: {e}"))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let command = cli.command.unwrap_or(if interactive {
        Command::Setup
    } else {
        Command::Serve { allow_send: false }
    });

    // stdout is the MCP channel for `serve`, so all logging goes to stderr
    // (the daemon's stderr is its log file). Keep the library quiet during
    // interactive use; the prompts are the UI.
    let default_filter = match command {
        Command::Serve { .. } | Command::Daemon { .. } => "warn,zap_mcp=info",
        _ => "error",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .target(env_logger::Target::Stderr)
        .init();

    let paths = Paths::new(cli.data_dir.clone())?;
    if let Command::Daemon { detach: true, .. } = command {
        let exe = std::env::current_exe().context("locating zap-mcp")?;
        return daemon::spawn_detached(&paths, &exe);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        match command {
            Command::Setup => {
                if setup::run(&paths).await.is_err() {
                    // Already reported to the user inside the installer.
                    std::process::exit(1);
                }
                Ok(())
            }
            Command::Login { phone, qr } => login(&paths, phone, qr).await,
            Command::Daemon { allow_send, .. } => daemon::run(&paths, allow_send).await,
            Command::Serve { allow_send } => proxy::run(paths, allow_send).await,
            Command::Status => status(&paths).await,
            Command::Uninstall => setup::uninstall(&paths).await,
        }
    })
}

async fn login(paths: &Paths, phone: Option<String>, qr: bool) -> Result<()> {
    setup::ensure_terms_accepted(paths)?;
    let was_running = daemon::stop(paths).await?;
    let Some(lock) = try_lock(&paths.lock)? else {
        anyhow::bail!(
            "another zap-mcp process is using the session. Quit Claude completely, then \
             run login again."
        );
    };
    let result = async {
        if let Some(number) = link::linked_number(paths).await? {
            eprintln!(
                "Already linked as {number}. To link another account, remove this device on \
                 your phone (Linked Devices) and delete {}.",
                paths.session_db.display()
            );
            return Ok(());
        }
        let method = match (phone, qr) {
            (Some(p), _) => {
                link::Method::Phone(link::normalize_phone(&p).map_err(|e| anyhow!(e))?)
            }
            (None, true) => link::Method::Qr,
            (None, false) => setup::ask_method()?,
        };
        link::link(paths, method, &mut link::Keys::default()).await?;
        eprintln!("Done. Archive: {}", paths.archive_db.display());
        Ok(())
    }
    .await;
    drop(lock);
    if was_running || daemon::autostart::is_installed() {
        daemon::start(paths)?;
    }
    result
}

async fn status(paths: &Paths) -> Result<()> {
    let settings = daemon::Settings::load_or_create(paths)?;
    match daemon::health(&settings).await {
        Some(h) => println!(
            "background service: running (v{}) at {}\nWhatsApp: {}",
            h.version,
            settings.url(),
            match (h.linked, h.connected) {
                (false, _) => "not linked (run `zap-mcp setup`)",
                (true, true) => "linked, connected",
                (true, false) => "linked, connecting...",
            }
        ),
        None if paths.waiting.exists() => {
            println!("background service: {}", daemon::not_running_reason(paths))
        }
        None => println!("background service: not running"),
    }
    println!(
        "start at login: {}",
        if daemon::autostart::is_installed() { "yes" } else { "no" }
    );
    println!(
        "send_message: {}",
        if settings.allow_send { "allowed" } else { "off" }
    );
    if let Ok(store) = Store::open(&paths.archive_db)
        && let Ok(s) = store.stats()
    {
        println!("archive: {} chats, {} messages", s.chats, s.messages);
    }
    println!("data: {}", paths.dir.display());
    Ok(())
}
