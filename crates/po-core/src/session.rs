//! Append-only session log.
//!
//! A session is a JSONL file: one JSON object per line, appended as the
//! conversation progresses. Forking a session copies the prefix of entries and
//! records the parent session id, so any branch can be replayed on its own.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::ChatMessage;

/// Errors produced by the session store.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Malformed JSON in the log.
    #[error("invalid session entry: {0}")]
    Json(#[from] serde_json::Error),
    /// The requested session does not exist.
    #[error("session not found: {0}")]
    NotFound(String),
}

/// One recorded event in a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEntry {
    /// A transcript message.
    Message {
        /// The message itself.
        message: ChatMessage,
    },
    /// A state-machine event, recorded so sessions can be replayed.
    Event {
        /// The event name.
        name: String,
        /// The event payload.
        payload: serde_json::Value,
    },
    /// A compaction marker: older entries were summarized.
    Compaction {
        /// Summary replacing the compacted prefix.
        summary: String,
        /// How many entries were folded into the summary.
        compacted_entries: usize,
    },
    /// A branch marker: this session continues another one.
    Fork {
        /// Identifier of the parent session.
        parent: String,
        /// Entry index in the parent where the fork happened.
        at_entry: usize,
    },
}

/// Metadata stored in the first line of every session file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
    /// Stable session identifier.
    pub id: String,
    /// Human-readable title, defaulting to the first user message.
    pub title: String,
    /// Session this one was forked from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Entry index in the parent this session branched from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_at: Option<usize>,
    /// Creation timestamp (unix seconds).
    pub created_at: u64,
}

impl SessionHeader {
    /// Create a fresh header with the current timestamp.
    #[must_use]
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            parent: None,
            forked_at: None,
            created_at: now_unix(),
        }
    }
}

/// Current unix timestamp in seconds.
#[must_use]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// On-disk session: a JSONL file plus its header.
#[derive(Debug)]
pub struct Session {
    header: SessionHeader,
    path: PathBuf,
}

impl Session {
    /// Open (or create) a session file at `path`.
    ///
    /// # Errors
    /// Returns an error when the file cannot be created or read.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        if path.exists() {
            let header = read_header(&path)?;
            Ok(Self { header, path })
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("session")
                .to_string();
            let header = SessionHeader::new(id, "New session");
            write_header(&path, &header)?;
            Ok(Self { header, path })
        }
    }

    /// The session header.
    #[must_use]
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// The session identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.header.id
    }

    /// Path of the backing file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append an entry to the log.
    ///
    /// # Errors
    /// Returns an error when the entry cannot be serialized or written.
    pub fn append(&mut self, entry: &SessionEntry) -> Result<(), SessionError> {
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        let line = serde_json::to_string(entry)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }

    /// Read every entry after the header.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read or contains invalid JSON.
    pub fn entries(&self) -> Result<Vec<SessionEntry>, SessionError> {
        let file = File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            // The first line is the session header, not an entry.
            if index == 0 {
                continue;
            }
            if line.trim().is_empty() {
                continue;
            }
            entries.push(serde_json::from_str(&line)?);
        }
        Ok(entries)
    }

    /// Read only the transcript messages.
    ///
    /// # Errors
    /// Returns an error when the file cannot be read or contains invalid JSON.
    pub fn messages(&self) -> Result<Vec<ChatMessage>, SessionError> {
        Ok(self
            .entries()?
            .into_iter()
            .filter_map(|entry| match entry {
                SessionEntry::Message { message } => Some(message),
                _ => None,
            })
            .collect())
    }

    /// Fork this session into `dest`, copying the first `at_entry` entries.
    ///
    /// # Errors
    /// Returns an error when the source cannot be read or the destination written.
    pub fn fork(&self, dest: impl AsRef<Path>, at_entry: usize) -> Result<Session, SessionError> {
        let dest = dest.as_ref().to_path_buf();
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let entries = self.entries()?;
        let take = at_entry.min(entries.len());
        let mut header = self.header.clone();
        header.id = dest
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
            .to_string();
        header.parent = Some(self.header.id.clone());
        header.forked_at = Some(take);
        header.title = format!("{} (fork)", self.header.title);
        write_header(&dest, &header)?;

        let mut file = OpenOptions::new().create(true).append(true).open(&dest)?;
        for entry in &entries[..take] {
            let line = serde_json::to_string(entry)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        file.write_all(
            serde_json::to_string(&SessionEntry::Fork {
                parent: self.header.id.clone(),
                at_entry: take,
            })?
            .as_bytes(),
        )?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(Session { header, path: dest })
    }
}

fn write_header(path: &Path, header: &SessionHeader) -> Result<(), SessionError> {
    let mut file = File::create(path)?;
    let line = serde_json::to_string(header)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(())
}

fn read_header(path: &Path) -> Result<SessionHeader, SessionError> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.trim().is_empty() {
        return Err(SessionError::NotFound(path.display().to_string()));
    }
    Ok(serde_json::from_str(&line)?)
}

/// Filesystem-backed store of all sessions.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Create a store rooted at `root`, creating the directory if needed.
    ///
    /// # Errors
    /// Returns an error when the directory cannot be created.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, SessionError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Default store location under the user's data directory.
    ///
    /// # Errors
    /// Returns an error when the directory cannot be created.
    pub fn default_location() -> Result<Self, SessionError> {
        Self::new(crate::paths::po_sessions_dir())
    }

    /// Root directory of the store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path a session with the given id would occupy.
    #[must_use]
    pub fn path_for(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    /// Create and open a new session with a generated id.
    ///
    /// # Errors
    /// Returns an error when the file cannot be created.
    pub fn create(&self, title: &str) -> Result<Session, SessionError> {
        let id = format!("{}-{}", now_unix(), unique_suffix());
        let path = self.path_for(&id);
        let mut session = Session::open(path)?;
        session.header.title = title.to_string();
        write_header(session.path(), &session.header)?;
        Ok(session)
    }

    /// Open an existing session by id.
    ///
    /// # Errors
    /// Returns an error when the session does not exist or cannot be read.
    pub fn open(&self, id: &str) -> Result<Session, SessionError> {
        let path = self.path_for(id);
        if !path.exists() {
            return Err(SessionError::NotFound(id.to_string()));
        }
        Session::open(path)
    }

    /// List all sessions, newest first.
    ///
    /// # Errors
    /// Returns an error when a session file cannot be read.
    pub fn list(&self) -> Result<Vec<SessionHeader>, SessionError> {
        let mut headers = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(header) = read_header(&path) {
                headers.push(header);
            }
        }
        headers.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        Ok(headers)
    }
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:08x}-{seq:04x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_back_messages() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path()).expect("store");
        let mut session = store.create("test session").expect("create");
        session
            .append(&SessionEntry::Message {
                message: ChatMessage::user("hello"),
            })
            .expect("append");
        session
            .append(&SessionEntry::Message {
                message: ChatMessage::assistant("hi"),
            })
            .expect("append");

        let reopened = store.open(session.id()).expect("open");
        let messages = reopened.messages().expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].content, "hi");
    }

    #[test]
    fn fork_copies_prefix_and_records_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path()).expect("store");
        let mut session = store.create("original").expect("create");
        for i in 0..4 {
            session
                .append(&SessionEntry::Message {
                    message: ChatMessage::user(format!("m{i}")),
                })
                .expect("append");
        }

        let fork_path = store.path_for("forked");
        let forked = session.fork(&fork_path, 2).expect("fork");
        let messages = forked.messages().expect("messages");
        assert_eq!(messages.len(), 2, "fork should copy only the prefix");
        assert_eq!(messages[0].content, "m0");

        let header = forked.header();
        assert_eq!(header.parent.as_deref(), Some(session.id()));
        assert_eq!(header.forked_at, Some(2));
    }

    #[test]
    fn list_returns_sessions_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path()).expect("store");
        let first = store.create("first").expect("create");
        let second = store.create("second").expect("create");
        let headers = store.list().expect("list");
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].id, second.id());
        assert_ne!(headers[0].id, first.id());
    }
}
