use anyhow::Result;
use reqwest::Client;
use serde::Serialize;

/// Slack incoming webhook notifier
pub struct SlackNotifier {
    webhook_url: String,
    client: Client,
}

impl SlackNotifier {
    /// Create a new Slack notifier
    pub fn new(webhook_url: String) -> Self {
        Self {
            webhook_url,
            client: Client::new(),
        }
    }

    /// Send a simple text message via the async client.
    pub async fn notify_text(&self, text: &str) -> Result<()> {
        send_via_client(&self.client, &self.webhook_url, text).await
    }

    /// Send a simple text message via a blocking client.
    pub fn notify_text_blocking(webhook_url: &str, text: &str) -> Result<()> {
        let client = reqwest::blocking::Client::new();
        let response = client
            .post(webhook_url)
            .json(&SlackMessage::new(text))
            .send()?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            anyhow::bail!("Slack webhook error ({status}): {body}");
        }

        Ok(())
    }
}

async fn send_via_client(client: &Client, webhook_url: &str, text: &str) -> Result<()> {
    let response = client
        .post(webhook_url)
        .json(&SlackMessage::new(text))
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Slack webhook error ({status}): {body}");
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct SlackMessage<'a> {
    text: &'a str,
}

impl<'a> SlackMessage<'a> {
    fn new(text: &'a str) -> Self {
        Self { text }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_message_serializes_text_payload() {
        let json = serde_json::to_value(SlackMessage::new("panic")).unwrap();
        assert_eq!(json, serde_json::json!({ "text": "panic" }));
    }
}
