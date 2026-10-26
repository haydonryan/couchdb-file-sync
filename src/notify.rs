//! Shared plumbing for out-of-process conflict notifications.
//!
//! Delivering a conflict over Telegram or Matrix shares two concerns that this
//! module centralises:
//!
//! * an injectable HTTP transport ([`HttpSender`]) so the real notifiers can
//!   be exercised against fake senders in tests without network access, with a
//!   bounded per-request timeout so a hung remote endpoint cannot stall
//!   synchronization indefinitely;
//! * a per-conflict delivery identity ([`conflict_generation`]) so delivery
//!   state is tracked per channel, root and conflict generation instead of the
//!   old single `notified` flag / shared session set.

use crate::models::Conflict;
use anyhow::Result;
use std::pin::Pin;
use std::time::Duration;

/// Default per-request timeout for notification HTTP calls. A hung remote
/// endpoint must not block synchronization forever, so every notifier request
/// is bounded by this window.
pub const NOTIFICATION_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// A minimal HTTP response returned by an [`HttpSender`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Transport abstraction used by the chat notifiers.
///
/// Real deployments use [`ReqwestSender`]. Tests inject fakes that record
/// calls and script success, failure or slowness so delivery behaviour can be
/// verified without network access. The boxed-future shape keeps the trait
/// object-safe so a notifier can hold an `Arc<dyn HttpSender>`.
#[allow(clippy::manual_async_fn)] // dyn-compatible trait requires boxing.
pub trait HttpSender: Send + Sync {
    /// POST a JSON body to `url`, applying `headers`.
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(&'a str, &'a str)],
        json: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>>;

    /// GET `url`, applying `headers`.
    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(&'a str, &'a str)],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>>;
}

/// [`HttpSender`] backed by a real `reqwest` client whose requests are bounded
/// by a timeout so a hung notifier cannot block sync.
pub struct ReqwestSender {
    client: reqwest::Client,
}

impl ReqwestSender {
    /// Build a sender using the default [`NOTIFICATION_HTTP_TIMEOUT`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_timeout(NOTIFICATION_HTTP_TIMEOUT)
    }

    /// Build a sender with an explicit request timeout (tests use a short one
    /// so a hung endpoint is exercised quickly).
    ///
    /// # Panics
    ///
    /// Panics if `reqwest` cannot build an HTTP client (only possible with an
    /// invalid TLS backend).
    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("reqwest client build must not fail");
        Self { client }
    }
}

impl Default for ReqwestSender {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpSender for ReqwestSender {
    fn post_json<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(&'a str, &'a str)],
        json: &'a serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>> {
        Box::pin(async move {
            let mut request = self.client.post(url);
            for (key, value) in headers {
                request = request.header(*key, *value);
            }
            let response = request.json(json).send().await?;
            let status = response.status().as_u16();
            let body = response.text().await?;
            Ok(HttpResponse { status, body })
        })
    }

    fn get<'a>(
        &'a self,
        url: &'a str,
        headers: &'a [(&'a str, &'a str)],
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>> {
        Box::pin(async move {
            let mut request = self.client.get(url);
            for (key, value) in headers {
                request = request.header(*key, *value);
            }
            let response = request.send().await?;
            let status = response.status().as_u16();
            let body = response.text().await?;
            Ok(HttpResponse { status, body })
        })
    }
}

/// A notifier capable of sending a batch conflict notification.
///
/// Implemented by [`crate::telegram::TelegramNotifier`] and
/// [`crate::matrix::MatrixNotifier`] so the delivery-tracking orchestration in
/// `cli::commands` is channel-agnostic and testable with fakes.
#[allow(clippy::manual_async_fn)] // dyn-compatible trait requires boxing.
pub trait ConflictNotifier: Send + Sync {
    /// Notify about a batch of new conflicts, returning an error (or timing
    /// out) when delivery fails so the caller can keep it retryable.
    fn notify_new_conflicts<'a>(
        &'a self,
        conflicts: &'a [&'a Conflict],
        sync_dir: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// Stable per-conflict delivery identity: the relative path plus the conflict
/// generation derived from its remote `CouchDB` revision.
///
/// A brand-new conflict at the same path (a recurrent conflict with a new
/// generation) is delivered again, while re-detecting the same conflict is a
/// no-op. Falls back to the full revision string when the generation cannot be
/// parsed.
#[must_use]
pub fn conflict_generation(conflict: &Conflict) -> String {
    let rev = conflict.remote_state.couch_rev.as_str();
    crate::sync::triage::rev_generation(rev).map_or_else(|| rev.to_string(), |g| g.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::models::file::{CouchRev, FileState, RemoteState};

    /// Build a [`Conflict`] for delivery tests.
    #[must_use]
    pub fn make_conflict(path: &str, rev: &str) -> Conflict {
        let local_state = FileState::new(path.into(), "lhash".into(), 10, chrono::Utc::now());
        let remote_state = RemoteState {
            hash: "rhash".into(),
            size: 10,
            modified_at: chrono::Utc::now(),
            couch_rev: CouchRev::new(rev).unwrap_or_default(),
            deleted: false,
        };
        Conflict::new(path.into(), local_state, remote_state)
    }

    #[test]
    fn conflict_generation_uses_remote_rev_generation() {
        let c = make_conflict("/a.txt", "3-abc");
        assert_eq!(conflict_generation(&c), "3");
    }

    #[test]
    fn conflict_generation_falls_back_to_full_rev() {
        // A rev whose generation cannot be parsed falls back to the full rev
        // string so the identity is still stable for the same conflict.
        let c = make_conflict("/a.txt", "not-a-rev");
        assert_eq!(conflict_generation(&c), "not-a-rev");
    }

    /// A fake HTTP sender that records every call and scripts a queue of
    /// responses, so delivery orchestration is exercised without a network.
    #[derive(Debug)]
    pub struct FakeSender {
        calls: parking_lot::Mutex<Vec<String>>,
        results: parking_lot::Mutex<std::collections::VecDeque<anyhow::Result<HttpResponse>>>,
    }

    impl FakeSender {
        #[must_use]
        pub fn new(results: Vec<anyhow::Result<HttpResponse>>) -> Self {
            Self {
                calls: parking_lot::Mutex::new(Vec::new()),
                results: parking_lot::Mutex::new(results.into()),
            }
        }

        /// Number of recorded calls whose URL contains `needle`.
        #[must_use]
        pub fn call_count(&self, needle: &str) -> usize {
            self.calls
                .lock()
                .iter()
                .filter(|url| url.contains(needle))
                .count()
        }

        fn record_and_next(&self, url: &str) -> anyhow::Result<HttpResponse> {
            self.calls.lock().push(url.to_string());
            self.results.lock().pop_front().unwrap_or_else(|| {
                Ok(HttpResponse {
                    status: 200,
                    body: String::new(),
                })
            })
        }
    }

    impl HttpSender for FakeSender {
        fn post_json<'a>(
            &'a self,
            url: &'a str,
            _headers: &'a [(&'a str, &'a str)],
            _json: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>> {
            Box::pin(async move { self.record_and_next(url) })
        }

        fn get<'a>(
            &'a self,
            url: &'a str,
            _headers: &'a [(&'a str, &'a str)],
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>> {
            Box::pin(async move { self.record_and_next(url) })
        }
    }

    /// A fake HTTP sender that never responds, used to exercise the delivery
    /// timeout path (a hung notifier must not block sync).
    #[derive(Debug)]
    pub struct HangingSender;

    impl HttpSender for HangingSender {
        fn post_json<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(&'a str, &'a str)],
            _json: &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>> {
            Box::pin(std::future::pending())
        }

        fn get<'a>(
            &'a self,
            _url: &'a str,
            _headers: &'a [(&'a str, &'a str)],
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse>> + Send + 'a>> {
            Box::pin(std::future::pending())
        }
    }
}
