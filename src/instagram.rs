//! Minimal Instagram Graph API client for publishing + editing a single
//! photo post. Two-step publish (`/media` container → poll → `/media_publish`)
//! plus an in-place caption edit. Meta fetches the image itself via
//! `image_url`, so the URL must be publicly reachable from their servers.
//!
//! Docs: https://developers.facebook.com/docs/instagram-platform/content-publishing

use serde::Deserialize;
use std::fmt;
use std::time::Duration;

const API_BASE: &str = "https://graph.facebook.com/v21.0";
// Media containers take a few seconds to ingest. We poll for `status_code`:
// the observed happy-path case is FINISHED within 1–3 seconds; back off if
// Meta is slow. Cap total wait at ~60 s before giving up.
const CONTAINER_POLL_ATTEMPTS: u32 = 20;
const CONTAINER_POLL_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum Error {
    /// Network-layer failure (DNS, TLS, timeout).
    Transport(String),
    /// Non-2xx response from Graph. `body` carries Meta's JSON error blob.
    Http { status: u16, body: String },
    /// JSON parsed but `error.message` was present.
    Api { message: String },
    /// Container never reached FINISHED state within the poll budget.
    ContainerStuck { status_code: String, status: String },
    /// Response JSON didn't match our expected shape.
    Parse(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Transport(e) => write!(f, "Instagram network error: {e}"),
            Error::Http { status, body } => write!(f, "Instagram HTTP {status}: {body}"),
            Error::Api { message } => write!(f, "Instagram API error: {message}"),
            Error::ContainerStuck { status_code, status } => write!(
                f,
                "Instagram media container never reached FINISHED (last: {status_code} / {status}) — the image URL may be unreachable from Meta's servers, or the caption may be rejected."
            ),
            Error::Parse(e) => write!(f, "Instagram response parse error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

// ---- Graph API response shapes --------------------------------------------

#[derive(Debug, Deserialize)]
struct CreatedId {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ContainerStatus {
    /// One of EXPIRED | ERROR | FINISHED | IN_PROGRESS | PUBLISHED.
    status_code: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphErrorEnvelope {
    error: Option<GraphError>,
}

#[derive(Debug, Deserialize)]
struct GraphError {
    message: String,
}

// ---- Client ----------------------------------------------------------------

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    token: String,
    ig_user_id: String,
}

impl Client {
    pub fn new(token: String, ig_user_id: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self { http, token, ig_user_id }
    }

    /// Create a media container, wait for it to finish ingesting, then
    /// publish it. Returns the `ig-media-id` of the live post — store this
    /// so later caption edits can target the right media.
    pub async fn publish_photo(
        &self,
        image_url: &str,
        caption: &str,
    ) -> Result<String, Error> {
        let creation_id = self.create_container(image_url, caption).await?;
        self.wait_for_container(&creation_id).await?;
        self.publish_container(&creation_id).await
    }

    /// Update only the caption of an existing live post. The image cannot
    /// be changed — that would require delete + repost.
    pub async fn update_caption(
        &self,
        media_id: &str,
        caption: &str,
    ) -> Result<(), Error> {
        let url = format!("{API_BASE}/{media_id}");
        let resp = self
            .http
            .post(&url)
            .form(&[("caption", caption), ("access_token", &self.token)])
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        self.check_response(resp).await?;
        Ok(())
    }

    async fn create_container(&self, image_url: &str, caption: &str) -> Result<String, Error> {
        let url = format!("{API_BASE}/{}/media", self.ig_user_id);
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("image_url", image_url),
                ("caption", caption),
                ("access_token", &self.token),
            ])
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let raw = self.check_response(resp).await?;
        let parsed: CreatedId = serde_json::from_str(&raw)
            .map_err(|e| Error::Parse(format!("{e} — body: {raw}")))?;
        Ok(parsed.id)
    }

    async fn wait_for_container(&self, creation_id: &str) -> Result<(), Error> {
        let url = format!("{API_BASE}/{creation_id}");
        let mut last_code = "UNKNOWN".to_string();
        let mut last_status = String::new();
        for _ in 0..CONTAINER_POLL_ATTEMPTS {
            let resp = self
                .http
                .get(&url)
                .query(&[
                    ("fields", "status_code,status"),
                    ("access_token", &self.token),
                ])
                .send()
                .await
                .map_err(|e| Error::Transport(e.to_string()))?;
            let raw = self.check_response(resp).await?;
            let parsed: ContainerStatus = serde_json::from_str(&raw)
                .map_err(|e| Error::Parse(format!("{e} — body: {raw}")))?;
            last_code = parsed.status_code.unwrap_or_else(|| "UNKNOWN".into());
            last_status = parsed.status.unwrap_or_default();
            match last_code.as_str() {
                "FINISHED" => return Ok(()),
                "ERROR" | "EXPIRED" => {
                    return Err(Error::ContainerStuck {
                        status_code: last_code,
                        status: last_status,
                    });
                }
                _ => tokio::time::sleep(CONTAINER_POLL_INTERVAL).await,
            }
        }
        Err(Error::ContainerStuck {
            status_code: last_code,
            status: last_status,
        })
    }

    async fn publish_container(&self, creation_id: &str) -> Result<String, Error> {
        let url = format!("{API_BASE}/{}/media_publish", self.ig_user_id);
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("creation_id", creation_id),
                ("access_token", &self.token),
            ])
            .send()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        let raw = self.check_response(resp).await?;
        let parsed: CreatedId = serde_json::from_str(&raw)
            .map_err(|e| Error::Parse(format!("{e} — body: {raw}")))?;
        Ok(parsed.id)
    }

    /// Return the body text for 2xx responses; parse Meta's error envelope
    /// for non-2xx so we surface a readable message rather than a raw blob.
    async fn check_response(&self, resp: reqwest::Response) -> Result<String, Error> {
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| Error::Transport(e.to_string()))?;
        if (200..300).contains(&status) {
            return Ok(body);
        }
        if let Ok(env) = serde_json::from_str::<GraphErrorEnvelope>(&body) {
            if let Some(e) = env.error {
                return Err(Error::Api { message: e.message });
            }
        }
        Err(Error::Http { status, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_created_id_envelope() {
        let raw = r#"{"id":"18234567890123456"}"#;
        let p: CreatedId = serde_json::from_str(raw).unwrap();
        assert_eq!(p.id, "18234567890123456");
    }

    #[test]
    fn parses_container_status() {
        let raw = r#"{"status_code":"FINISHED","status":"Finished","id":"x"}"#;
        let p: ContainerStatus = serde_json::from_str(raw).unwrap();
        assert_eq!(p.status_code.as_deref(), Some("FINISHED"));
    }

    #[test]
    fn parses_graph_error_envelope() {
        let raw = r#"{"error":{"message":"Invalid OAuth access token","code":190}}"#;
        let e: GraphErrorEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(e.error.unwrap().message, "Invalid OAuth access token");
    }
}
