//! Linking this tool as a WhatsApp companion device (QR code or pairing code)
//! and pulling the initial message history. Interactive: talks to the user on
//! stderr.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tokio::sync::mpsc;
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::*;

use crate::store::Store;
use crate::{Paths, ingest, session_store};

pub enum Method {
    Qr,
    /// International number, digits only (e.g. `4915112345678`).
    Phone(String),
}

/// History is considered complete after this long without new messages.
const IDLE_DONE: Duration = Duration::from_secs(45);
/// …but give a phone that sends nothing at all at least this long.
const MIN_WAIT: Duration = Duration::from_secs(120);

/// Enter presses from stdin. Reading stdin blocks, so a thread does it; it is
/// started lazily and lives for the rest of the process, so every later
/// "press Enter" must go through the same `Keys`.
#[derive(Default)]
pub struct Keys {
    rx: Option<mpsc::UnboundedReceiver<()>>,
}

impl Keys {
    pub async fn enter(&mut self) {
        let rx = self.rx.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            std::thread::spawn(move || {
                let mut line = String::new();
                while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
                    line.clear();
                    if tx.send(()).is_err() {
                        break;
                    }
                }
            });
            rx
        });
        if rx.recv().await.is_none() {
            // stdin closed: never resolve, so callers fall through to their
            // other select! branches instead of treating EOF as Enter.
            std::future::pending::<()>().await;
        }
    }
}

/// The linked phone number (`+49…`) if this data dir already holds a session.
/// Opens the session store without connecting.
pub async fn linked_number(paths: &Paths) -> Result<Option<String>> {
    let bot = Bot::builder()
        .with_backend(session_store(paths).await?)
        .build()
        .await
        .map_err(|e| anyhow!("opening WhatsApp session: {e}"))?;
    Ok(bot.client().pn().map(|j| format!("+{}", j.user)))
}

/// Validate a phone number for pairing-code linking; returns digits only.
pub fn normalize_phone(input: &str) -> Result<String, &'static str> {
    let digits: String = input.chars().filter(char::is_ascii_digit).collect();
    if input
        .chars()
        .any(|c| !(c.is_ascii_digit() || " +-()/".contains(c)))
    {
        return Err("Only digits, spaces, +, -, ( ) are allowed");
    }
    if digits.starts_with('0') {
        return Err("Use the international format with country code, e.g. +49 151 12345678");
    }
    if !(7..=15).contains(&digits.len()) {
        return Err("That doesn't look like a full phone number with country code");
    }
    Ok(digits)
}

pub fn render_qr(code: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    match qrcode::QrCode::new(code) {
        Ok(qr) => qr
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
        Err(_) => code.to_string(),
    }
}

/// Link the device and download history until it settles, the user presses
/// Enter, or Ctrl+C. The caller must hold the session lock.
pub async fn link(paths: &Paths, method: Method, keys: &mut Keys) -> Result<()> {
    let store = Store::open(&paths.archive_db)?;
    let show_qr = matches!(method, Method::Qr);
    let connected = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(AtomicBool::new(false));

    let mut builder = Bot::builder()
        .with_backend(session_store(paths).await?)
        .on_qr_code(move |code, timeout| async move {
            if !show_qr {
                return;
            }
            let _ = console::Term::stderr().clear_screen();
            eprintln!(
                "On your phone: WhatsApp > Settings > Linked Devices > Link a Device,\n\
                 then scan this code. It refreshes every {}s.\n",
                timeout.as_secs()
            );
            eprintln!("{}", render_qr(&code));
        })
        .on_pair_code(|code, timeout| async move {
            eprintln!(
                "\nOn your phone: WhatsApp > Settings > Linked Devices > Link a Device >\n\
                 \"Link with phone number instead\", and enter this code:\n\n\
                 \x20   {code}\n\n(valid for {} seconds)\n",
                timeout.as_secs()
            );
        });

    let failed_flag = failed.clone();
    builder = builder.on_event_for(
        &[
            EventKind::PairSuccess,
            EventKind::PairError,
            EventKind::PairingCodeError,
            EventKind::PairingQrCodesExhausted,
            EventKind::LoggedOut,
        ],
        move |event, _client| {
            let failed = failed_flag.clone();
            async move {
                match &*event {
                    Event::PairSuccess(_) => eprintln!("\nLinked! Connecting..."),
                    Event::PairError(e) => {
                        eprintln!("\nLinking failed: {}", e.error);
                        failed.store(true, Ordering::SeqCst);
                    }
                    Event::PairingCodeError(e) => {
                        eprintln!(
                            "\nCould not get a pairing code ({}). Check the number, or use the QR code instead.",
                            e.error
                        );
                        failed.store(true, Ordering::SeqCst);
                    }
                    Event::PairingQrCodesExhausted(_) => {
                        eprintln!("\nThe QR codes expired without being scanned. Run the setup again.");
                        failed.store(true, Ordering::SeqCst);
                    }
                    Event::LoggedOut(_) => {
                        eprintln!("\nWhatsApp logged this device out.");
                        failed.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                }
            }
        },
    );

    let connected_flag = connected.clone();
    builder = builder.on_connected(move |client| {
        let connected = connected_flag.clone();
        async move {
            let who = client
                .pn()
                .map(|j| format!("+{}", j.user))
                .unwrap_or_else(|| "your number".into());
            eprintln!("\nConnected as {who}.");
            connected.store(true, Ordering::SeqCst);
        }
    });

    if let Method::Phone(phone_number) = method {
        builder = builder.with_pair_code(PairCodeOptions {
            phone_number,
            ..Default::default()
        });
    } else {
        eprintln!("Requesting a QR code from WhatsApp...");
    }

    let bot = ingest::register(builder, store.clone())
        .build()
        .await
        .map_err(|e| anyhow!("starting WhatsApp client: {e}"))?;
    let mut handle = bot.spawn();

    // Phase 1: wait until the phone has linked us and we're connected.
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = &mut handle => bail!("the WhatsApp client stopped before linking finished"),
            _ = tokio::signal::ctrl_c() => {
                handle.shutdown().await;
                bail!("cancelled");
            }
            _ = tick.tick() => {
                if connected.load(Ordering::SeqCst) {
                    break;
                }
                if failed.load(Ordering::SeqCst) {
                    handle.shutdown().await;
                    bail!("linking did not complete");
                }
            }
        }
    }

    // Phase 2: let the phone push history until it settles.
    eprintln!(
        "\nYour phone is now sending your message history. This usually takes a minute or two.\n\
         It finishes automatically, or press Enter to stop early (the rest arrives later\n\
         whenever Claude is running).\n"
    );
    let started = Instant::now();
    let mut last = (-1, -1);
    let mut last_change = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            _ = &mut handle => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = keys.enter() => break,
            _ = tick.tick() => {
                let Ok(s) = store.stats() else { continue };
                if (s.chats, s.messages) != last {
                    last = (s.chats, s.messages);
                    last_change = Instant::now();
                    eprintln!("  {} chats, {} messages", s.chats, s.messages);
                } else if last_change.elapsed() > IDLE_DONE
                    && (s.messages > 0 || started.elapsed() > MIN_WAIT)
                {
                    eprintln!("  No new messages for {}s, looks complete.", IDLE_DONE.as_secs());
                    break;
                }
            }
        }
    }
    eprintln!("Saving...");
    handle.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::normalize_phone;

    #[test]
    fn phone_numbers() {
        assert_eq!(normalize_phone("+49 151 1234-5678").unwrap(), "4915112345678");
        assert!(normalize_phone("0151 12345678").is_err());
        assert!(normalize_phone("12345").is_err());
        assert!(normalize_phone("+49 abc").is_err());
    }
}
