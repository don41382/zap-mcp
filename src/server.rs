//! MCP tools over the local archive (and, optionally, sending through the
//! linked WhatsApp session). One instance per MCP session, all sharing the
//! daemon's store and WhatsApp client.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::json;
use whatsapp_rust::download::DownloadParams;
use whatsapp_rust::media::{self, AudioOptions, DocumentOptions, ImageOptions, VideoOptions};
use whatsapp_rust::prelude::*;
use whatsapp_rust::wacore::download::MediaType;
use whatsapp_rust::{MediaRetryResult, MediaReuploadRequest, UploadOptions};

use crate::ingest::{self, Content};
use crate::store::{MediaHit, MediaRef, NewMessage, StoredMessage, Store};

pub const INSTRUCTIONS: &str = "Access to the user's personal WhatsApp messages via a locally linked device. \
     Messages are private: quote only what the user needs. Times are in the user's \
     local timezone. Start with list_chats or recent_messages; use get_messages for \
     a conversation and search_messages for keywords. Media messages carry an id \
     (`[image id=…]`); pass it to download_media to save the file and see images.";

#[derive(Clone)]
pub struct WhatsAppServer {
    store: Store,
    client: Arc<Client>,
    /// Default folder for download_media.
    media_dir: PathBuf,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListChatsParams {
    /// Filter by chat or contact name (substring, case-insensitive) or JID.
    pub query: Option<String>,
    /// Maximum number of chats (default 30).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMessagesParams {
    /// Chat to read: a name (e.g. "Mom", "Family group"), a phone number with
    /// country code, or a JID from list_chats.
    pub chat: String,
    /// Maximum number of messages, most recent first within the window (default 50).
    pub limit: Option<u32>,
    /// Only messages at or after this time (ISO 8601, YYYY-MM-DD, or unix seconds).
    pub after: Option<String>,
    /// Only messages before this time (ISO 8601, YYYY-MM-DD, or unix seconds).
    pub before: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    /// Text to look for (substring, case-insensitive for ASCII).
    pub query: String,
    /// Restrict to one chat (name, phone number, or JID).
    pub chat: Option<String>,
    /// Only messages at or after this time (ISO 8601, YYYY-MM-DD, or unix seconds).
    pub after: Option<String>,
    /// Only messages before this time (ISO 8601, YYYY-MM-DD, or unix seconds).
    pub before: Option<String>,
    /// Maximum number of results (default 50).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RecentParams {
    /// Only messages at or after this time (ISO 8601, YYYY-MM-DD, or unix seconds).
    /// Defaults to the last 24 hours.
    pub since: Option<String>,
    /// Maximum number of messages (default 100).
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendParams {
    /// Recipient: a chat name, a phone number with country code, or a JID.
    pub to: String,
    /// Message text.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendMediaParams {
    /// Recipient: a chat name, a phone number with country code, or a JID.
    pub to: String,
    /// Absolute path to a local file. Images (JPEG, PNG, WebP, GIF), MP4/3GP
    /// videos and audio (OGG/Opus, MP3, M4A, AAC, AMR) are sent as media,
    /// anything else as a document.
    pub path: String,
    /// Optional caption shown under the photo, video or document (not
    /// supported for audio).
    pub caption: Option<String>,
    /// Send as a document (file attachment) even if it is a photo, video or
    /// audio file. Keeps the original quality and allows larger files.
    pub as_document: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DownloadMediaParams {
    /// Id of the media message, shown as `id=…` in message listings.
    pub message_id: String,
    /// Chat the message is in (name, phone number, or JID). Only needed when
    /// the id is ambiguous.
    pub chat: Option<String>,
    /// Absolute path of a folder to save the file in. Defaults to the
    /// `media` folder in zap-mcp's data directory.
    pub save_dir: Option<String>,
}

/// WhatsApp rejects photos, videos and audio above 16 MB.
const MAX_MEDIA_BYTES: u64 = 16 * 1024 * 1024;
/// Documents may be larger on WhatsApp's side, but uploads are read into
/// memory, so we stop well before its 2 GB limit.
const MAX_DOCUMENT_BYTES: u64 = 100 * 1024 * 1024;
/// Downloaded images up to this size are also returned inline. Keeps the
/// base64 under the 5 MB per-image limit of model APIs.
const MAX_INLINE_IMAGE_BYTES: u64 = 3 * 1024 * 1024;
const INLINE_IMAGE_TYPES: &[&str] = &["image/jpeg", "image/png", "image/webp", "image/gif"];

/// File extensions we know: mimetype and how the file is sent by default.
/// The first extension listed for a mimetype names downloaded files.
const FILE_TYPES: &[(&str, &str, MediaType)] = &[
    ("jpg", "image/jpeg", MediaType::Image),
    ("jpeg", "image/jpeg", MediaType::Image),
    ("png", "image/png", MediaType::Image),
    ("webp", "image/webp", MediaType::Image),
    ("gif", "image/gif", MediaType::Image),
    ("mp4", "video/mp4", MediaType::Video),
    ("m4v", "video/mp4", MediaType::Video),
    ("3gp", "video/3gpp", MediaType::Video),
    ("ogg", "audio/ogg; codecs=opus", MediaType::Audio),
    ("opus", "audio/ogg; codecs=opus", MediaType::Audio),
    ("mp3", "audio/mpeg", MediaType::Audio),
    ("m4a", "audio/mp4", MediaType::Audio),
    ("aac", "audio/aac", MediaType::Audio),
    ("amr", "audio/amr", MediaType::Audio),
    ("pdf", "application/pdf", MediaType::Document),
    ("txt", "text/plain", MediaType::Document),
    ("csv", "text/csv", MediaType::Document),
    ("json", "application/json", MediaType::Document),
    ("zip", "application/zip", MediaType::Document),
    ("doc", "application/msword", MediaType::Document),
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document", MediaType::Document),
    ("xls", "application/vnd.ms-excel", MediaType::Document),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", MediaType::Document),
    ("ppt", "application/vnd.ms-powerpoint", MediaType::Document),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation", MediaType::Document),
    ("mov", "video/quicktime", MediaType::Document),
    ("heic", "image/heic", MediaType::Document),
];

fn file_type(path: &Path) -> (&'static str, MediaType) {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    FILE_TYPES
        .iter()
        .find(|(e, ..)| *e == ext)
        .map_or(("application/octet-stream", MediaType::Document), |&(_, mime, kind)| {
            (mime, kind)
        })
}

/// `<id>.<ext>`, or `<id>-<original name>` for documents, so files from
/// different messages never collide.
fn download_file_name(id: &str, m: &MediaRef) -> String {
    let id: String = id.chars().filter(char::is_ascii_alphanumeric).collect();
    if let Some(name) = m.file_name.as_deref() {
        let name: String = name
            .chars()
            .map(|c| if c.is_control() || r#"/\:*?"<>|"#.contains(c) { '_' } else { c })
            .collect();
        let name = name.trim().trim_start_matches('.');
        if !name.is_empty() {
            return format!("{id}-{name}");
        }
    }
    let essence = |t: &str| t.split(';').next().unwrap_or_default().trim().to_ascii_lowercase();
    let ext = m.mimetype.as_deref().map(essence).and_then(|mime| {
        FILE_TYPES.iter().find(|(_, t, _)| essence(t) == mime).map(|(e, ..)| *e)
    });
    format!("{id}.{}", ext.unwrap_or("bin"))
}

fn download_type(media_type: &str) -> MediaType {
    match media_type {
        "image" => MediaType::Image,
        "video" => MediaType::Video,
        "audio" => MediaType::Audio,
        "sticker" => MediaType::Sticker,
        _ => MediaType::Document,
    }
}

fn fmt_size(bytes: u64) -> String {
    match bytes {
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b => format!("{b} bytes"),
    }
}

// Tools return `Result<String, String>`: rmcp turns `Err` into a tool result
// with `isError: true`, which the model sees and can act on (unlike a
// JSON-RPC protocol error).
type ToolResult = Result<String, String>;

fn invalid(msg: impl Into<String>) -> String {
    msg.into()
}

fn internal(e: impl std::fmt::Display) -> String {
    format!("internal error: {e}")
}

fn text_result(text: impl Into<String>) -> ToolResult {
    Ok(text.into())
}

fn parse_time(field: &str, s: Option<&str>) -> Result<Option<i64>, String> {
    let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if let Ok(secs) = s.parse::<i64>() {
        return Ok(Some(secs));
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(Some(dt.timestamp()));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt)
            && let Some(local) = Local.from_local_datetime(&naive).earliest()
        {
            return Ok(Some(local.timestamp()));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d")
        && let Some(local) = date
            .and_hms_opt(0, 0, 0)
            .and_then(|d| Local.from_local_datetime(&d).earliest())
    {
        return Ok(Some(local.timestamp()));
    }
    Err(invalid(format!(
        "could not parse `{field}` = {s:?}; use ISO 8601, YYYY-MM-DD, or unix seconds"
    )))
}

fn fmt_time(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn sender_label(m: &StoredMessage) -> String {
    if m.from_me {
        return "Me".into();
    }
    let s = m.sender.as_str();
    match s.split_once('@') {
        Some((user, "s.whatsapp.net")) => format!("+{user}"),
        Some((user, _)) if !user.is_empty() => user.to_string(),
        _ if s.is_empty() => "?".into(),
        _ => s.to_string(),
    }
}

fn body(m: &StoredMessage) -> String {
    // Downloadable media carries its id, which download_media takes.
    let tag = if m.has_media {
        format!("{} id={}", m.kind, m.id)
    } else {
        m.kind.clone()
    };
    match (m.kind.as_str(), &m.text) {
        ("text", Some(t)) => t.clone(),
        ("deleted", _) => "[deleted]".into(),
        (_, Some(t)) => format!("[{tag}] {t}"),
        (_, None) => format!("[{tag}]"),
    }
}

/// One line per message: `[2026-10-08 12:01] Alice: hello`. Much cheaper for
/// the model to read than JSON.
fn render(messages: &[StoredMessage], with_chat: bool) -> String {
    let mut out = String::new();
    for m in messages {
        let _ = write!(out, "[{}] ", fmt_time(m.ts));
        if with_chat {
            let chat = m.chat_name.as_deref().unwrap_or(&m.chat_jid);
            let _ = write!(out, "({chat}) ");
        }
        let _ = writeln!(out, "{}: {}", sender_label(m), body(m));
    }
    out
}

impl WhatsAppServer {
    pub fn new(store: Store, client: Arc<Client>, allow_send: bool, media_dir: PathBuf) -> Self {
        let mut tool_router = Self::tool_router();
        if !allow_send {
            tool_router.remove_route("send_message");
            tool_router.remove_route("send_media");
        }
        Self {
            store,
            client,
            media_dir,
            tool_router,
        }
    }

    /// Resolve user input to exactly one chat JID, or explain the ambiguity.
    fn resolve_chat(&self, input: &str) -> Result<(String, Option<String>), String> {
        let matches = self.store.find_chats(input).map_err(internal)?;
        match matches.as_slice() {
            [one] => Ok((one.jid.clone(), one.name.clone())),
            [] => Err(invalid(format!(
                "no chat matches {input:?}; use list_chats to find it"
            ))),
            many => {
                let options: Vec<String> = many
                    .iter()
                    .map(|c| format!("{} ({})", c.name.as_deref().unwrap_or("?"), c.jid))
                    .collect();
                Err(invalid(format!(
                    "{input:?} matches several chats, pass one of these JIDs: {}",
                    options.join("; ")
                )))
            }
        }
    }

    /// Like `resolve_chat`, but a bare phone number that is not a known chat
    /// yet is accepted directly.
    fn resolve_recipient(&self, to: &str) -> Result<(Jid, String, Option<String>), String> {
        let digits: String = to.chars().filter(|c| c.is_ascii_digit()).collect();
        let is_phone = !digits.is_empty()
            && to.chars().all(|c| c.is_ascii_digit() || " +-()".contains(c));
        let (jid_str, name) = match self.resolve_chat(to) {
            Ok(found) => found,
            Err(_) if is_phone => (format!("{digits}@s.whatsapp.net"), None),
            Err(e) => return Err(e),
        };
        let jid: Jid = jid_str
            .parse()
            .map_err(|e| invalid(format!("invalid JID {jid_str}: {e}")))?;
        Ok((jid, jid_str, name))
    }

    /// Send `message` to `to` and archive it, since our own sends are not
    /// echoed back to this device.
    async fn send_and_archive(&self, to: &str, message: wa::Message) -> ToolResult {
        let client = &self.client;
        let (jid, jid_str, name) = self.resolve_recipient(to)?;
        let Some(Content::Message { kind, text, media }) = ingest::describe(&message) else {
            return Err(internal("outgoing message has no archivable content"));
        };
        let sent = client.send_message(jid, message).await.map_err(internal)?;

        let own = client.pn().map(|j| j.to_non_ad().to_string());
        let now = chrono::Utc::now().timestamp();
        let _ = self.store.batch(|b| {
            b.upsert_chat(&jid_str, None, jid_str.ends_with("@g.us"), Some(now))?;
            if let Some(media) = &media {
                b.insert_media(&jid_str, &sent.message_id, media)?;
            }
            b.insert_message(&NewMessage {
                chat_jid: jid_str.clone(),
                id: sent.message_id.clone(),
                sender_jid: own,
                sender_name: None,
                from_me: true,
                ts: now,
                kind: kind.into(),
                text,
            })
        });

        text_result(format!(
            "Sent to {} ({}), message id {}",
            name.as_deref().unwrap_or(&jid_str),
            sent.to,
            sent.message_id
        ))
    }

    /// Download `hit` to `path`. If that fails (WhatsApp's CDN drops media
    /// after a while), ask the phone to re-upload it and try once more.
    async fn fetch_media(&self, id: &str, hit: &MediaHit, path: &Path) -> Result<(), String> {
        let m = &hit.media;
        let params = |direct_path: &str| DownloadParams {
            direct_path: direct_path.into(),
            media_key: m.media_key.clone(),
            file_sha256: m.file_sha256.clone(),
            file_enc_sha256: m.file_enc_sha256.clone(),
            file_length: m.file_length,
            media_type: download_type(&m.media_type),
        };
        let Err(first) = self.download_to(&params(&m.direct_path), path).await else {
            return Ok(());
        };
        let direct_path = self.request_reupload(id, hit).await.map_err(|e| {
            invalid(format!(
                "download failed ({first}), and asking the phone to re-upload it failed: {e}"
            ))
        })?;
        let _ = self.store.set_media_path(&hit.chat_jid, id, &direct_path);
        self.download_to(&params(&direct_path), path)
            .await
            .map_err(internal)
    }

    /// Stream into `<path>.part` and move it into place only once the file
    /// is complete and verified.
    async fn download_to(&self, params: &DownloadParams, path: &Path) -> anyhow::Result<()> {
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let file = std::fs::File::create(&part)?;
        if let Err(e) = self.client.download_to_writer(params, file).await {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, path)?;
        Ok(())
    }

    async fn request_reupload(&self, id: &str, hit: &MediaHit) -> Result<String, String> {
        let media_key = hit
            .media
            .media_key
            .as_deref()
            .ok_or("the media is not end-to-end encrypted, so it cannot be re-requested")?;
        let chat: Jid = hit.chat_jid.parse().map_err(internal)?;
        let participant: Option<Jid> = hit
            .sender_jid
            .as_deref()
            .filter(|_| hit.chat_jid.ends_with("@g.us"))
            .and_then(|s| s.parse().ok());
        let result = self
            .client
            .media_reupload()
            .request(&MediaReuploadRequest {
                msg_id: id,
                chat_jid: &chat,
                media_key,
                is_from_me: hit.from_me,
                participant: participant.as_ref(),
            })
            .await
            .map_err(|e| format!("{e} (the phone needs to be online)"))?;
        match result {
            MediaRetryResult::Success { direct_path } => Ok(direct_path),
            MediaRetryResult::NotFound => Err("the phone no longer has this file".into()),
            other => Err(format!("the phone answered {other:?}")),
        }
    }
}

#[tool_router]
impl WhatsAppServer {
    #[tool(description = "Connection state of the linked WhatsApp device and size of the local message archive.")]
    async fn whatsapp_status(&self) -> ToolResult {
        let stats = self.store.stats().map_err(internal)?;
        let mut status = json!({
            "archive": {
                "chats": stats.chats,
                "messages": stats.messages,
                "oldest_message": stats.oldest_message.map(fmt_time),
                "newest_message": stats.newest_message.map(fmt_time),
            }
        });
        let client = &self.client;
        status["logged_in"] = json!(client.is_logged_in());
        status["connected"] = json!(client.is_connected());
        status["own_number"] = json!(client.pn().map(|j| format!("+{}", j.user)));
        if client.pn().is_none() {
            status["hint"] = json!(
                "Not linked yet. Run `zap-mcp setup` (or just open the program) in a terminal to link your phone."
            );
        }
        text_result(serde_json::to_string_pretty(&status).map_err(internal)?)
    }

    #[tool(description = "List WhatsApp chats (1:1 and groups), most recently active first, with the last message preview. Use the returned jid or name with get_messages.")]
    async fn list_chats(
        &self,
        Parameters(p): Parameters<ListChatsParams>,
    ) -> ToolResult {
        let chats = self
            .store
            .list_chats(p.query.as_deref(), p.limit.unwrap_or(30).min(500))
            .map_err(internal)?;
        let chats: Vec<_> = chats
            .into_iter()
            .map(|c| {
                json!({
                    "jid": c.jid,
                    "name": c.name,
                    "is_group": c.is_group,
                    "last_activity": c.last_activity.filter(|t| *t > 0).map(fmt_time),
                    "last_message": c.last_message.map(|t| truncate(&t, 120)),
                })
            })
            .collect();
        text_result(serde_json::to_string_pretty(&chats).map_err(internal)?)
    }

    #[tool(description = "Read messages from one WhatsApp chat in chronological order. Returns the most recent `limit` messages inside the optional after/before window.")]
    async fn get_messages(
        &self,
        Parameters(p): Parameters<GetMessagesParams>,
    ) -> ToolResult {
        let (jid, name) = self.resolve_chat(&p.chat)?;
        let after = parse_time("after", p.after.as_deref())?;
        let before = parse_time("before", p.before.as_deref())?;
        let msgs = self
            .store
            .messages(&jid, after, before, p.limit.unwrap_or(50).min(1000))
            .map_err(internal)?;
        let header = format!(
            "Chat: {} ({jid}), {} message(s)\n",
            name.as_deref().unwrap_or("?"),
            msgs.len()
        );
        text_result(header + &render(&msgs, false))
    }

    #[tool(description = "Full-text search across all WhatsApp messages (or one chat). Newest matches first.")]
    async fn search_messages(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> ToolResult {
        if p.query.trim().is_empty() {
            return Err(invalid("query must not be empty"));
        }
        let chat = p.chat.as_deref().map(|c| self.resolve_chat(c)).transpose()?;
        let after = parse_time("after", p.after.as_deref())?;
        let before = parse_time("before", p.before.as_deref())?;
        let msgs = self
            .store
            .search(
                &p.query,
                chat.as_ref().map(|(jid, _)| jid.as_str()),
                after,
                before,
                p.limit.unwrap_or(50).min(500),
            )
            .map_err(internal)?;
        if msgs.is_empty() {
            return text_result("No matches.");
        }
        text_result(render(&msgs, true))
    }

    #[tool(description = "Recent WhatsApp messages across all chats (default: last 24 hours), chronological. Good for 'what did I miss?'.")]
    async fn recent_messages(
        &self,
        Parameters(p): Parameters<RecentParams>,
    ) -> ToolResult {
        let since = parse_time("since", p.since.as_deref())?
            .unwrap_or_else(|| chrono::Utc::now().timestamp() - 24 * 3600);
        let msgs = self
            .store
            .recent(Some(since), p.limit.unwrap_or(100).min(1000))
            .map_err(internal)?;
        if msgs.is_empty() {
            return text_result(format!("No messages since {}.", fmt_time(since)));
        }
        text_result(render(&msgs, true))
    }

    #[tool(description = "Send a text message on WhatsApp from the user's account. Only call this when the user explicitly asked to send this exact message to this recipient.")]
    async fn send_message(
        &self,
        Parameters(p): Parameters<SendParams>,
    ) -> ToolResult {
        let client = &self.client;
        if !client.is_logged_in() {
            return Err(invalid("WhatsApp is not linked; run `zap-mcp setup` in a terminal"));
        }
        if p.text.trim().is_empty() {
            return Err(invalid("text must not be empty"));
        }
        self.send_and_archive(&p.to, wa::Message::text(p.text)).await
    }

    #[tool(description = "Send a local file on WhatsApp from the user's account: a photo, video, audio file or any document, with an optional caption. Only call this when the user explicitly asked to send this exact file to this recipient.")]
    async fn send_media(
        &self,
        Parameters(p): Parameters<SendMediaParams>,
    ) -> ToolResult {
        let client = &self.client;
        if !client.is_logged_in() {
            return Err(invalid("WhatsApp is not linked; run `zap-mcp setup` in a terminal"));
        }
        let path = Path::new(&p.path);
        let (mimetype, mut kind) = file_type(path);
        if p.as_document == Some(true) {
            kind = MediaType::Document;
        }
        let size = std::fs::metadata(path)
            .map_err(|e| invalid(format!("cannot read {:?}: {e}", p.path)))?
            .len();
        if kind == MediaType::Document && size > MAX_DOCUMENT_BYTES {
            return Err(invalid(format!("file is {}; documents can be at most 100 MB", fmt_size(size))));
        }
        if kind != MediaType::Document && size > MAX_MEDIA_BYTES {
            return Err(invalid(format!(
                "file is {}; WhatsApp allows at most 16 MB for photos, videos and audio \
                 (pass as_document: true to send it as a file instead)",
                fmt_size(size)
            )));
        }
        let caption = p.caption.filter(|c| !c.trim().is_empty());
        if kind == MediaType::Audio && caption.is_some() {
            return Err(invalid(
                "audio messages cannot have a caption; send the text separately with send_message",
            ));
        }
        let data = std::fs::read(path).map_err(|e| invalid(format!("cannot read {:?}: {e}", p.path)))?;

        let upload = client
            .upload(data, kind, UploadOptions::default())
            .await
            .map_err(internal)?;
        let mimetype = Some(mimetype.to_string());
        let message = match kind {
            MediaType::Image => media::image_message(
                upload,
                ImageOptions { caption, mimetype, ..Default::default() },
            ),
            MediaType::Video => media::video_message(
                upload,
                VideoOptions { caption, mimetype, ..Default::default() },
            ),
            MediaType::Audio => {
                media::audio_message(upload, AudioOptions { mimetype, ..Default::default() })
            }
            _ => media::document_message(
                upload,
                DocumentOptions {
                    caption,
                    mimetype,
                    file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()),
                    ..Default::default()
                },
            ),
        };
        self.send_and_archive(&p.to, message).await
    }

    #[tool(description = "Download the photo, video, voice note, audio, document or sticker of a WhatsApp message and save it as a local file. Pass the id shown as `[image id=…]` in get_messages, search_messages or recent_messages. Returns the file path; photos and stickers are also returned as images so you can look at them.")]
    async fn download_media(
        &self,
        Parameters(p): Parameters<DownloadMediaParams>,
    ) -> Result<CallToolResult, String> {
        let id = p.message_id.trim().trim_start_matches("id=");
        let chat = p.chat.as_deref().map(|c| self.resolve_chat(c)).transpose()?;
        let hits = self
            .store
            .find_media(id, chat.as_ref().map(|(jid, _)| jid.as_str()))
            .map_err(internal)?;
        let hit = match hits.as_slice() {
            [one] => one,
            [] => {
                return Err(invalid(format!(
                    "no downloadable media for message id {id:?}; only media archived by \
                     zap-mcp 0.4 or later can be downloaded"
                )));
            }
            _ => return Err(invalid(format!("message id {id:?} is in several chats; pass `chat`"))),
        };
        let dir = match p.save_dir.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) if Path::new(d).is_absolute() => PathBuf::from(d),
            Some(d) => return Err(invalid(format!("save_dir must be an absolute path, got {d:?}"))),
            None => self.media_dir.clone(),
        };
        std::fs::create_dir_all(&dir)
            .map_err(|e| invalid(format!("cannot create {}: {e}", dir.display())))?;
        let path = dir.join(download_file_name(id, &hit.media));
        // Downloaded before: names are unique per message.
        let cached = std::fs::metadata(&path).is_ok_and(|m| m.len() == hit.media.file_length);
        if !cached {
            self.fetch_media(id, hit, &path).await?;
        }

        let m = &hit.media;
        let mimetype = m.mimetype.as_deref().unwrap_or("application/octet-stream");
        let mut content = vec![ContentBlock::text(format!(
            "Saved {} ({}, {mimetype}) to {}",
            m.media_type,
            fmt_size(m.file_length),
            path.display()
        ))];
        if INLINE_IMAGE_TYPES.contains(&mimetype) && m.file_length <= MAX_INLINE_IMAGE_BYTES {
            let data = std::fs::read(&path).map_err(internal)?;
            let data = base64::engine::general_purpose::STANDARD.encode(data);
            content.push(ContentBlock::image(data, mimetype));
        }
        Ok(CallToolResult::success(content))
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WhatsAppServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(mimetype: &str, file_name: Option<&str>) -> MediaRef {
        MediaRef {
            media_type: "document".into(),
            mimetype: Some(mimetype.into()),
            file_name: file_name.map(str::to_string),
            direct_path: "/d".into(),
            media_key: None,
            file_sha256: vec![],
            file_enc_sha256: None,
            file_length: 0,
        }
    }

    #[test]
    fn download_names_are_safe_and_typed() {
        assert_eq!(download_file_name("3EB0AB", &media("image/jpeg", None)), "3EB0AB.jpg");
        assert_eq!(
            download_file_name("3EB0AB", &media("audio/ogg; codecs=opus", None)),
            "3EB0AB.ogg"
        );
        assert_eq!(download_file_name("x", &media("application/x-what", None)), "x.bin");
        assert_eq!(
            download_file_name("../ab", &media("application/pdf", Some("../../etc/Q3: plan.pdf"))),
            "ab-_.._etc_Q3_ plan.pdf"
        );
    }

    #[test]
    fn files_are_sent_by_extension() {
        assert_eq!(file_type(Path::new("/a/B.JPG")), ("image/jpeg", MediaType::Image));
        assert_eq!(file_type(Path::new("/a/clip.mp4")), ("video/mp4", MediaType::Video));
        assert_eq!(file_type(Path::new("/a/v.opus")).1, MediaType::Audio);
        assert_eq!(file_type(Path::new("/a/r.pdf")), ("application/pdf", MediaType::Document));
        assert_eq!(file_type(Path::new("/a/noext")).1, MediaType::Document);
    }
}
