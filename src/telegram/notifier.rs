use crate::models::Conflict;
use crate::notify::{ConflictNotifier, HttpSender, ReqwestSender};
use anyhow::Result;
use std::pin::Pin;
use std::sync::Arc;
use tracing::{debug, warn};

/// Telegram bot notifier
pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    sender: Arc<dyn HttpSender>,
}

impl TelegramNotifier {
    /// Create a new Telegram notifier backed by a timed real HTTP transport.
    #[must_use]
    pub fn new(bot_token: String, chat_id: String) -> Self {
        Self::with_sender(bot_token, chat_id, Arc::new(ReqwestSender::new()))
    }

    /// Create a notifier backed by an explicit HTTP transport (tests inject a
    /// fake sender).
    #[must_use]
    pub fn with_sender(bot_token: String, chat_id: String, sender: Arc<dyn HttpSender>) -> Self {
        Self {
            bot_token,
            chat_id,
            sender,
        }
    }

    /// Send notification for multiple new conflicts (single message per sync run)
    ///
    /// # Errors
    ///
    /// Returns an error if the Telegram API request fails.
    pub async fn notify_new_conflicts(
        &self,
        conflicts: &[&Conflict],
        sync_dir: &str,
    ) -> Result<()> {
        if conflicts.is_empty() {
            return Ok(());
        }

        let file_list: String = conflicts
            .iter()
            .map(|c| format!("  • <code>{}</code>", escape_html(&c.path)))
            .collect::<Vec<_>>()
            .join("\n");

        let message = format!(
            "⚠️ <b>CouchDB File Sync: {} New Conflict{}</b>\n\n\
             📂 Location: <code>{}</code>\n\n\
             📁 <b>Files in conflict:</b>\n\
             {}\n\n\
             Run <code>couchdb-file-sync resolve</code> to resolve interactively.",
            conflicts.len(),
            if conflicts.len() == 1 { "" } else { "s" },
            escape_html(sync_dir),
            file_list
        );

        self.send_message(&message).await?;
        debug!(
            "Sent Telegram notification for {} conflicts",
            conflicts.len()
        );
        Ok(())
    }

    /// Send sync error notification
    ///
    /// # Errors
    ///
    /// Returns an error if the Telegram API request fails.
    pub async fn notify_error(&self, error: &str) -> Result<()> {
        let message = format!(
            "❌ <b>CouchDB File Sync Error</b>\n\n{}\n\nTimestamp: {}",
            escape_html(error),
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );

        self.send_message(&message).await?;
        Ok(())
    }

    /// Send generic message
    async fn send_message(&self, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);

        let params = serde_json::json!({
            "chat_id": self.chat_id,
            "text": text,
            "parse_mode": "HTML",
            "disable_notification": false,
        });

        let response = self.sender.post_json(&url, &[], &params).await?;

        if !(200..300).contains(&response.status) {
            anyhow::bail!("Telegram API error: {}", response.body);
        }

        Ok(())
    }

    /// Test the connection
    ///
    /// # Errors
    ///
    /// Returns an error if the Telegram API request fails.
    pub async fn test(&self) -> Result<bool> {
        let url = format!("https://api.telegram.org/bot{}/getMe", self.bot_token);

        match self.sender.get(&url, &[]).await {
            Ok(response) => Ok((200..300).contains(&response.status)),
            Err(e) => {
                warn!("Telegram connection test failed: {}", e);
                Ok(false)
            }
        }
    }
}

impl ConflictNotifier for TelegramNotifier {
    fn notify_new_conflicts<'a>(
        &'a self,
        conflicts: &'a [&'a Conflict],
        sync_dir: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move { Self::notify_new_conflicts(self, conflicts, sync_dir).await })
    }
}

/// Escape HTML characters for Telegram
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_html() {
        assert_eq!(escape_html("<test>"), "&lt;test&gt;");
        assert_eq!(escape_html("a & b"), "a &amp; b");
        assert_eq!(escape_html("\"quoted\""), "&quot;quoted&quot;");
    }
}
