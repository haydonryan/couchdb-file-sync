pub mod cli;
pub mod config;
pub mod couchdb;
pub mod local;
pub mod logging;
pub mod models;
pub mod slack;
pub mod sync;
pub mod telegram;

// Re-export commonly used types
pub use config::AppConfig;
pub use couchdb::CouchDb;
pub use local::LocalDb;
pub use models::{Change, Conflict, FileDoc, FileState, ResolutionStrategy};
pub use sync::{SyncEngine, SyncReport};
