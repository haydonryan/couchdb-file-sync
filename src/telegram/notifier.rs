use crate::models::Conflict;
use anyhow::Result;
use reqwest::Client;
use tracing::{debug, warn};

/// Telegram bot notifier
pub struct TelegramNotifier {
    bot_token: String,
    chat_id: String,
    client: Client,
}

impl TelegramNotifier {
    /// Create a new Telegram notifier
    pub fn new(bot_token: String, chat_id: String) -> Self {
        Self {
            bot_token,
            chat_id,
            client: Client::new(),
        }
    }

    /// Send conflict notification
    pub async fn notify_conflict(&self, conflict: &Conflict, sync_dir: &str) -> Result<()> {
        let message = format!(
            "⚠️ <b>CouchFS Conflict Detected</b>\n\n\
             📁 File: <code>{}</code>\n\
             📂 Location: <code>{}</code>\n\n\
             📝 <b>Local version:</b>\n\
              • Modified: {}\n\
              • Size: {} KB\n\n\
             🌐 <b>Remote version:</b>\n\
              • Modified: {}\n\
              • Size: {} KB\n\n\
             ✅ Remote file saved as: <code>{}.remote</code>\n\n\
             Resolve with:\n\
             <code>couchfs resolve {} --strategy keep-local</code>\n\
             <code>couchfs resolve {} --strategy keep-remote</code>\n\
             <code>couchfs resolve {} --strategy keep-both</code>",
            escape_html(&conflict.path),
            escape_html(sync_dir),
            conflict.local_state.modified_at.format("%Y-%m-%d %H:%M"),
            conflict.local_state.size / 1024,
            conflict.remote_state.modified_at.format("%Y-%m-%d %H:%M"),
            conflict.remote_state.size / 1024,
            escape_html(&conflict.path),
            escape_html(&conflict.path),
            escape_html(&conflict.path),
            escape_html(&conflict.path)
        );

        self.send_message(&message).await?;
        debug!("Sent Telegram notification for conflict: {}", conflict.path);
        Ok(())
    }

    /// Send sync error notification
    pub async fn notify_error(&self, error: &str) -> Result<()> {
        let message = format!(
            "❌ <b>CouchFS Sync Error</b>\n\n{}\n\nTimestamp: {}",
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

        let response = self.client.post(&url).json(&params).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            anyhow::bail!("Telegram API error: {}", error_text);
        }

        Ok(())
    }

    /// Test the connection
    pub async fn test(&self) -> Result<bool> {
        let url = format!("https://api.telegram.org/bot{}/getMe", self.bot_token);

        match self.client.get(&url).send().await {
            Ok(response) => Ok(response.status().is_success()),
            Err(e) => {
                warn!("Telegram connection test failed: {}", e);
                Ok(false)
            }
        }
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
