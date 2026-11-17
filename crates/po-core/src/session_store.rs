//! Live session persistence: the in-memory transcript backed by the JSONL log.
//!
//! The UI mutates a `LiveSession` (append user/assistant/system rows); every
//! mutation is mirrored into the append-only [`SessionStore`] file so any
//! session can be replayed or forked later.

use std::path::PathBuf;

use thiserror::Error;

use crate::model::ChatMessage;
use crate::session::{Session, SessionEntry, SessionStore};

/// Errors raised by the live session layer.
#[derive(Debug, Error)]
pub enum LiveSessionError {
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Log parsing failure.
    #[error("log error: {0}")]
    Log(String),
    /// Underlying session-log failure.
    #[error("session error: {0}")]
    Session(#[from] crate::session::SessionError),
}

/// A transcript that is being recorded to disk as it happens.
#[derive(Debug, Clone)]
pub struct LiveSession {
    id: String,
    title: String,
    messages: Vec<ChatMessage>,
    path: PathBuf,
}

impl LiveSession {
    /// Start a new recorded session inside `store`.
    ///
    /// # Errors
    /// Returns an error when the session file cannot be created.
    pub async fn new(store: &SessionStore, title: &str) -> Result<Self, LiveSessionError> {
        let id = format!("{}-live", crate::session::now_unix());
        let path = store.path_for(&id);
        // SessionStore::create is sync; run it off the async reactor with an
        // owned clone of the store.
        let store = store.clone();
        let title_owned = title.to_string();
        let session = tokio::task::spawn_blocking(move || store.create(&title_owned))
            .await
            .map_err(|e| LiveSessionError::Log(e.to_string()))?
            .map_err(|e| LiveSessionError::Log(e.to_string()))?;
        Ok(Self {
            id: session.id().to_string(),
            title: session.header().title.clone(),
            messages: Vec::new(),
            path,
        })
    }

    /// Wrap an already-persisted session.
    ///
    /// # Errors
    /// Returns an error when the log cannot be read.
    pub async fn resume(session: &Session) -> Result<Self, LiveSessionError> {
        let path = session.path().to_path_buf();
        let header = session.header().clone();
        let log = path.clone();
        let entries = tokio::task::spawn_blocking(move || {
            Session::open(log).and_then(|s| s.messages())
        })
            .await
            .map_err(|e| LiveSessionError::Log(e.to_string()))?
            .map_err(|e| LiveSessionError::Log(e.to_string()))?;
        Ok(Self {
            id: header.id.clone(),
            title: header.title.clone(),
            messages: entries,
            path,
        })
    }

    /// The session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The session title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The backing file.
    #[must_use]
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// The in-memory transcript.
    #[must_use]
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// Append a message to the transcript and the log.
    ///
    /// # Errors
    /// Returns an error when the log cannot be written.
    pub async fn record(&mut self, message: ChatMessage) -> Result<(), LiveSessionError> {
        self.messages.push(message.clone());
        let path = self.path.clone();
        let entry = SessionEntry::Message { message };
        tokio::task::spawn_blocking(move || -> Result<(), LiveSessionError> {
            let mut session = Session::open(path)?;
            session.append(&entry)?;
            Ok(())
        })
        .await
        .map_err(|e| LiveSessionError::Log(e.to_string()))??;
        Ok(())
    }

    /// Append a user message.
    ///
    /// # Errors
    /// Returns an error when the log cannot be written.
    pub async fn record_user(&mut self, text: impl Into<String>) -> Result<(), LiveSessionError> {
        self.record(ChatMessage::user(text)).await
    }

    /// Append an assistant message.
    ///
    /// # Errors
    /// Returns an error when the log cannot be written.
    pub async fn record_assistant(
        &mut self,
        text: impl Into<String>,
    ) -> Result<(), LiveSessionError> {
        self.record(ChatMessage::assistant(text)).await
    }

    /// Append a system notice.
    ///
    /// # Errors
    /// Returns an error when the log cannot be written.
    pub async fn record_system(
        &mut self,
        text: impl Into<String>,
    ) -> Result<(), LiveSessionError> {
        self.record(ChatMessage::system(text)).await
    }

    /// Replace the in-memory transcript from the log (after fork/switch).
    ///
    /// # Errors
    /// Returns an error when the log cannot be read.
    pub async fn reload(&mut self) -> Result<(), LiveSessionError> {
        let path = self.path.clone();
        let entries = tokio::task::spawn_blocking(move || {
            Session::open(path).and_then(|s| s.messages())
        })
        .await
        .map_err(|e| LiveSessionError::Log(e.to_string()))?
        .map_err(|e| LiveSessionError::Log(e.to_string()))?;
        self.messages = entries;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::new(dir.path()).expect("store");
        (dir, store)
    }

    #[tokio::test]
    async fn records_and_reloads_a_transcript() {
        let (_dir, store) = store();
        let mut live = LiveSession::new(&store, "demo").await.expect("new");
        live.record_user("hello").await.expect("user");
        live.record_assistant("hi there").await.expect("assistant");
        assert_eq!(live.messages().len(), 2);

        live.reload().await.expect("reload");
        assert_eq!(live.messages().len(), 2);
        assert_eq!(live.messages()[0].content, "hello");
        assert_eq!(live.messages()[1].content, "hi there");
    }

    #[tokio::test]
    async fn system_notices_are_recorded() {
        let (_dir, store) = store();
        let mut live = LiveSession::new(&store, "notices").await.expect("new");
        live.record_system("theme: dark").await.expect("system");
        live.reload().await.expect("reload");
        assert_eq!(live.messages()[0].role.role_label(), "system");
    }

    #[tokio::test]
    async fn resumed_session_reads_the_log() {
        let (_dir, store) = store();
        let mut live = LiveSession::new(&store, "resume-me").await.expect("new");
        live.record_user("one").await.expect("user");
        drop(live);

        let persisted = store.open(&store.list().expect("list")[0].id).expect("open");
        let resumed = LiveSession::resume(&persisted).await.expect("resume");
        assert_eq!(resumed.messages().len(), 1);
        assert_eq!(resumed.messages()[0].content, "one");
    }
}
