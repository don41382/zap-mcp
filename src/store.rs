//! Local message archive. WhatsApp only delivers history once (at link time)
//! and live messages as they arrive, so everything the MCP tools can answer
//! comes from this SQLite file.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;

CREATE TABLE IF NOT EXISTS chats (
    jid      TEXT PRIMARY KEY,
    name     TEXT,
    is_group INTEGER NOT NULL DEFAULT 0,
    last_ts  INTEGER
);

CREATE TABLE IF NOT EXISTS contacts (
    jid       TEXT PRIMARY KEY,
    full_name TEXT,
    push_name TEXT
);

-- WhatsApp addresses many users by an opaque LID instead of their phone
-- number. We keep everything keyed by the phone-number JID once known.
CREATE TABLE IF NOT EXISTS jid_map (
    lid TEXT PRIMARY KEY,
    pn  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    chat_jid    TEXT NOT NULL,
    id          TEXT NOT NULL,
    sender_jid  TEXT,
    sender_name TEXT,
    from_me     INTEGER NOT NULL,
    ts          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    text        TEXT,
    PRIMARY KEY (chat_jid, id)
);
CREATE INDEX IF NOT EXISTS messages_chat_ts ON messages (chat_jid, ts);
CREATE INDEX IF NOT EXISTS messages_ts ON messages (ts);
"#;

/// Display name for a chat: explicit chat/group name, then address-book name,
/// then the contact's self-chosen push name.
const CHAT_NAME_SQL: &str = "COALESCE(c.name, ct.full_name, ct.push_name)";

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub chat_jid: String,
    pub id: String,
    pub sender_jid: Option<String>,
    pub sender_name: Option<String>,
    pub from_me: bool,
    pub ts: i64,
    pub kind: String,
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ChatSummary {
    pub jid: String,
    pub name: Option<String>,
    pub is_group: bool,
    pub last_activity: Option<i64>,
    pub last_message: Option<String>,
}

#[derive(Debug)]
pub struct StoredMessage {
    pub chat_jid: String,
    pub chat_name: Option<String>,
    pub sender: String,
    pub from_me: bool,
    pub ts: i64,
    pub kind: String,
    pub text: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub chats: i64,
    pub messages: i64,
    pub oldest_message: Option<i64>,
    pub newest_message: Option<i64>,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> Result<T> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&mut conn)?)
    }

    /// Run several writes in one transaction (history sync chunks carry
    /// thousands of rows).
    pub fn batch(&self, f: impl FnOnce(&Batch<'_>) -> rusqlite::Result<()>) -> Result<()> {
        self.with(|conn| {
            let tx = conn.transaction()?;
            f(&Batch { tx: &tx })?;
            tx.commit()
        })
    }

    pub fn canonical(&self, jid: &str) -> Result<String> {
        self.with(|conn| canonical(conn, jid))
    }

    pub fn stats(&self) -> Result<Stats> {
        self.with(|conn| {
            conn.query_row(
                "SELECT (SELECT COUNT(*) FROM chats), COUNT(*), MIN(ts), MAX(ts) FROM messages",
                [],
                |r| {
                    Ok(Stats {
                        chats: r.get(0)?,
                        messages: r.get(1)?,
                        oldest_message: r.get(2)?,
                        newest_message: r.get(3)?,
                    })
                },
            )
        })
    }

    pub fn list_chats(&self, query: Option<&str>, limit: u32) -> Result<Vec<ChatSummary>> {
        let pattern = query.map(like_pattern);
        self.with(|conn| {
            let sql = format!(
                "SELECT c.jid, {CHAT_NAME_SQL}, c.is_group, c.last_ts,
                        (SELECT CASE WHEN m.kind = 'text' THEN m.text
                                     WHEN m.text IS NULL THEN '[' || m.kind || ']'
                                     ELSE '[' || m.kind || '] ' || m.text END
                           FROM messages m
                          WHERE m.chat_jid = c.jid ORDER BY m.ts DESC LIMIT 1)
                   FROM chats c LEFT JOIN contacts ct ON ct.jid = c.jid
                  WHERE ?1 IS NULL OR {CHAT_NAME_SQL} LIKE ?1 ESCAPE '\\' OR c.jid LIKE ?1 ESCAPE '\\'
                  ORDER BY c.last_ts DESC NULLS LAST
                  LIMIT ?2"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params![pattern, limit], |r| {
                Ok(ChatSummary {
                    jid: r.get(0)?,
                    name: r.get(1)?,
                    is_group: r.get(2)?,
                    last_activity: r.get(3)?,
                    last_message: r.get(4)?,
                })
            })?;
            rows.collect()
        })
    }

    /// Resolve user input (JID, phone number, or chat name) to chat JIDs.
    pub fn find_chats(&self, input: &str) -> Result<Vec<ChatSummary>> {
        let input = input.trim();
        let digits: String = input.chars().filter(|c| c.is_ascii_digit()).collect();
        let looks_like_phone = !digits.is_empty()
            && input
                .chars()
                .all(|c| c.is_ascii_digit() || " +-()".contains(c));

        if input.contains('@') || looks_like_phone {
            let jid = if input.contains('@') {
                input.to_string()
            } else {
                format!("{digits}@s.whatsapp.net")
            };
            let jid = self.canonical(&jid)?;
            let exact = self.with(|conn| {
                conn.query_row(
                    &format!(
                        "SELECT c.jid, {CHAT_NAME_SQL}, c.is_group, c.last_ts
                           FROM chats c LEFT JOIN contacts ct ON ct.jid = c.jid
                          WHERE c.jid = ?1"
                    ),
                    [&jid],
                    |r| {
                        Ok(ChatSummary {
                            jid: r.get(0)?,
                            name: r.get(1)?,
                            is_group: r.get(2)?,
                            last_activity: r.get(3)?,
                            last_message: None,
                        })
                    },
                )
                .optional()
            })?;
            return Ok(exact.into_iter().collect());
        }

        let matches = self.list_chats(Some(input), 20)?;
        // Prefer an exact (case-insensitive) name match over substring hits.
        let exact: Vec<_> = matches
            .iter()
            .filter(|c| c.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(input)))
            .map(|c| c.jid.clone())
            .collect();
        if exact.len() == 1 {
            return Ok(matches.into_iter().filter(|c| c.jid == exact[0]).collect());
        }
        Ok(matches)
    }

    pub fn messages(
        &self,
        chat_jid: &str,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<StoredMessage>> {
        // Newest `limit` messages in the window, returned oldest-first.
        let mut out = self.query_messages(
            "m.chat_jid = ?1 AND (?2 IS NULL OR m.ts >= ?2) AND (?3 IS NULL OR m.ts < ?3)",
            params![chat_jid, after, before, limit],
        )?;
        out.reverse();
        Ok(out)
    }

    pub fn search(
        &self,
        query: &str,
        chat_jid: Option<&str>,
        after: Option<i64>,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<StoredMessage>> {
        self.query_messages(
            "m.text LIKE ?1 ESCAPE '\\' AND (?2 IS NULL OR m.chat_jid = ?2)
             AND (?3 IS NULL OR m.ts >= ?3) AND (?4 IS NULL OR m.ts < ?4)",
            params![like_pattern(query), chat_jid, after, before, limit],
        )
    }

    pub fn recent(&self, after: Option<i64>, limit: u32) -> Result<Vec<StoredMessage>> {
        let mut out = self.query_messages(
            "(?1 IS NULL OR m.ts >= ?1) AND m.chat_jid NOT LIKE '%@newsletter'",
            params![after, limit],
        )?;
        out.reverse();
        Ok(out)
    }

    /// `filter` is a WHERE clause whose last positional parameter is the LIMIT.
    fn query_messages(
        &self,
        filter: &str,
        params: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<StoredMessage>> {
        let limit_idx = params.len();
        self.with(|conn| {
            let sql = format!(
                "SELECT m.chat_jid, {CHAT_NAME_SQL},
                        COALESCE(sc.full_name, sc.push_name, m.sender_name, m.sender_jid, ''),
                        m.from_me, m.ts, m.kind, m.text
                   FROM messages m
                   LEFT JOIN chats c ON c.jid = m.chat_jid
                   LEFT JOIN contacts ct ON ct.jid = m.chat_jid
                   LEFT JOIN contacts sc ON sc.jid = m.sender_jid
                  WHERE {filter}
                  ORDER BY m.ts DESC
                  LIMIT ?{limit_idx}"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(params, |r| {
                Ok(StoredMessage {
                    chat_jid: r.get(0)?,
                    chat_name: r.get(1)?,
                    sender: r.get(2)?,
                    from_me: r.get(3)?,
                    ts: r.get(4)?,
                    kind: r.get(5)?,
                    text: r.get(6)?,
                })
            })?;
            rows.collect()
        })
    }
}

/// Write operations, usable both standalone (via [`Store::batch`]) and inside
/// a history-sync transaction.
pub struct Batch<'a> {
    tx: &'a rusqlite::Transaction<'a>,
}

impl Batch<'_> {
    pub fn canonical(&self, jid: &str) -> rusqlite::Result<String> {
        canonical(self.tx, jid)
    }

    pub fn upsert_chat(
        &self,
        jid: &str,
        name: Option<&str>,
        is_group: bool,
        ts: Option<i64>,
    ) -> rusqlite::Result<()> {
        let jid = self.canonical(jid)?;
        self.tx.execute(
            "INSERT INTO chats (jid, name, is_group, last_ts) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (jid) DO UPDATE SET
                name    = COALESCE(excluded.name, chats.name),
                last_ts = MAX(COALESCE(chats.last_ts, 0), COALESCE(excluded.last_ts, 0))",
            params![jid, name.filter(|n| !n.is_empty()), is_group, ts],
        )?;
        Ok(())
    }

    pub fn upsert_contact(
        &self,
        jid: &str,
        full_name: Option<&str>,
        push_name: Option<&str>,
    ) -> rusqlite::Result<()> {
        let full_name = full_name.filter(|n| !n.is_empty());
        let push_name = push_name.filter(|n| !n.is_empty());
        if full_name.is_none() && push_name.is_none() {
            return Ok(());
        }
        let jid = self.canonical(jid)?;
        self.tx.execute(
            "INSERT INTO contacts (jid, full_name, push_name) VALUES (?1, ?2, ?3)
             ON CONFLICT (jid) DO UPDATE SET
                full_name = COALESCE(excluded.full_name, contacts.full_name),
                push_name = COALESCE(excluded.push_name, contacts.push_name)",
            params![jid, full_name, push_name],
        )?;
        Ok(())
    }

    /// Record a LID → phone-number mapping and fold any rows stored under the
    /// LID into the phone-number JID.
    pub fn add_mapping(&self, lid: &str, pn: &str) -> rusqlite::Result<()> {
        if lid == pn || !lid.ends_with("@lid") || !pn.ends_with("@s.whatsapp.net") {
            return Ok(());
        }
        let known: Option<String> = self
            .tx
            .query_row("SELECT pn FROM jid_map WHERE lid = ?1", [lid], |r| r.get(0))
            .optional()?;
        if known.as_deref() == Some(pn) {
            return Ok(());
        }
        self.tx.execute(
            "INSERT INTO jid_map (lid, pn) VALUES (?1, ?2)
             ON CONFLICT (lid) DO UPDATE SET pn = excluded.pn",
            [lid, pn],
        )?;
        self.tx.execute(
            "INSERT INTO chats (jid, name, is_group, last_ts)
             SELECT ?2, name, is_group, last_ts FROM chats WHERE jid = ?1
             ON CONFLICT (jid) DO UPDATE SET
                name    = COALESCE(chats.name, excluded.name),
                last_ts = MAX(COALESCE(chats.last_ts, 0), COALESCE(excluded.last_ts, 0))",
            [lid, pn],
        )?;
        self.tx.execute("DELETE FROM chats WHERE jid = ?1", [lid])?;
        self.tx.execute(
            "INSERT INTO contacts (jid, full_name, push_name)
             SELECT ?2, full_name, push_name FROM contacts WHERE jid = ?1
             ON CONFLICT (jid) DO UPDATE SET
                full_name = COALESCE(contacts.full_name, excluded.full_name),
                push_name = COALESCE(contacts.push_name, excluded.push_name)",
            [lid, pn],
        )?;
        self.tx.execute("DELETE FROM contacts WHERE jid = ?1", [lid])?;
        self.tx.execute(
            "UPDATE OR IGNORE messages SET chat_jid = ?2 WHERE chat_jid = ?1",
            [lid, pn],
        )?;
        self.tx.execute("DELETE FROM messages WHERE chat_jid = ?1", [lid])?;
        self.tx.execute(
            "UPDATE messages SET sender_jid = ?2 WHERE sender_jid = ?1",
            [lid, pn],
        )?;
        Ok(())
    }

    pub fn insert_message(&self, m: &NewMessage) -> rusqlite::Result<()> {
        let chat = self.canonical(&m.chat_jid)?;
        let sender = m.sender_jid.as_deref().map(|s| self.canonical(s)).transpose()?;
        self.tx.execute(
            "INSERT INTO messages (chat_jid, id, sender_jid, sender_name, from_me, ts, kind, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (chat_jid, id) DO NOTHING",
            params![
                chat,
                m.id,
                sender,
                m.sender_name.as_deref().filter(|n| !n.is_empty()),
                m.from_me,
                m.ts,
                m.kind,
                m.text
            ],
        )?;
        self.tx.execute(
            "UPDATE chats SET last_ts = MAX(COALESCE(last_ts, 0), ?2) WHERE jid = ?1",
            params![chat, m.ts],
        )?;
        Ok(())
    }

    pub fn edit_message(&self, chat_jid: &str, id: &str, text: &str) -> rusqlite::Result<()> {
        let chat = self.canonical(chat_jid)?;
        self.tx.execute(
            "UPDATE messages SET text = ?3 WHERE chat_jid = ?1 AND id = ?2",
            params![chat, id, text],
        )?;
        Ok(())
    }

    /// "Delete for everyone": drop the content, keep a tombstone.
    pub fn revoke_message(&self, chat_jid: &str, id: &str) -> rusqlite::Result<()> {
        let chat = self.canonical(chat_jid)?;
        self.tx.execute(
            "UPDATE messages SET kind = 'deleted', text = NULL WHERE chat_jid = ?1 AND id = ?2",
            params![chat, id],
        )?;
        Ok(())
    }
}

fn canonical(conn: &Connection, jid: &str) -> rusqlite::Result<String> {
    if !jid.ends_with("@lid") {
        return Ok(jid.to_string());
    }
    let pn: Option<String> = conn
        .query_row("SELECT pn FROM jid_map WHERE lid = ?1", [jid], |r| r.get(0))
        .optional()?;
    Ok(pn.unwrap_or_else(|| jid.to_string()))
}

fn like_pattern(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(chat: &str, id: &str, ts: i64, text: &str) -> NewMessage {
        NewMessage {
            chat_jid: chat.into(),
            id: id.into(),
            sender_jid: Some(chat.into()),
            sender_name: Some("Alice".into()),
            from_me: false,
            ts,
            kind: "text".into(),
            text: Some(text.into()),
        }
    }

    fn temp_store() -> Store {
        // Tests run in parallel; a counter keeps their databases apart.
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "wa-mcp-test-{}-{n}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        Store::open(&path).unwrap()
    }

    #[test]
    fn lid_rows_fold_into_phone_number() {
        let s = temp_store();
        let lid = "123@lid";
        let pn = "4915112345678@s.whatsapp.net";
        s.batch(|b| {
            b.upsert_chat(lid, None, false, Some(10))?;
            b.upsert_contact(lid, None, Some("Alice"))?;
            b.insert_message(&msg(lid, "a", 10, "hello from lid"))
        })
        .unwrap();
        s.batch(|b| {
            b.add_mapping(lid, pn)?;
            b.insert_message(&msg(lid, "b", 20, "after mapping"))
        })
        .unwrap();

        let chats = s.list_chats(None, 10).unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].jid, pn);
        assert_eq!(chats[0].name.as_deref(), Some("Alice"));
        assert_eq!(chats[0].last_message.as_deref(), Some("after mapping"));

        let msgs = s.messages(pn, None, None, 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text.as_deref(), Some("hello from lid"));
    }

    #[test]
    fn search_finds_by_name_and_phone() {
        let s = temp_store();
        let pn = "4915112345678@s.whatsapp.net";
        s.batch(|b| {
            b.upsert_chat(pn, None, false, None)?;
            b.upsert_contact(pn, Some("Bob Builder"), None)?;
            b.insert_message(&msg(pn, "x", 5, "50% off_sale"))
        })
        .unwrap();
        assert_eq!(s.find_chats("bob builder").unwrap()[0].jid, pn);
        assert_eq!(s.find_chats("+49 151 12345678").unwrap()[0].jid, pn);
        assert_eq!(s.search("50% off_", None, None, None, 10).unwrap().len(), 1);
        assert!(s.search("50%x", None, None, None, 10).unwrap().is_empty());
    }

    #[test]
    fn revoke_clears_text() {
        let s = temp_store();
        let pn = "1@s.whatsapp.net";
        s.batch(|b| {
            b.upsert_chat(pn, None, false, None)?;
            b.insert_message(&msg(pn, "x", 5, "secret"))?;
            b.revoke_message(pn, "x")
        })
        .unwrap();
        let m = &s.messages(pn, None, None, 10).unwrap()[0];
        assert_eq!(m.kind, "deleted");
        assert!(m.text.is_none());
    }
}
