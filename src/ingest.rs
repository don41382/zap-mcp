//! Turns WhatsApp events (live messages, history sync, contact sync) into rows
//! in the local [`Store`].

use std::sync::Arc;

use log::{info, warn};
use whatsapp_rust::prelude::*;
use whatsapp_rust::waproto::whatsapp::message::protocol_message::Type as ProtocolType;

use crate::store::{Batch, NewMessage, Store};

const STATUS_BROADCAST: &str = "status@broadcast";

/// What we keep of a message: a kind tag plus human-readable text.
enum Content {
    Message { kind: &'static str, text: Option<String> },
    Edit { id: String, text: String },
    Revoke { id: String },
}

fn describe(msg: &wa::Message) -> Option<Content> {
    let m = msg.get_base_message();

    if let Some(p) = m.protocol_message.as_option() {
        let id = p.key.as_option().and_then(|k| k.id.clone())?;
        return match p.r#type {
            Some(ProtocolType::REVOKE) => Some(Content::Revoke { id }),
            Some(ProtocolType::MESSAGE_EDIT) => {
                let text = p
                    .edited_message
                    .as_option()
                    .and_then(|e| e.text_content().or_else(|| e.get_caption()))?;
                Some(Content::Edit { id, text: text.to_string() })
            }
            _ => None,
        };
    }
    // Reactions, poll votes, key distribution etc. carry no readable content.
    if m.reaction_message.is_set() || m.sender_key_distribution_message.is_set() {
        return None;
    }

    let msg = |kind, text: Option<&str>| {
        Some(Content::Message {
            kind,
            text: text.filter(|t| !t.is_empty()).map(str::to_string),
        })
    };

    if let Some(text) = m.text_content() {
        return msg("text", Some(text));
    }
    if let Some(img) = m.image_message.as_option() {
        return msg("image", img.caption.as_deref());
    }
    if let Some(v) = m.video_message.as_option().or(m.ptv_message.as_option()) {
        return msg("video", v.caption.as_deref());
    }
    if let Some(a) = m.audio_message.as_option() {
        return msg(if a.ptt == Some(true) { "voice" } else { "audio" }, None);
    }
    if let Some(d) = m.document_message.as_option() {
        let text = match (&d.file_name, &d.caption) {
            (Some(f), Some(c)) => Some(format!("{f}: {c}")),
            (f, c) => f.clone().or_else(|| c.clone()),
        };
        return msg("document", text.as_deref());
    }
    if m.sticker_message.is_set() {
        return msg("sticker", None);
    }
    if let Some(l) = m.location_message.as_option() {
        let text = [l.name.as_deref(), l.address.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        return msg("location", Some(&text));
    }
    if m.live_location_message.is_set() {
        return msg("live_location", None);
    }
    if let Some(c) = m.contact_message.as_option() {
        return msg("contact", c.display_name.as_deref());
    }
    if m.contacts_array_message.is_set() {
        return msg("contacts", None);
    }
    if let Some(p) = [
        &m.poll_creation_message,
        &m.poll_creation_message_v2,
        &m.poll_creation_message_v3,
        &m.poll_creation_message_v5,
        &m.poll_creation_message_v6,
    ]
    .into_iter()
    .find_map(|p| p.as_option())
    {
        return msg("poll", p.name.as_deref());
    }
    if let Some(e) = m.event_message.as_option() {
        return msg("event", e.name.as_deref());
    }
    None
}

fn is_group_jid(jid: &str) -> bool {
    jid.ends_with("@g.us")
}

/// Live (or offline-queued) messages.
pub fn store_live_messages(store: &Store, batch: &MessageBatch) {
    let result = store.batch(|b| {
        for m in batch.iter() {
            let src = &m.info.source;
            let chat = src.chat.to_non_ad().to_string();
            if chat == STATUS_BROADCAST {
                continue;
            }
            let sender = src.sender.to_non_ad().to_string();

            // In 1:1 chats addressed by LID, the alt JID carries the phone number.
            if !src.is_group {
                let alt = if src.is_from_me { &src.recipient_alt } else { &src.sender_alt };
                if let Some(alt) = alt
                    && alt.is_pn()
                {
                    b.add_mapping(&chat, &alt.to_non_ad().to_string())?;
                }
            }
            if let Some(alt) = &src.sender_alt
                && src.sender.is_lid()
                && alt.is_pn()
            {
                b.add_mapping(&sender, &alt.to_non_ad().to_string())?;
            }

            if !src.is_from_me {
                b.upsert_contact(&sender, None, Some(&m.info.push_name))?;
            }
            let id = m.info.id.to_string();
            apply(
                b,
                &chat,
                src.is_group,
                &id,
                Some(&sender),
                Some(&m.info.push_name),
                src.is_from_me,
                m.info.timestamp.timestamp(),
                &m.message,
            )?;
        }
        Ok(())
    });
    if let Err(e) = result {
        warn!("failed to store live messages: {e:#}");
    }
}

#[allow(clippy::too_many_arguments)]
fn apply(
    b: &Batch<'_>,
    chat: &str,
    is_group: bool,
    id: &str,
    sender: Option<&str>,
    sender_name: Option<&str>,
    from_me: bool,
    ts: i64,
    message: &wa::Message,
) -> rusqlite::Result<()> {
    match describe(message) {
        Some(Content::Message { kind, text }) => {
            b.upsert_chat(chat, None, is_group, Some(ts))?;
            b.insert_message(&NewMessage {
                chat_jid: chat.to_string(),
                id: id.to_string(),
                sender_jid: sender.map(str::to_string),
                sender_name: sender_name.map(str::to_string),
                from_me,
                ts,
                kind: kind.to_string(),
                text,
            })
        }
        Some(Content::Edit { id, text }) => b.edit_message(chat, &id, &text),
        Some(Content::Revoke { id }) => b.revoke_message(chat, &id),
        None => Ok(()),
    }
}

/// History sync blobs: past conversations sent by the phone after linking,
/// plus push names and LID mappings.
pub fn store_history_sync(store: &Store, own_jid: Option<&str>, hs: &wa::HistorySync) {
    let mut count = 0usize;
    let result = store.batch(|b| {
        for m in &hs.phone_number_to_lid_mappings {
            if let (Some(lid), Some(pn)) = (&m.lid_jid, &m.pn_jid) {
                b.add_mapping(lid, pn)?;
            }
        }
        for p in &hs.pushnames {
            if let (Some(jid), Some(name)) = (&p.id, &p.pushname) {
                b.upsert_contact(jid, None, Some(name))?;
            }
        }
        for conv in &hs.conversations {
            let chat = conv.id.as_str();
            if chat == STATUS_BROADCAST {
                continue;
            }
            if let (Some(lid), Some(pn)) = (&conv.lid_jid, &conv.pn_jid) {
                b.add_mapping(lid, pn)?;
            } else if let Some(pn) = &conv.pn_jid {
                b.add_mapping(chat, pn)?;
            } else if let Some(lid) = &conv.lid_jid {
                b.add_mapping(lid, chat)?;
            }
            let is_group = is_group_jid(chat);
            let name = conv.name.as_deref().or(conv.display_name.as_deref());
            let ts = conv
                .conversation_timestamp
                .or(conv.last_msg_timestamp)
                .map(|t| t as i64);
            // Only groups carry a real chat name here; for 1:1 chats the
            // phone's address-book name arrives via contact sync instead.
            b.upsert_chat(chat, if is_group { name } else { None }, is_group, ts)?;
            if !is_group && name.is_some() {
                b.upsert_contact(chat, name, None)?;
            }

            for hm in &conv.messages {
                let Some(wmi) = hm.message.as_option() else { continue };
                let Some(key) = wmi.key.as_option() else { continue };
                let (Some(id), Some(message)) = (&key.id, wmi.message.as_option()) else {
                    continue;
                };
                let from_me = key.from_me.unwrap_or(false);
                let sender = if from_me {
                    own_jid.map(str::to_string)
                } else if is_group {
                    key.participant.clone().or_else(|| wmi.participant.clone())
                } else {
                    Some(chat.to_string())
                };
                let sender = sender.map(|s| strip_device(&s));
                if let (Some(s), Some(name)) = (&sender, &wmi.push_name)
                    && !from_me
                {
                    b.upsert_contact(s, None, Some(name))?;
                }
                apply(
                    b,
                    chat,
                    is_group,
                    id,
                    sender.as_deref(),
                    wmi.push_name.as_deref(),
                    from_me,
                    wmi.message_timestamp.unwrap_or(0) as i64,
                    message,
                )?;
                count += 1;
            }
        }
        Ok(())
    });
    match result {
        Ok(()) => info!(
            "history sync chunk stored: {} conversations, {count} messages (progress {}%)",
            hs.conversations.len(),
            hs.progress.unwrap_or(0)
        ),
        Err(e) => warn!("failed to store history sync: {e:#}"),
    }
}

/// `123:4@s.whatsapp.net` → `123@s.whatsapp.net`
fn strip_device(jid: &str) -> String {
    match jid.parse::<Jid>() {
        Ok(j) => j.to_non_ad().to_string(),
        Err(_) => jid.to_string(),
    }
}

/// Address-book names from app-state contact sync.
pub fn store_contact(store: &Store, jid: &Jid, action: &wa::sync_action_value::ContactAction) {
    let jid = jid.to_non_ad().to_string();
    let result = store.batch(|b| {
        if let (Some(lid), Some(pn)) = (&action.lid_jid, &action.pn_jid) {
            b.add_mapping(lid, pn)?;
        }
        let name = action.full_name.as_deref().or(action.first_name.as_deref());
        b.upsert_contact(&jid, name, None)
    });
    if let Err(e) = result {
        warn!("failed to store contact: {e:#}");
    }
}

/// Group subjects for every group we're in (names for groups that had no
/// history sync, and renames).
pub async fn refresh_groups(store: &Store, client: &Arc<Client>) {
    match client.groups().get_participating().await {
        Ok(groups) => {
            let result = store.batch(|b| {
                for (jid, meta) in &groups {
                    b.upsert_chat(&jid.to_string(), Some(&meta.subject), true, None)?;
                }
                Ok(())
            });
            match result {
                Ok(()) => info!("refreshed {} group names", groups.len()),
                Err(e) => warn!("failed to store group names: {e:#}"),
            }
        }
        Err(e) => warn!("failed to fetch groups: {e}"),
    }
}

/// Wire all ingestion handlers onto a bot builder.
pub fn register<B, T, H, R>(builder: BotBuilder<B, T, H, R>, store: Store) -> BotBuilder<B, T, H, R> {
    builder.on_event_for(
        &[
            EventKind::Messages,
            EventKind::HistorySync,
            EventKind::ContactUpdate,
            EventKind::PushNameUpdate,
            EventKind::Connected,
        ],
        move |event, client| {
            let store = store.clone();
            async move {
                match &*event {
                    Event::Messages(batch) => store_live_messages(&store, batch),
                    Event::HistorySync(_) => {
                        let own = client.pn().map(|j| j.to_non_ad().to_string());
                        // Inflating + decoding a multi-MB chunk is CPU work.
                        let res = tokio::task::spawn_blocking(move || {
                            if let Event::HistorySync(hs) = &*event {
                                match hs.get() {
                                    Some(decoded) => {
                                        store_history_sync(&store, own.as_deref(), decoded)
                                    }
                                    None => warn!("could not decode history sync chunk"),
                                }
                            }
                        })
                        .await;
                        if let Err(e) = res {
                            warn!("history sync task failed: {e}");
                        }
                    }
                    Event::ContactUpdate(c) => store_contact(&store, &c.jid, &c.action),
                    Event::PushNameUpdate(p) => {
                        let jid = p.jid.to_non_ad().to_string();
                        let _ = store.batch(|b| b.upsert_contact(&jid, None, Some(&p.new_push_name)));
                    }
                    Event::Connected(_) => refresh_groups(&store, &client).await,
                    _ => {}
                }
            }
        },
    )
}
