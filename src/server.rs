//! MCP tools over the local archive (and, optionally, sending through the
//! linked WhatsApp session). One instance per MCP session, all sharing the
//! daemon's store and WhatsApp client.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, Local, NaiveDate, TimeZone};
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::json;
use whatsapp_rust::media::{self, ImageOptions};
use whatsapp_rust::prelude::*;
use whatsapp_rust::wacore::download::MediaType;
use whatsapp_rust::UploadOptions;

use crate::store::{NewMessage, StoredMessage, Store};

pub const INSTRUCTIONS: &str = "Access to the user's personal WhatsApp messages via a locally linked device. \
     Messages are private: quote only what the user needs. Times are in the user's \
     local timezone. Start with list_chats or recent_messages; use get_messages for \
     a conversation and search_messages for keywords.";

#[derive(Clone)]
pub struct WhatsAppServer {
    store: Store,
    client: Arc<Client>,
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
pub struct SendImageParams {
    /// Recipient: a chat name, a phone number with country code, or a JID.
    pub to: String,
    /// Absolute path to a local image file (JPEG, PNG, WebP or GIF).
    pub path: String,
    /// Optional caption shown under the image.
    pub caption: Option<String>,
}

/// WhatsApp rejects images above 16 MB.
const MAX_IMAGE_BYTES: u64 = 16 * 1024 * 1024;

fn image_mimetype(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => return None,
    })
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
    match (m.kind.as_str(), &m.text) {
        ("text", Some(t)) => t.clone(),
        ("deleted", _) => "[deleted]".into(),
        (kind, Some(t)) => format!("[{kind}] {t}"),
        (kind, None) => format!("[{kind}]"),
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
    pub fn new(store: Store, client: Arc<Client>, allow_send: bool) -> Self {
        let mut tool_router = Self::tool_router();
        if !allow_send {
            tool_router.remove_route("send_message");
            tool_router.remove_route("send_image");
        }
        Self {
            store,
            client,
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

    /// Send `message` to `to` and archive it as `kind`/`text`, since our own
    /// sends are not echoed back to this device.
    async fn send_and_archive(
        &self,
        to: &str,
        message: wa::Message,
        kind: &str,
        text: Option<String>,
    ) -> ToolResult {
        let client = &self.client;
        let (jid, jid_str, name) = self.resolve_recipient(to)?;
        let sent = client.send_message(jid, message).await.map_err(internal)?;

        let own = client.pn().map(|j| j.to_non_ad().to_string());
        let now = chrono::Utc::now().timestamp();
        let _ = self.store.batch(|b| {
            b.upsert_chat(&jid_str, None, jid_str.ends_with("@g.us"), Some(now))?;
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
        self.send_and_archive(&p.to, wa::Message::text(p.text.clone()), "text", Some(p.text))
            .await
    }

    #[tool(description = "Send an image file on WhatsApp from the user's account, with an optional caption. Only call this when the user explicitly asked to send this exact image to this recipient.")]
    async fn send_image(
        &self,
        Parameters(p): Parameters<SendImageParams>,
    ) -> ToolResult {
        let client = &self.client;
        if !client.is_logged_in() {
            return Err(invalid("WhatsApp is not linked; run `zap-mcp setup` in a terminal"));
        }
        let path = std::path::Path::new(&p.path);
        let mimetype = image_mimetype(path).ok_or_else(|| {
            invalid(format!("{:?} is not a supported image; use JPEG, PNG, WebP or GIF", p.path))
        })?;
        let size = std::fs::metadata(path)
            .map_err(|e| invalid(format!("cannot read {:?}: {e}", p.path)))?
            .len();
        if size > MAX_IMAGE_BYTES {
            return Err(invalid(format!("image is {size} bytes; WhatsApp allows at most 16 MB")));
        }
        let data = std::fs::read(path).map_err(|e| invalid(format!("cannot read {:?}: {e}", p.path)))?;

        let upload = client
            .upload(data, MediaType::Image, UploadOptions::default())
            .await
            .map_err(internal)?;
        let caption = p.caption.filter(|c| !c.trim().is_empty());
        let message = media::image_message(
            upload,
            ImageOptions {
                caption: caption.clone(),
                mimetype: Some(mimetype.into()),
                ..Default::default()
            },
        );
        self.send_and_archive(&p.to, message, "image", caption).await
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
