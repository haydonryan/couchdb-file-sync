use crate::models::Conflict;
use anyhow::Result;
use reqwest::Client;
use tracing::{debug, warn};

/// Matrix bot notifier
pub struct MatrixNotifier {
    homeserver_url: String,
    access_token: String,
    room_id: String,
    message_type: String,
    client: Client,
}

impl MatrixNotifier {
    /// Create a new Matrix notifier
    pub fn new(
        homeserver_url: String,
        access_token: String,
        room_id: String,
        message_type: String,
    ) -> Self {
        Self {
            homeserver_url,
            access_token,
            room_id,
            message_type,
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
        debug!("Sent Matrix notification for {} conflicts", conflicts.len());
        Ok(())
    }

    /// Send sync error notification
    pub async fn notify_error(&self, error: &str) -> Result<()> {
        let message = format!(
            "❌ <b>CouchDB File Sync Error</b>\n\n{}\n\nTimestamp: {}",
            escape_html(error),
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );

        self.send_message(&message).await?;
        Ok(())
    }

    /// Send a message to a Matrix room
    async fn send_message(&self, text: &str) -> Result<()> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message",
            self.homeserver_url.trim_end_matches('/'),
            self.room_id
        );

        let body = serde_json::json!({
            "msgtype": self.message_type,
            "body": strip_html(text),
            "format": "org.matrix.custom.html",
            "formatted_body": text,
        });

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            anyhow::bail!("Matrix API error: {}", error_text);
        }

        Ok(())
    }

    /// Test the connection by checking if the room is accessible
    pub async fn test(&self) -> Result<bool> {
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/state/m.room.name",
            self.homeserver_url.trim_end_matches('/'),
            self.room_id
        );

        match self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.access_token))
            .send()
            .await
        {
            Ok(response) => Ok(response.status().is_success()),
            Err(e) => {
                warn!("Matrix connection test failed: {}", e);
                Ok(false)
            }
        }
    }
}

/// Strip HTML tags for plain text body
fn strip_html(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ => {
                if !in_tag {
                    result.push(ch);
                }
            }
        }
    }
    // Decode common entities
    result
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
}

/// Escape HTML characters for Matrix formatted body
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

    #[test]
    fn test_strip_html() {
        assert_eq!(strip_html("<b>bold</b>"), "bold");
        assert_eq!(strip_html("a &amp; b"), "a & b");
        assert_eq!(strip_html("Hello<br>World"), "HelloWorld");
    }
}
