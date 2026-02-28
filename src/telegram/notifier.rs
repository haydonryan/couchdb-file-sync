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

    /// Send notification for multiple new conflicts (single message per sync run)
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
            "⚠️ <b>CouchFS: {} New Conflict{}</b>\n\n\
             📂 Location: <code>{}</code>\n\n\
             📁 <b>Files in conflict:</b>\n\
             {}\n\n\
             Run <code>couchfs resolve</code> to resolve interactively.",
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
