#![allow(dead_code)] // Discord API fields kept for future slices (images, mention resolution, pagination).

use serde::Deserialize;
use std::fmt;
use std::time::Duration;

const API_BASE: &str = "https://discord.com/api/v10";
const USER_AGENT: &str = "DiscordBot (https://github.com/extragornax/discord_to_insta, 0.1.0)";

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub content: String,
    pub timestamp: String,
    pub author: Author,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub mentions: Vec<User>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Author {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub global_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub content_type: Option<String>,
}

impl Author {
    pub fn display(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }
}

impl User {
    pub fn display(&self) -> &str {
        self.global_name.as_deref().unwrap_or(&self.username)
    }
}

#[derive(Debug)]
pub enum Error {
    Unauthorized,
    Forbidden,
    NotFound,
    RateLimited { retry_after_ms: u64 },
    Http { status: u16, body: String },
    Transport(String),
    Parse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unauthorized => write!(f, "Discord rejected the bot token (401). Check DISCORD_BOT_TOKEN."),
            Error::Forbidden => write!(f, "Forbidden (403). Bot needs View Channel + Read Message History (fetch) and Add Reactions (auto-react)."),
            Error::NotFound => write!(f, "Channel not found (404). Wrong channel ID?"),
            Error::RateLimited { retry_after_ms } => write!(f, "Rate limited. Retry in {}ms.", retry_after_ms),
            Error::Http { status, body } => write!(f, "Discord HTTP {status}: {body}"),
            Error::Transport(e) => write!(f, "Network error: {e}"),
            Error::Parse(e) => write!(f, "Failed to parse Discord response: {e}"),
        }
    }
}

impl std::error::Error for Error {}

pub struct Client {
    token: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(token: impl Into<String>) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(USER_AGENT)
            .build();
        Self { token: token.into(), agent }
    }

    /// Fetch the most recent `limit` messages from a channel (newest first).
    /// Discord caps `limit` at 100.
    pub fn fetch_messages(&self, channel_id: &str, limit: u32) -> Result<Vec<Message>, Error> {
        let limit = limit.clamp(1, 100);
        let url = format!("{API_BASE}/channels/{channel_id}/messages?limit={limit}");

        let response = self
            .agent
            .get(&url)
            .set("Authorization", &format!("Bot {}", self.token))
            .call();

        match response {
            Ok(resp) => {
                let body = resp
                    .into_string()
                    .map_err(|e| Error::Transport(e.to_string()))?;
                serde_json::from_str::<Vec<Message>>(&body)
                    .map_err(|e| Error::Parse(format!("{e} — body was: {body}")))
            }
            Err(ureq::Error::Status(status, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(map_status(status, body))
            }
            Err(ureq::Error::Transport(t)) => Err(Error::Transport(t.to_string())),
        }
    }

    /// React to a message as the bot. `emoji` must be a Unicode emoji string
    /// (e.g. "✅"). Custom emojis would need `name:id` form, not supported here.
    pub fn add_reaction(
        &self,
        channel_id: &str,
        message_id: &str,
        emoji: &str,
    ) -> Result<(), Error> {
        let encoded = percent_encode(emoji);
        let url = format!(
            "{API_BASE}/channels/{channel_id}/messages/{message_id}/reactions/{encoded}/@me"
        );

        let response = self
            .agent
            .put(&url)
            .set("Authorization", &format!("Bot {}", self.token))
            .set("Content-Length", "0")
            .call();

        match response {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(status, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(map_status(status, body))
            }
            Err(ureq::Error::Transport(t)) => Err(Error::Transport(t.to_string())),
        }
    }
}

fn map_status(status: u16, body: String) -> Error {
    match status {
        401 => Error::Unauthorized,
        403 => Error::Forbidden,
        404 => Error::NotFound,
        429 => {
            let ms = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("retry_after").and_then(|r| r.as_f64()))
                .map(|s| (s * 1000.0) as u64)
                .unwrap_or(0);
            Error::RateLimited { retry_after_ms: ms }
        }
        _ => Error::Http { status, body },
    }
}

/// Minimal RFC 3986 percent-encoder. Unreserved chars pass through; everything
/// else — including all UTF-8 continuation bytes of a non-ASCII emoji — is
/// encoded as `%HH`.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", byte));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canned sample in Discord's v10 JSON shape for the fields we consume.
    const SAMPLE: &str = r#"[
      {
        "id": "111",
        "content": "@everyone\n⏰ RDV ...",
        "timestamp": "2026-04-20T19:45:00.000000+00:00",
        "author": {"id": "42", "username": "bot", "global_name": "BotDisplay"},
        "attachments": [
          {"id": "a1", "url": "https://cdn.discordapp.com/attachments/x/y.png", "filename": "y.png", "content_type": "image/png"}
        ],
        "mentions": [
          {"id": "699543821465419806", "username": "bertrand", "global_name": "Bertrand B"}
        ]
      }
    ]"#;

    #[test]
    fn parses_canned_message_list() {
        let msgs: Vec<Message> = serde_json::from_str(SAMPLE).expect("parse");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "111");
        assert_eq!(msgs[0].author.display(), "BotDisplay");
        assert_eq!(msgs[0].attachments.len(), 1);
        assert_eq!(msgs[0].mentions[0].id, "699543821465419806");
        assert_eq!(msgs[0].mentions[0].display(), "Bertrand B");
    }

    #[test]
    fn parses_message_without_optional_fields() {
        let raw = r#"[{"id":"1","content":"hi","timestamp":"2026-01-01T00:00:00+00:00","author":{"id":"2","username":"u","global_name":null}}]"#;
        let msgs: Vec<Message> = serde_json::from_str(raw).expect("parse");
        assert_eq!(msgs[0].author.display(), "u");
        assert!(msgs[0].attachments.is_empty());
        assert!(msgs[0].mentions.is_empty());
    }

    #[test]
    fn percent_encode_emojis() {
        // ✅ U+2705 — UTF-8 E2 9C 85
        assert_eq!(percent_encode("✅"), "%E2%9C%85");
        // 🚫 U+1F6AB — UTF-8 F0 9F 9A AB
        assert_eq!(percent_encode("🚫"), "%F0%9F%9A%AB");
        // 🤔 U+1F914 — UTF-8 F0 9F A4 94
        assert_eq!(percent_encode("🤔"), "%F0%9F%A4%94");
    }

    #[test]
    fn percent_encode_preserves_unreserved() {
        assert_eq!(percent_encode("abcXYZ-_.~0189"), "abcXYZ-_.~0189");
    }
}
