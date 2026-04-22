use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
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

    /// Per-channel, per-message, the set of emojis already reacted.
    /// Written after every successful reaction so a crash mid-batch resumes
    /// cleanly and doesn't re-hit the API for emojis already in place.
    /// Entries are cleared once all expected emojis are on a message (at
    /// which point `last_reacted_by_channel` advances past it), keeping
    /// this map bounded to messages currently in flight.
    #[serde(default)]
    pub reactions_done_by_channel:
        HashMap<String, HashMap<String, BTreeSet<String>>>,
}

impl AppState {
    pub fn has_reacted(&self, channel: &str, message: &str, emoji: &str) -> bool {
        self.reactions_done_by_channel
            .get(channel)
            .and_then(|m| m.get(message))
            .map(|s| s.contains(emoji))
            .unwrap_or(false)
    }

    pub fn record_reaction(&mut self, channel: &str, message: &str, emoji: &str) {
        self.reactions_done_by_channel
            .entry(channel.to_string())
            .or_default()
            .entry(message.to_string())
            .or_default()
            .insert(emoji.to_string());
    }

    /// Drop the per-emoji tracking for a message once its full reaction set
    /// has been placed. Called when `last_reacted_by_channel` advances past
    /// the message.
    pub fn clear_reactions(&mut self, channel: &str, message: &str) {
        if let Some(m) = self.reactions_done_by_channel.get_mut(channel) {
            m.remove(message);
            if m.is_empty() {
                self.reactions_done_by_channel.remove(channel);
            }
        }
    }
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
        s.record_reaction("ch2", "msg99", "✅");
        s.save(&tmp).unwrap();
        let loaded = AppState::load(&tmp);
        assert_eq!(
            loaded.last_reacted_by_channel.get("ch1"),
            Some(&"msg42".to_string())
        );
        assert!(loaded.has_reacted("ch2", "msg99", "✅"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn reactions_record_and_check() {
        let mut s = AppState::default();
        assert!(!s.has_reacted("ch", "m", "✅"));
        s.record_reaction("ch", "m", "✅");
        assert!(s.has_reacted("ch", "m", "✅"));
        assert!(!s.has_reacted("ch", "m", "🚫"));
        s.record_reaction("ch", "m", "🚫");
        assert!(s.has_reacted("ch", "m", "🚫"));
        // Idempotent.
        s.record_reaction("ch", "m", "✅");
        assert!(s.has_reacted("ch", "m", "✅"));
    }

    #[test]
    fn reactions_clear_removes_and_prunes_empty_channel() {
        let mut s = AppState::default();
        s.record_reaction("ch", "m1", "✅");
        s.record_reaction("ch", "m2", "🚫");
        s.clear_reactions("ch", "m1");
        assert!(!s.has_reacted("ch", "m1", "✅"));
        assert!(s.has_reacted("ch", "m2", "🚫"));
        s.clear_reactions("ch", "m2");
        assert!(s.reactions_done_by_channel.get("ch").is_none());
    }
}
