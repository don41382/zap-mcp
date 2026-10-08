//! WhatsApp MCP server built on whatsapp-rust: links as a companion device,
//! archives messages to a local SQLite file, and exposes them as MCP tools.
//!
//! One binary, two faces: started by a person (terminal / double-click) it is
//! an interactive installer; spawned by an MCP client (stdio pipes) it is the
//! server.

mod ingest;
mod link;
mod server;
mod setup;
mod store;

use std::fs::File;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use log::{info, warn};
use rmcp::ServiceExt;
use whatsapp_rust::prelude::*;

use crate::server::{Session, WhatsAppServer};
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
    /// Interactive installer: disclaimer, install, add to Claude, link WhatsApp.
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
    /// Run the MCP server on stdio.
    Serve {
        /// Expose the send_message tool.
        #[arg(long)]
        allow_send: bool,
    },
}

pub(crate) struct Paths {
    session_db: PathBuf,
    archive_db: PathBuf,
    lock: PathBuf,
    terms: PathBuf,
}

impl Paths {
    fn new(dir: Option<PathBuf>) -> Result<Self> {
        let dir = match dir {
            Some(d) => d,
            None => default_data_dir()?,
        };
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        restrict_permissions(&dir);
        Ok(Self {
            session_db: dir.join("session.db"),
            archive_db: dir.join("messages.db"),
            lock: dir.join("session.lock"),
            terms: dir.join("terms-accepted.txt"),
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

    // stdout is the MCP channel, so all logging goes to stderr. Keep the
    // library quiet during interactive use; the prompts are the UI.
    let default_filter = match command {
        Command::Serve { .. } => "warn,zap_mcp=info",
        _ => "error",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .target(env_logger::Target::Stderr)
        .init();

    let paths = Paths::new(cli.data_dir.clone())?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        match command {
            Command::Setup => {
                if setup::run(&paths, cli.data_dir.as_deref()).await.is_err() {
                    // Already reported to the user inside the installer.
                    std::process::exit(1);
                }
                Ok(())
            }
            Command::Login { phone, qr } => login(&paths, phone, qr).await,
            Command::Serve { allow_send } => serve(&paths, allow_send).await,
        }
    })
}

async fn login(paths: &Paths, phone: Option<String>, qr: bool) -> Result<()> {
    setup::ensure_terms_accepted(paths)?;
    let Some(_lock) = try_lock(&paths.lock)? else {
        bail!(
            "another zap-mcp process is using the session (probably the MCP server \
             inside Claude). Quit it first, then run login again."
        );
    };
    if let Some(number) = link::linked_number(paths).await? {
        eprintln!(
            "Already linked as {number}. To link another account, remove this device on \
             your phone (Linked Devices) and delete {}.",
            paths.session_db.display()
        );
        return Ok(());
    }
    let method = match (phone, qr) {
        (Some(p), _) => link::Method::Phone(link::normalize_phone(&p).map_err(|e| anyhow!(e))?),
        (None, true) => link::Method::Qr,
        (None, false) => setup::ask_method()?,
    };
    link::link(paths, method, &mut link::Keys::default()).await?;
    eprintln!("Done. Archive: {}", paths.archive_db.display());
    Ok(())
}

async fn serve(paths: &Paths, allow_send: bool) -> Result<()> {
    let store = Store::open(&paths.archive_db)?;
    let lock = try_lock(&paths.lock)?;

    let bot_handle = if lock.is_some() {
        let builder = Bot::builder()
            .with_backend(session_store(paths).await?)
            .on_qr_code(|_, _| async {
                warn!("WhatsApp is not linked; run `zap-mcp setup` in a terminal");
            })
            .on_logged_out(|_| async {
                warn!("WhatsApp session was logged out; run `zap-mcp setup` again");
            });
        let bot = ingest::register(builder, store.clone())
            .build()
            .await
            .map_err(|e| anyhow!("starting WhatsApp client: {e}"))?;
        Some(bot.spawn())
    } else {
        info!("another process owns the WhatsApp session; serving the archive read-only");
        None
    };

    let session = match &bot_handle {
        Some(h) => Session::Live(h.client()),
        None => Session::ReadOnly,
    };
    let service = WhatsAppServer::new(store, session, allow_send)
        .serve(rmcp::transport::stdio())
        .await
        .context("starting MCP server")?;
    let result = service.waiting().await;

    if let Some(handle) = bot_handle {
        handle.shutdown().await;
    }
    drop(lock);
    result.context("MCP server")?;
    Ok(())
}
