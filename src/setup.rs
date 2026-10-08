//! Interactive installer: what runs when a person starts the binary (double-
//! click or terminal) instead of an MCP client spawning it over stdio.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};

use crate::daemon::{self, Settings};
use crate::link::{self, Keys, Method};
use crate::{Paths, try_lock};

pub const DISCLAIMER: &str = include_str!("../DISCLAIMER.md");
/// Bump when DISCLAIMER.md changes materially, so users re-accept.
const TERMS_VERSION: &str = "1";
const SERVER_NAME: &str = "whatsapp";
const EXE_NAME: &str = if cfg!(windows) {
    "zap-mcp.exe"
} else {
    "zap-mcp"
};

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

fn heading(step: &str) {
    eprintln!("\n── {step} ──────────────────────────────────────────\n");
}

/// Show the disclaimer and require explicit acceptance once per terms version.
/// The acceptance (version + time) is recorded in the data dir.
pub fn ensure_terms_accepted(paths: &Paths) -> Result<()> {
    let marker = format!("terms-version: {TERMS_VERSION}");
    if std::fs::read_to_string(&paths.terms)
        .is_ok_and(|s| s.lines().any(|l| l == marker))
    {
        return Ok(());
    }
    eprintln!("{DISCLAIMER}");
    let accepted = Confirm::with_theme(&theme())
        .with_prompt(
            "I have read the disclaimer, understand that WhatsApp may ban my number, \
             and use this software entirely at my own risk",
        )
        .default(false)
        .interact()?;
    if !accepted {
        bail!("You did not accept the disclaimer. Nothing was installed or changed.");
    }
    std::fs::write(
        &paths.terms,
        format!(
            "{marker}\naccepted-at: {}\nsoftware-version: {}\n",
            chrono::Utc::now().to_rfc3339(),
            env!("CARGO_PKG_VERSION")
        ),
    )
    .context("recording acceptance")?;
    Ok(())
}

fn install_dir() -> Result<PathBuf> {
    if cfg!(windows) {
        Ok(dirs::data_local_dir()
            .context("cannot find %LOCALAPPDATA%")?
            .join("Programs")
            .join("zap-mcp"))
    } else {
        Ok(dirs::home_dir()
            .context("cannot find home directory")?
            .join(".local")
            .join("bin"))
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Copy the running binary to a stable location. Replaces via rename so a copy
/// that is currently running is not overwritten in place.
fn install_binary(current: &Path, dest: &Path) -> Result<()> {
    let dir = dest.parent().context("install path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dest.with_extension("new");
    std::fs::copy(current, &tmp).with_context(|| format!("copying to {}", tmp.display()))?;
    if cfg!(windows) && dest.exists() {
        // A running .exe can't be replaced, but it can be renamed away.
        let old = dest.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(dest, &old).with_context(|| {
            format!("{} is in use; quit Claude and try again", dest.display())
        })?;
    }
    std::fs::rename(&tmp, dest).with_context(|| format!("installing {}", dest.display()))?;
    if cfg!(target_os = "macos") {
        // We're already running, so the user approved this download; don't
        // let Gatekeeper block the copy when launchd or Claude starts it.
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(dest)
            .output();
    }
    Ok(())
}

fn find_claude_cli() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd", "claude.bat"]
    } else {
        &["claude"]
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    // Default install locations, in case the shell PATH isn't loaded (e.g.
    // when double-clicked).
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".claude").join("local"));
    }
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file())
}

fn claude_cmd(cli: &Path) -> Command {
    let is_script = cli
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    if is_script {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cli);
        c
    } else {
        Command::new(cli)
    }
}

/// Claude Code talks to the daemon directly over HTTP.
fn register_claude_code(cli: &Path, settings: &Settings) -> Result<()> {
    // Replace any previous registration (older stdio command, other port).
    let _ = claude_cmd(cli)
        .args(["mcp", "remove", SERVER_NAME, "-s", "user"])
        .output();
    let out = claude_cmd(cli)
        .args(["mcp", "add", "-s", "user", "--transport", "http", SERVER_NAME])
        .arg(settings.url())
        .arg("-H")
        .arg(settings.auth_header())
        .output()
        .context("running `claude mcp add`")?;
    if !out.status.success() {
        bail!(
            "`claude mcp add` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn desktop_config_path() -> Option<PathBuf> {
    Some(
        dirs::config_dir()?
            .join("Claude")
            .join("claude_desktop_config.json"),
    )
}

fn read_json_config(config: &Path) -> Result<serde_json::Value> {
    match std::fs::read_to_string(config) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON; fix or remove it, then run setup again",
                config.display()
            )
        }),
        _ => Ok(serde_json::json!({})),
    }
}

fn write_json_config(config: &Path, root: &serde_json::Value) -> Result<()> {
    if config.exists() {
        std::fs::copy(config, config.with_extension("json.bak"))
            .context("backing up Claude Desktop config")?;
    } else if let Some(dir) = config.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(config, serde_json::to_string_pretty(root)? + "\n")
        .with_context(|| format!("writing {}", config.display()))
}

/// Claude Desktop can only launch commands, so it runs `zap-mcp serve`, which
/// relays to the daemon. Keeps everything else in the config, plus a backup.
fn register_claude_desktop(config: &Path, exe: &Path, args: &[String]) -> Result<()> {
    let mut root = read_json_config(config)?;
    let obj = root
        .as_object_mut()
        .context("Claude Desktop config is not a JSON object")?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("`mcpServers` in Claude Desktop config is not an object")?;
    servers.insert(
        SERVER_NAME.into(),
        serde_json::json!({ "command": exe, "args": args }),
    );
    write_json_config(config, &root)
}

fn unregister_claude_desktop(config: &Path) -> Result<bool> {
    if !config.exists() {
        return Ok(false);
    }
    let mut root = read_json_config(config)?;
    let removed = root
        .get_mut("mcpServers")
        .and_then(|s| s.as_object_mut())
        .is_some_and(|s| s.remove(SERVER_NAME).is_some());
    if removed {
        write_json_config(config, &root)?;
    }
    Ok(removed)
}

fn relay_args(paths: &Paths) -> Vec<String> {
    vec![
        "serve".into(),
        "--data-dir".into(),
        paths.dir.display().to_string(),
    ]
}

/// Ask how to link and validate the phone number.
pub fn ask_method() -> Result<Method> {
    let choice = Select::with_theme(&theme())
        .with_prompt("How do you want to link WhatsApp?")
        .items([
            "Scan a QR code with your phone",
            "Enter a code on your phone (pairing with your phone number)",
        ])
        .default(0)
        .interact()?;
    if choice == 0 {
        return Ok(Method::Qr);
    }
    let phone: String = Input::with_theme(&theme())
        .with_prompt("Your WhatsApp phone number with country code (e.g. +49 151 12345678)")
        .validate_with(|s: &String| link::normalize_phone(s).map(|_| ()))
        .interact_text()?;
    Ok(Method::Phone(
        link::normalize_phone(&phone).expect("validated above"),
    ))
}

fn confirm(prompt: &str, default: bool) -> Result<bool> {
    Ok(Confirm::with_theme(&theme())
        .with_prompt(prompt)
        .default(default)
        .interact()?)
}

pub async fn run(paths: &Paths) -> Result<()> {
    let mut keys = Keys::default();
    let result = wizard(paths, &mut keys).await;
    if let Err(e) = &result {
        eprintln!("\n{e:#}");
    }
    // When double-clicked, the window closes as soon as we exit.
    eprintln!("\nPress Enter to close.");
    keys.enter().await;
    result
}

async fn wizard(paths: &Paths, keys: &mut Keys) -> Result<()> {
    eprintln!(
        "\nzap-mcp {} setup\n\
         Gives Claude access to your WhatsApp messages through a linked device.",
        env!("CARGO_PKG_VERSION")
    );

    heading("1/5  Disclaimer");
    ensure_terms_accepted(paths)?;
    eprintln!("Accepted.");

    heading("2/5  Install");
    let current = std::env::current_exe().context("locating this program")?;
    let dest = install_dir()?.join(EXE_NAME);
    let exe = if same_file(&current, &dest) {
        eprintln!("Already installed at {}", dest.display());
        dest
    } else if confirm(&format!("Install to {}?", dest.display()), true)? {
        install_binary(&current, &dest)?;
        eprintln!("Installed {}", dest.display());
        dest
    } else {
        eprintln!("Using {} where it is.", current.display());
        current
    };

    heading("3/5  Options");
    eprintln!(
        "Claude can read your messages. It can also send messages for you, but\n\
         sending through an unofficial client increases the risk of a ban."
    );
    let mut settings = Settings::load_or_create(paths)?;
    settings.allow_send = confirm("Allow Claude to send WhatsApp messages?", false)?;
    settings.save(paths)?;
    eprintln!(
        "\nzap-mcp runs as one small background service that keeps WhatsApp connected\n\
         and stores new messages, shared by all Claude windows."
    );
    let at_login = confirm("Start it automatically when you log in? (recommended)", true)?;

    heading("4/5  Connect to Claude");
    let mut registered = Vec::new();
    match find_claude_cli() {
        Some(cli) => {
            if confirm("Add to Claude Code?", true)? {
                match register_claude_code(&cli, &settings) {
                    Ok(()) => registered.push("Claude Code"),
                    Err(e) => eprintln!("  {e:#}"),
                }
            }
        }
        None => eprintln!("Claude Code not found (skipping)."),
    }
    match desktop_config_path() {
        Some(config) if config.parent().is_some_and(Path::exists) => {
            if confirm("Add to Claude Desktop?", true)? {
                match register_claude_desktop(&config, &exe, &relay_args(paths)) {
                    Ok(()) => registered.push("Claude Desktop"),
                    Err(e) => eprintln!("  {e:#}"),
                }
            }
        }
        _ => eprintln!("Claude Desktop not found (skipping)."),
    }
    if registered.is_empty() {
        eprintln!(
            "\nNot added to any Claude app. Point your MCP client at:\n  \
             HTTP: {}  (header \"{}\")\n  or stdio: {} {}",
            settings.url(),
            settings.auth_header(),
            exe.display(),
            relay_args(paths).join(" ")
        );
    } else {
        eprintln!("Added to {}.", registered.join(" and "));
    }

    // Linking needs the session, so a running (possibly older) daemon has to
    // stop first; it's started again below with the installed binary.
    heading("5/5  Link WhatsApp");
    daemon::stop(paths).await?;
    match try_lock(&paths.lock)? {
        None => bail!(
            "Another zap-mcp (or whatsapp-rust-mcp) is still using WhatsApp, probably\n\
             inside a Claude window. Quit Claude completely and run setup again."
        ),
        Some(lock) => {
            let linked = link::linked_number(paths).await;
            let result = match linked {
                Ok(Some(number)) => {
                    eprintln!("Already linked as {number}.");
                    Ok(())
                }
                Ok(None) => match ask_method() {
                    Ok(method) => link::link(paths, method, keys).await,
                    Err(e) => Err(e),
                },
                Err(e) => Err(e),
            };
            drop(lock);
            result?;
        }
    }

    heading("Starting");
    if at_login {
        daemon::autostart::install(paths, &exe)?;
    } else {
        daemon::autostart::uninstall()?;
        daemon::spawn_detached(paths, &exe)?;
    }
    let health = daemon::wait_until_healthy(paths, &settings).await?;
    eprintln!(
        "Background service running{}{}.",
        if at_login { " (starts at login)" } else { "" },
        if health.linked { "" } else { ", but WhatsApp is not linked" }
    );

    heading("Done");
    if registered.is_empty() {
        eprintln!("Set up your MCP client as shown above.");
    } else {
        eprintln!(
            "Restart {} and ask something like \"What did I miss on WhatsApp today?\"",
            registered.join(" / ")
        );
    }
    eprintln!(
        "Your data stays on this computer in {}\nCheck on it any time with `zap-mcp status`.",
        paths.dir.display()
    );
    Ok(())
}

/// Stop the daemon, remove start-at-login and the Claude registrations.
/// Leaves the data (and the WhatsApp link) in place.
pub async fn uninstall(paths: &Paths) -> Result<()> {
    if daemon::stop(paths).await? {
        eprintln!("Stopped the background service.");
    }
    if daemon::autostart::uninstall()? {
        eprintln!("Removed start at login.");
    }
    if let Some(cli) = find_claude_cli() {
        let out = claude_cmd(&cli)
            .args(["mcp", "remove", SERVER_NAME, "-s", "user"])
            .output();
        if out.is_ok_and(|o| o.status.success()) {
            eprintln!("Removed from Claude Code.");
        }
    }
    if let Some(config) = desktop_config_path()
        && unregister_claude_desktop(&config)?
    {
        eprintln!("Removed from Claude Desktop.");
    }
    eprintln!(
        "\nYour data is still in {}.\n\
         To remove everything: unlink the device on your phone (WhatsApp > Linked Devices),\n\
         then delete that folder and the zap-mcp program.",
        paths.dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_config_is_merged_not_replaced() {
        let dir = std::env::temp_dir().join(format!("wa-mcp-setup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("claude_desktop_config.json");
        std::fs::write(
            &config,
            r#"{"theme":"dark","mcpServers":{"other":{"command":"x"}}}"#,
        )
        .unwrap();

        let args = vec!["serve".to_string()];
        register_claude_desktop(&config, Path::new("/bin/wa"), &args).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["whatsapp"]["command"], "/bin/wa");
        assert_eq!(v["mcpServers"]["whatsapp"]["args"], serde_json::json!(["serve"]));
        assert!(config.with_extension("json.bak").exists());

        assert!(unregister_claude_desktop(&config).unwrap());
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert!(v["mcpServers"].get("whatsapp").is_none());
        assert_eq!(v["mcpServers"]["other"]["command"], "x");

        std::fs::write(&config, "{ not json").unwrap();
        assert!(register_claude_desktop(&config, Path::new("/bin/wa"), &args).is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
