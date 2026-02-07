use chrono::{DateTime, Utc};
use couch_rs::document::TypedCouchDocument;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// File metadata stored in CouchDB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDoc {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev", skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    pub content_type: String,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub hash: String,
    #[serde(default)]
    pub deleted: bool,
    pub synced_at: DateTime<Utc>,
}

impl FileDoc {
    pub fn new(path: String, hash: String, size: u64) -> Self {
        let now = Utc::now();
        let content_type = mime_guess::from_path(&path)
            .first_or_octet_stream()
            .to_string();
        Self {
            id: path,
            rev: None,
            content_type,
            size,
            modified_at: now,
            hash,
            deleted: false,
            synced_at: now,
        }
    }
}

impl TypedCouchDocument for FileDoc {
    fn get_id(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.id)
    }

    fn get_rev(&self) -> Cow<'_, str> {
        Cow::Borrowed(self.rev.as_deref().unwrap_or(""))
    }

    fn set_rev(&mut self, rev: &str) {
        self.rev = Some(rev.to_string());
    }

    fn set_id(&mut self, id: &str) {
        self.id = id.to_string();
    }

    fn merge_ids(&mut self, other: &Self) {
        self.id = other.id.clone();
        self.rev = other.rev.clone();
    }
}

/// Local file state for tracking
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub couch_rev: Option<String>,
    pub last_sync_at: DateTime<Utc>,
}

impl FileState {
    pub fn new(path: String, hash: String, size: u64, modified_at: DateTime<Utc>) -> Self {
        Self {
            path,
            hash,
            size,
            couch_rev: None,
            last_sync_at: Utc::now(),
            modified_at,
        }
    }
}

/// Remote file state from CouchDB
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteState {
    pub path: String,
    pub hash: String,
    pub size: u64,
    pub modified_at: DateTime<Utc>,
    pub couch_rev: String,
    #[serde(default)]
    pub deleted: bool,
}

impl From<FileDoc> for RemoteState {
    fn from(doc: FileDoc) -> Self {
        Self {
            path: doc.id,
            hash: doc.hash,
            size: doc.size,
            modified_at: doc.modified_at,
            couch_rev: doc.rev.unwrap_or_default(),
            deleted: doc.deleted,
        }
    }
}
