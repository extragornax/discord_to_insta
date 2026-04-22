use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Persistent state. Kept tiny on purpose — anything bigger probably wants its
/// own file / table.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AppState {
    /// Map from Discord channel ID → the most recent message ID the
    /// auto-reactor has already handled. New messages are those with a
    /// numerically larger (snowflake) ID.
    #[serde(default)]
    pub last_reacted_by_channel: HashMap<String, String>,
}

/// `$XDG_CONFIG_HOME/discord_to_insta/state.json`, falling back to
/// `$HOME/.config/discord_to_insta/state.json`, falling back to `./state.json`.
pub fn default_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("discord_to_insta").join("state.json");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home)
                .join(".config")
                .join("discord_to_insta")
                .join("state.json");
        }
    }
    PathBuf::from("state.json")
}

impl AppState {
    /// Load state from disk. Missing/corrupt file → default (empty) state.
    pub fn load(path: &PathBuf) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)
            .expect("AppState is trivially serializable");
        std::fs::write(path, data)
    }
}

/// Discord snowflake IDs encode a timestamp in their high bits, so numeric
/// comparison = chronological comparison. String comparison would be wrong
/// because IDs have varying lengths historically.
pub fn is_newer_snowflake(candidate: &str, reference: &str) -> bool {
    match (candidate.parse::<u64>(), reference.parse::<u64>()) {
        (Ok(a), Ok(b)) => a > b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_numeric_compare() {
        assert!(is_newer_snowflake("2000", "1000"));
        assert!(!is_newer_snowflake("1000", "2000"));
        assert!(!is_newer_snowflake("1000", "1000"));
    }

    #[test]
    fn snowflake_different_lengths() {
        // Older IDs can be shorter. 999 < 1000, don't be fooled by string cmp
        // which would say "999" > "1000" lexically.
        assert!(is_newer_snowflake("1000", "999"));
        assert!(!is_newer_snowflake("999", "1000"));
    }

    #[test]
    fn snowflake_invalid_is_not_newer() {
        assert!(!is_newer_snowflake("abc", "123"));
        assert!(!is_newer_snowflake("123", "abc"));
    }

    #[test]
    fn roundtrip_state() {
        let tmp = std::env::temp_dir().join(format!(
            "discord_to_insta_state_{}.json",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut s = AppState::default();
        s.last_reacted_by_channel
            .insert("ch1".into(), "msg42".into());
        s.save(&tmp).unwrap();
        let loaded = AppState::load(&tmp);
        assert_eq!(
            loaded.last_reacted_by_channel.get("ch1"),
            Some(&"msg42".to_string())
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
