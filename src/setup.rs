//! Interactive installer: what runs when a person starts the binary (double-
//! click or terminal) instead of an MCP client spawning it over stdio.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};

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
/// that Claude is currently running is not overwritten in place.
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
        // let Gatekeeper block the copy when Claude starts it.
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

fn register_claude_code(cli: &Path, exe: &Path, args: &[String]) -> Result<()> {
    // Replace any previous registration (e.g. older path or flags).
    let _ = claude_cmd(cli)
        .args(["mcp", "remove", SERVER_NAME, "-s", "user"])
        .output();
    let out = claude_cmd(cli)
        .args(["mcp", "add", "-s", "user", SERVER_NAME, "--"])
        .arg(exe)
        .args(args)
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

/// Add/replace our entry in Claude Desktop's config, keeping everything else
/// (and a backup of the previous file).
fn register_claude_desktop(config: &Path, exe: &Path, args: &[String]) -> Result<()> {
    let mut root: serde_json::Value = match std::fs::read_to_string(config) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON; fix or remove it, then run setup again",
                config.display()
            )
        })?,
        _ => serde_json::json!({}),
    };
    let obj = root
        .as_object_mut()
        .context("Claude Desktop config is not a JSON object")?;
    let servers = obj
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .context("`mcpServers` in Claude Desktop config is not an object")?;
    servers.insert(
        SERVER_NAME.into(),
        serde_json::json!({ "command": exe, "args": args }),
    );

    if config.exists() {
        std::fs::copy(config, config.with_extension("json.bak"))
            .context("backing up Claude Desktop config")?;
    } else if let Some(dir) = config.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(config, serde_json::to_string_pretty(&root)? + "\n")
        .with_context(|| format!("writing {}", config.display()))?;
    Ok(())
}

fn server_args(allow_send: bool, custom_data_dir: Option<&Path>) -> Vec<String> {
    let mut args = vec!["serve".to_string()];
    if allow_send {
        args.push("--allow-send".into());
    }
    if let Some(dir) = custom_data_dir {
        args.push("--data-dir".into());
        args.push(dir.display().to_string());
    }
    args
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

pub async fn run(paths: &Paths, custom_data_dir: Option<&Path>) -> Result<()> {
    let mut keys = Keys::default();
    let result = wizard(paths, custom_data_dir, &mut keys).await;
    if let Err(e) = &result {
        eprintln!("\n{e:#}");
    }
    // When double-clicked, the window closes as soon as we exit.
    eprintln!("\nPress Enter to close.");
    keys.enter().await;
    result
}

async fn wizard(paths: &Paths, custom_data_dir: Option<&Path>, keys: &mut Keys) -> Result<()> {
    eprintln!(
        "\nzap-mcp {} setup\n\
         Gives Claude access to your WhatsApp messages through a linked device.",
        env!("CARGO_PKG_VERSION")
    );

    heading("1/4  Disclaimer");
    ensure_terms_accepted(paths)?;
    eprintln!("Accepted.");

    heading("2/4  Install");
    let current = std::env::current_exe().context("locating this program")?;
    let dest = install_dir()?.join(EXE_NAME);
    let exe = if same_file(&current, &dest) {
        eprintln!("Already installed at {}", dest.display());
        dest
    } else if Confirm::with_theme(&theme())
        .with_prompt(format!("Install to {}?", dest.display()))
        .default(true)
        .interact()?
    {
        install_binary(&current, &dest)?;
        eprintln!("Installed {}", dest.display());
        dest
    } else {
        eprintln!("Using {} where it is.", current.display());
        current
    };

    heading("3/4  Connect to Claude");
    eprintln!(
        "Claude can read your messages. It can also send messages for you, but\n\
         sending through an unofficial client increases the risk of a ban."
    );
    let allow_send = Confirm::with_theme(&theme())
        .with_prompt("Allow Claude to send WhatsApp messages?")
        .default(false)
        .interact()?;
    let args = server_args(allow_send, custom_data_dir);

    let mut registered = Vec::new();
    match find_claude_cli() {
        Some(cli) => {
            if Confirm::with_theme(&theme())
                .with_prompt("Add to Claude Code?")
                .default(true)
                .interact()?
            {
                match register_claude_code(&cli, &exe, &args) {
                    Ok(()) => registered.push("Claude Code"),
                    Err(e) => eprintln!("  {e:#}"),
                }
            }
        }
        None => eprintln!("Claude Code not found (skipping)."),
    }
    match desktop_config_path() {
        Some(config) if config.parent().is_some_and(Path::exists) => {
            if Confirm::with_theme(&theme())
                .with_prompt("Add to Claude Desktop?")
                .default(true)
                .interact()?
            {
                match register_claude_desktop(&config, &exe, &args) {
                    Ok(()) => registered.push("Claude Desktop"),
                    Err(e) => eprintln!("  {e:#}"),
                }
            }
        }
        _ => eprintln!("Claude Desktop not found (skipping)."),
    }
    if registered.is_empty() {
        eprintln!(
            "\nNot added to any Claude app. Configure your MCP client to run:\n  {} {}",
            exe.display(),
            args.join(" ")
        );
    } else {
        eprintln!("Added to {}.", registered.join(" and "));
    }

    heading("4/4  Link WhatsApp");
    match try_lock(&paths.lock)? {
        None => eprintln!(
            "The WhatsApp server is running inside Claude right now, so it's already set up.\n\
             To link a different account, quit Claude and run this setup again."
        ),
        Some(_lock) => match link::linked_number(paths).await? {
            Some(number) => eprintln!("Already linked as {number}."),
            None => link::link(paths, ask_method()?, keys).await?,
        },
    }

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
        "Your data stays on this computer in {}",
        paths.terms.parent().map(|p| p.display().to_string()).unwrap_or_default()
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

        let args = server_args(true, None);
        register_claude_desktop(&config, Path::new("/bin/wa"), &args).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["mcpServers"]["whatsapp"]["command"], "/bin/wa");
        assert_eq!(
            v["mcpServers"]["whatsapp"]["args"],
            serde_json::json!(["serve", "--allow-send"])
        );
        assert!(config.with_extension("json.bak").exists());

        std::fs::write(&config, "{ not json").unwrap();
        assert!(register_claude_desktop(&config, Path::new("/bin/wa"), &args).is_err());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "{ not json");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
