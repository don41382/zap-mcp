//! The background service: owns the WhatsApp connection and serves MCP over
//! HTTP on 127.0.0.1, so every Claude window shares one process (and messages
//! keep arriving while Claude is closed).

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use log::{error, info, warn};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use whatsapp_rust::prelude::*;

use crate::server::WhatsAppServer;
use crate::store::Store;
use crate::{Paths, ingest, session_store, try_lock};

const PREFERRED_PORT: u16 = 47821;
const HEALTH_NAME: &str = "zap-mcp";

/// Persistent daemon configuration (`settings.json` in the data dir).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub port: u16,
    /// Bearer token every MCP request must carry. Keeps other local programs
    /// and web pages (DNS rebinding, CSRF) away from the messages.
    pub token: String,
    pub allow_send: bool,
}

impl Settings {
    pub fn load_or_create(paths: &Paths) -> Result<Self> {
        if let Ok(text) = std::fs::read_to_string(&paths.settings) {
            return serde_json::from_str(&text)
                .with_context(|| format!("reading {}", paths.settings.display()));
        }
        let settings = Self {
            port: pick_port(),
            token: random_token()?,
            allow_send: false,
        };
        settings.save(paths)?;
        Ok(settings)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let text = serde_json::to_string_pretty(self)? + "\n";
        write_private(&paths.settings, &text)
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn auth_header(&self) -> String {
        format!("Authorization: Bearer {}", self.token)
    }
}

/// The preferred port if it's free right now, otherwise any free port. Chosen
/// once and persisted, so the URL registered in Claude stays valid.
fn pick_port() -> u16 {
    if std::net::TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).is_ok() {
        return PREFERRED_PORT;
    }
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(PREFERRED_PORT)
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("generating token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_token(State(token): State<Arc<str>>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match presented {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => next.run(req).await,
        _ => (StatusCode::UNAUTHORIZED, "missing or invalid token").into_response(),
    }
}

/// What `/health` reports. No message content or phone number.
#[derive(Debug, Serialize, Deserialize)]
pub struct Health {
    pub name: String,
    pub version: String,
    pub linked: bool,
    pub connected: bool,
}

/// Ask a running daemon for its health; `None` if nothing (of ours) answers.
pub async fn health(settings: &Settings) -> Option<Health> {
    let url = format!("http://127.0.0.1:{}/health", settings.port);
    let resp = reqwest::Client::new()
        .get(url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    let body = resp.bytes().await.ok()?;
    let health: Health = serde_json::from_slice(&body).ok()?;
    (health.name == HEALTH_NAME).then_some(health)
}

/// Run the daemon in the foreground until SIGINT/SIGTERM.
pub async fn run(paths: &Paths, allow_send_flag: bool) -> Result<()> {
    let settings = Settings::load_or_create(paths)?;
    let allow_send = settings.allow_send || allow_send_flag;

    // Only one daemon process at a time, whether it's serving or still
    // waiting for the session below. Later starts (launchd, relays) just exit.
    let Some(_instance) = try_lock(&paths.daemon_lock)? else {
        info!("another zap-mcp daemon is already running or starting");
        return Ok(());
    };

    // Wait for the session lock, which a pre-daemon stdio server (old
    // version, one per Claude window) may still hold.
    let mut announced = false;
    let lock = loop {
        if let Some(lock) = try_lock(&paths.lock)? {
            break lock;
        }
        if !announced {
            warn!(
                "waiting for another process to release the WhatsApp session \
                 (an older zap-mcp / whatsapp-rust-mcp, probably in a Claude window)"
            );
            std::fs::write(&paths.waiting, "").ok();
            announced = true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    };
    let _ = std::fs::remove_file(&paths.waiting);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", settings.port))
        .await
        .with_context(|| {
            format!(
                "port {} is in use by another program; change \"port\" in {}",
                settings.port,
                paths.settings.display()
            )
        })?;
    std::fs::write(&paths.pid, std::process::id().to_string()).ok();

    let store = Store::open(&paths.archive_db)?;
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
    let handle = bot.spawn();
    let client = handle.client();

    let shutdown = CancellationToken::new();
    let mcp = {
        let (store, client) = (store.clone(), client.clone());
        StreamableHttpService::new(
            move || Ok(WhatsAppServer::new(store.clone(), client.clone(), allow_send)),
            Arc::new(LocalSessionManager::default()),
            // Loopback Host only (default) and no browser Origin at all.
            StreamableHttpServerConfig::default()
                .enforce_origin_validation()
                .with_cancellation_token(shutdown.child_token()),
        )
    };
    let token: Arc<str> = settings.token.clone().into();
    let health_client = client.clone();
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .layer(axum::middleware::from_fn_with_state(token, require_token))
        .route(
            "/health",
            axum::routing::get(move || {
                let client = health_client.clone();
                async move {
                    axum::Json(Health {
                        name: HEALTH_NAME.into(),
                        version: env!("CARGO_PKG_VERSION").into(),
                        linked: client.pn().is_some(),
                        connected: client.is_connected(),
                    })
                }
            }),
        );

    info!(
        "zap-mcp daemon {} listening on {} (send_message {})",
        env!("CARGO_PKG_VERSION"),
        settings.url(),
        if allow_send { "enabled" } else { "disabled" }
    );
    let server_shutdown = shutdown.clone();
    let served = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            info!("shutting down");
            server_shutdown.cancel();
        })
        .await;

    shutdown.cancel();
    handle.shutdown().await;
    let _ = std::fs::remove_file(&paths.pid);
    drop(lock);
    served.context("HTTP server")?;
    Ok(())
}

/// `--data-dir <dir>` for commands we launch, only if the user chose the dir.
pub fn data_dir_args(paths: &Paths) -> Vec<String> {
    if paths.custom {
        vec!["--data-dir".into(), paths.dir.display().to_string()]
    } else {
        Vec::new()
    }
}

/// Why the daemon isn't answering, for error messages.
pub fn not_running_reason(paths: &Paths) -> String {
    if paths.waiting.exists() && try_lock(&paths.daemon_lock).is_ok_and(|l| l.is_none()) {
        format!(
            "the zap-mcp background service is waiting for an older zap-mcp / whatsapp-rust-mcp \
             process to release WhatsApp. Quit every Claude window that still uses the old \
             version (or run `pkill -f whatsapp-rust-mcp`); it then starts by itself. Log: {}",
            paths.log.display()
        )
    } else {
        format!(
            "the zap-mcp background service did not start. See {}",
            paths.log.display()
        )
    }
}

/// Start the daemon as a detached background process (no console window).
pub fn spawn_detached(paths: &Paths, exe: &Path) -> Result<()> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)
        .with_context(|| format!("opening {}", paths.log.display()))?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .args(data_dir_args(paths))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group: survives the terminal / Claude that started it.
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    cmd.spawn().context("starting the zap-mcp daemon")?;
    Ok(())
}

/// Start the daemon in the background: through launchd when the LaunchAgent
/// is installed (so it stays managed), otherwise as a detached process.
pub fn start(paths: &Paths) -> Result<()> {
    start_with(paths, &std::env::current_exe().context("locating zap-mcp")?)
}

/// Like [`start`], with `exe` as the program for a detached start.
pub fn start_with(paths: &Paths, exe: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    if autostart::is_installed() && autostart::kickstart().is_ok() {
        return Ok(());
    }
    spawn_detached(paths, exe)
}

/// Wait until a daemon answers.
pub async fn wait_until_healthy(paths: &Paths, settings: &Settings) -> Result<Health> {
    for _ in 0..40 {
        if let Some(h) = health(settings).await {
            return Ok(h);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("{}", not_running_reason(paths))
}

/// Make sure a daemon answers, starting one if needed.
pub async fn ensure_running(paths: &Paths, settings: &Settings) -> Result<Health> {
    if let Some(h) = health(settings).await {
        return Ok(h);
    }
    start(paths)?;
    wait_until_healthy(paths, settings).await
}

/// Stop a running daemon (graceful on macOS/Linux) and wait until it has
/// released the session.
pub async fn stop(paths: &Paths) -> Result<bool> {
    let Some(pid) = std::fs::read_to_string(&paths.pid)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return Ok(false);
    };
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .output()
    } else {
        Command::new("kill").args(["-TERM", &pid.to_string()]).output()
    };
    if !status.is_ok_and(|o| o.status.success()) {
        // Stale pid file.
        let _ = std::fs::remove_file(&paths.pid);
        return Ok(false);
    }
    for _ in 0..60 {
        if let Some(lock) = try_lock(&paths.lock)? {
            drop(lock);
            return Ok(true);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    error!("the daemon (pid {pid}) did not stop in time");
    bail!("could not stop the running zap-mcp daemon (pid {pid})")
}

/// Start at login: a LaunchAgent on macOS, a Run entry on Windows.
pub mod autostart {
    use super::*;

    #[cfg(target_os = "macos")]
    const LABEL: &str = "dev.zap-mcp.daemon";

    #[cfg(target_os = "macos")]
    fn plist_path() -> Result<std::path::PathBuf> {
        Ok(dirs::home_dir()
            .context("no home directory")?
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    #[cfg(target_os = "macos")]
    fn gui_domain() -> Result<String> {
        use std::os::unix::fs::MetadataExt;
        let home = dirs::home_dir().context("no home directory")?;
        Ok(format!("gui/{}", std::fs::metadata(home)?.uid()))
    }

    #[cfg(target_os = "macos")]
    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
    }

    #[cfg(target_os = "macos")]
    pub fn install(paths: &Paths, exe: &Path) -> Result<()> {
        let plist = plist_path()?;
        let mut args = vec![exe.display().to_string(), "daemon".into()];
        args.extend(data_dir_args(paths));
        let args_xml: String = args
            .iter()
            .map(|a| format!("\t\t<string>{}</string>\n", xml_escape(a)))
            .collect();
        let log = xml_escape(&paths.log.display().to_string());
        let content = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
{args_xml}	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>SuccessfulExit</key>
		<false/>
	</dict>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
        );
        if let Some(dir) = plist.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&plist, content)
            .with_context(|| format!("writing {}", plist.display()))?;
        let domain = gui_domain()?;
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LABEL}")])
            .output();
        let out = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist)
            .output()
            .context("running launchctl")?;
        if !out.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// (Re)start the installed LaunchAgent now.
    #[cfg(target_os = "macos")]
    pub fn kickstart() -> Result<()> {
        let target = format!("{}/{LABEL}", gui_domain()?);
        let out = Command::new("launchctl")
            .args(["kickstart", &target])
            .output()
            .context("running launchctl")?;
        if out.status.success() {
            return Ok(());
        }
        // Not loaded (e.g. after `bootout`): load it, which also starts it.
        let out = Command::new("launchctl")
            .args(["bootstrap", &gui_domain()?])
            .arg(plist_path()?)
            .output()
            .context("running launchctl")?;
        if !out.status.success() {
            bail!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub fn uninstall() -> Result<bool> {
        let plist = plist_path()?;
        // Only unload what we installed: the label is global to the login
        // session, the plist is per home directory.
        if !plist.exists() {
            return Ok(false);
        }
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{}/{LABEL}", gui_domain()?)])
            .output();
        std::fs::remove_file(&plist)?;
        Ok(true)
    }

    #[cfg(target_os = "macos")]
    pub fn is_installed() -> bool {
        plist_path().is_ok_and(|p| p.exists())
    }

    #[cfg(windows)]
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    #[cfg(windows)]
    pub fn install(paths: &Paths, exe: &Path) -> Result<()> {
        // `daemon --detach` re-launches itself without a console window.
        let mut value = format!("\"{}\" daemon --detach", exe.display());
        if paths.custom {
            value.push_str(&format!(" --data-dir \"{}\"", paths.dir.display()));
        }
        let out = Command::new("reg")
            .args(["add", RUN_KEY, "/v", "zap-mcp", "/t", "REG_SZ", "/d", &value, "/f"])
            .output()
            .context("running reg")?;
        if !out.status.success() {
            bail!("reg add failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        spawn_detached(paths, exe)
    }

    #[cfg(windows)]
    pub fn uninstall() -> Result<bool> {
        let out = Command::new("reg")
            .args(["delete", RUN_KEY, "/v", "zap-mcp", "/f"])
            .output()
            .context("running reg")?;
        Ok(out.status.success())
    }

    #[cfg(windows)]
    pub fn is_installed() -> bool {
        Command::new("reg")
            .args(["query", RUN_KEY, "/v", "zap-mcp"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn install(paths: &Paths, exe: &Path) -> Result<()> {
        warn!("start-at-login isn't supported on this OS yet; starting the daemon now only");
        spawn_detached(paths, exe)
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn uninstall() -> Result<bool> {
        Ok(false)
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn is_installed() -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Installs the real LaunchAgent / Run entry against a throwaway data dir,
    /// checks the OS starts the daemon, then removes it again. Touches the
    /// user's login items, so it only runs on request:
    /// `ZAP_TEST_DIR=/tmp/x cargo test -- --ignored autostart_roundtrip`
    #[tokio::test]
    #[ignore]
    async fn autostart_roundtrip() {
        let dir = std::env::var("ZAP_TEST_DIR").expect("set ZAP_TEST_DIR");
        let paths = Paths::new(Some(dir.into())).unwrap();
        let settings = Settings::load_or_create(&paths).unwrap();
        let exe = std::env::var("ZAP_TEST_EXE").expect("set ZAP_TEST_EXE");
        assert!(health(&settings).await.is_none(), "stop other test daemons first");

        autostart::install(&paths, Path::new(&exe)).unwrap();
        assert!(autostart::is_installed());
        let h = wait_until_healthy(&paths, &settings).await;
        let stopped = stop(&paths).await;
        let removed = autostart::uninstall();
        assert!(h.is_ok(), "daemon did not come up via autostart: {h:?}");
        assert!(stopped.unwrap(), "daemon was not stoppable");
        assert!(removed.unwrap());
        assert!(!autostart::is_installed());
    }

    #[test]
    fn token_comparison() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        let t = random_token().unwrap();
        assert_eq!(t.len(), 64);
        assert_ne!(t, random_token().unwrap());
    }
}
