//! Minimal Telegram Bot API client used for the "confirm before posting to
//! Instagram" approval gate. Sends a preview (image + caption + inline
//! keyboard) to a configured group, long-polls `getUpdates` for callback
//! queries, and routes them back to the awaiting approval task via a
//! `tokio::sync::oneshot`.
//!
//! Intentionally transport-only: approval state and flow live in `main.rs`.

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fmt;
use std::path::Path;
use std::time::Duration;

const API_BASE: &str = "https://api.telegram.org/bot";
// Telegram honors timeouts up to 50s; 25s keeps us responsive to shutdown
// while keeping the round-trip count low.
const LONG_POLL_TIMEOUT_SECS: u64 = 25;

#[derive(Debug)]
pub enum Error {
    Http { status: u16, body: String },
    Transport(String),
    Parse(String),
    Api { description: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http { status, body } => write!(f, "Telegram HTTP {status}: {body}"),
            Error::Transport(e) => write!(f, "Telegram network error: {e}"),
            Error::Parse(e) => write!(f, "Telegram response parse error: {e}"),
            Error::Api { description } => write!(f, "Telegram API error: {description}"),
        }
    }
}

impl std::error::Error for Error {}

// ---- API response envelopes ------------------------------------------------

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    description: Option<String>,
    result: Option<T>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub from: Option<CallbackFrom>,
    #[serde(default)]
    pub message: Option<CallbackMessage>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CallbackFrom {
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
}

impl CallbackFrom {
    pub fn display(&self) -> String {
        self.username
            .clone()
            .or_else(|| self.first_name.clone())
            .unwrap_or_else(|| "?".to_string())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CallbackMessage {
    pub message_id: i64,
    pub chat: CallbackChat,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CallbackChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
struct SentMessage {
    message_id: i64,
}

// ---- Client ----------------------------------------------------------------

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    token: String,
}

impl Client {
    pub fn new(token: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(LONG_POLL_TIMEOUT_SECS + 15))
            .build()
            .expect("reqwest client");
        Self {
            http,
            token: token.into(),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("{API_BASE}{}/{method}", self.token)
    }

    /// Post the image and a text message with inline keyboard. The keyboard
    /// carries the discord_message_id so callbacks can be routed back.
    /// Returns the `message_id` of the text message (the one with buttons),
    /// so callers can edit it to show the outcome later.
    pub async fn send_preview(
        &self,
        chat_id: i64,
        caption: &str,
        image_bytes: Vec<u8>,
        image_filename: &str,
        discord_msg_id: &str,
    ) -> Result<i64, Error> {
        // Photo first, no caption — caption goes on the text message so the
        // whole Instagram caption is visible regardless of the 1024-char
        // sendPhoto caption limit.
        self.send_photo(chat_id, image_bytes, image_filename).await?;

        // Text message with the full caption + approval buttons.
        let reply_markup = json!({
            "inline_keyboard": [[
                { "text": "✅ Publier sur Instagram", "callback_data": format!("approve:{discord_msg_id}") },
                { "text": "❌ Annuler", "callback_data": format!("reject:{discord_msg_id}") }
            ]]
        });
        let body = json!({
            "chat_id": chat_id,
            "text": format!("📬 Aperçu à valider avant publication Instagram :\n\n{caption}"),
            "reply_markup": reply_markup,
            // Plain text — don't let any * / _ / ` in the caption break.
            "disable_web_page_preview": true,
        });

        let resp = self
            .http
            .post(self.url("sendMessage"))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;

        let status = resp.status().as_u16();
        let raw = resp
            .text()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(Error::Http { status, body: raw });
        }
        let parsed: ApiResponse<SentMessage> = serde_json::from_str(&raw)
            .map_err(|e| Error::Parse(format!("{e} — body was: {raw}")))?;
        if !parsed.ok {
            return Err(Error::Api {
                description: parsed.description.unwrap_or_default(),
            });
        }
        Ok(parsed.result.map(|m| m.message_id).unwrap_or(0))
    }

    async fn send_photo(
        &self,
        chat_id: i64,
        image_bytes: Vec<u8>,
        image_filename: &str,
    ) -> Result<(), Error> {
        let form = reqwest::multipart::Form::new()
            .text("chat_id", chat_id.to_string())
            .part(
                "photo",
                reqwest::multipart::Part::bytes(image_bytes)
                    .file_name(image_filename.to_string())
                    .mime_str("image/png")
                    .unwrap_or_else(|_| reqwest::multipart::Part::bytes(vec![])),
            );

        let resp = self
            .http
            .post(self.url("sendPhoto"))
            .multipart(form)
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Http { status, body });
        }
        Ok(())
    }

    /// Required call after any callback_query: answers the "loading" state
    /// of the button on the client. Optional `text` appears as a toast.
    pub async fn answer_callback(
        &self,
        callback_id: &str,
        text: Option<&str>,
    ) -> Result<(), Error> {
        let mut body = serde_json::Map::new();
        body.insert("callback_query_id".into(), json!(callback_id));
        if let Some(t) = text {
            body.insert("text".into(), json!(t));
        }
        let resp = self
            .http
            .post(self.url("answerCallbackQuery"))
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if !(200..300).contains(&resp.status().as_u16()) {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Http { status, body });
        }
        Ok(())
    }

    /// Overwrite the text of an existing message (so approved/rejected
    /// decisions visibly update in the group). Also clears the keyboard.
    pub async fn edit_message(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
    ) -> Result<(), Error> {
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "reply_markup": { "inline_keyboard": [] }
        });
        let resp = self
            .http
            .post(self.url("editMessageText"))
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if !(200..300).contains(&resp.status().as_u16()) {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(Error::Http { status, body });
        }
        Ok(())
    }

    /// Long-poll for updates. Returns the batch and the next `offset` to use.
    pub async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<Update>, Error> {
        let mut body = serde_json::Map::new();
        body.insert("timeout".into(), json!(LONG_POLL_TIMEOUT_SECS));
        body.insert("allowed_updates".into(), json!(["callback_query"]));
        if let Some(off) = offset {
            body.insert("offset".into(), json!(off));
        }
        let resp = self
            .http
            .post(self.url("getUpdates"))
            .json(&serde_json::Value::Object(body))
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let raw = resp
            .text()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(Error::Http { status, body: raw });
        }
        let parsed: ApiResponse<Vec<Update>> = serde_json::from_str(&raw)
            .map_err(|e| Error::Parse(format!("{e} — body was: {raw}")))?;
        if !parsed.ok {
            return Err(Error::Api {
                description: parsed.description.unwrap_or_default(),
            });
        }
        Ok(parsed.result.unwrap_or_default())
    }
}

/// Helper: read a PNG/JPEG off disk for a preview.
pub fn read_image(path: &Path) -> std::io::Result<(Vec<u8>, String)> {
    let bytes = std::fs::read(path)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("image.png")
        .to_string();
    Ok((bytes, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_callback_query_update() {
        let raw = r#"{
            "update_id": 10000,
            "callback_query": {
                "id": "cb_abc",
                "data": "approve:1234567890",
                "from": {"username": "gaspard", "first_name": "Gaspard"},
                "message": {
                    "message_id": 42,
                    "chat": {"id": -1001234567890}
                }
            }
        }"#;
        let u: Update = serde_json::from_str(raw).expect("parse");
        let cb = u.callback_query.expect("callback");
        assert_eq!(cb.id, "cb_abc");
        assert_eq!(cb.data.as_deref(), Some("approve:1234567890"));
        assert_eq!(cb.from.as_ref().map(|f| f.display()), Some("gaspard".to_string()));
        assert_eq!(cb.message.as_ref().map(|m| m.message_id), Some(42));
        assert_eq!(cb.message.as_ref().map(|m| m.chat.id), Some(-1001234567890));
    }

    #[test]
    fn parses_non_callback_update_silently() {
        // Telegram can send updates with other kinds (message, edited_message,
        // etc.). Since we only subscribe to callback_query via allowed_updates,
        // we still want to tolerate stray fields.
        let raw = r#"{"update_id": 1, "message": {"ignored": true}}"#;
        let u: Update = serde_json::from_str(raw).expect("parse");
        assert!(u.callback_query.is_none());
    }

    #[test]
    fn callback_from_display_prefers_username() {
        let f = CallbackFrom {
            username: Some("gas".into()),
            first_name: Some("Gaspard".into()),
        };
        assert_eq!(f.display(), "gas");
        let f = CallbackFrom {
            username: None,
            first_name: Some("Gaspard".into()),
        };
        assert_eq!(f.display(), "Gaspard");
    }
}
