//! Invite-link moderation. Watches every MESSAGE_CREATE the gateway can
//! see (any channel), warns users who post Discord invite links, and
//! escalates when the same user posts the same invite in more than one
//! channel: the operator gets a DM and the offending messages are deleted.
//!
//! Escalation policy, per (user, invite code):
//! - first channel: reply in-channel with a warning (every offending message);
//! - second distinct channel: DM the operator once, delete every recorded
//!   offending message (and every later one for that pair);
//! - deletion failure: post a notice in the channel where it failed —
//!   at most once per user, across all channels.
//!
//! Tracking is in-memory only; a restart forgets past offenses. Good enough
//! for a small community server — a spammer restarting the clock still gets
//! warned again on the next link.
//!
//! Needs Send Messages (warnings/notices) and Manage Messages (deletes) on
//! the channels it moderates.

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::discord;

/// Operator DM'd when a cross-channel invite spammer is detected.
const ALERT_USER_ID: &str = "222353499638202369";

/// Matches discord.gg/CODE, discord.com/invite/CODE and
/// discordapp.com/invite/CODE (any casing, any subdomain). The `\b` keeps
/// look-alike hosts (mydiscord.gg) from matching.
static INVITE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:discord(?:app)?\.com/invite|discord\.gg)/([A-Za-z0-9-]+)").unwrap()
});

/// Extract the distinct invite codes in a message body, in order of first
/// appearance. Codes are case-sensitive (Discord treats them as such).
fn find_invite_codes(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    INVITE_RE
        .captures_iter(text)
        .map(|c| c[1].to_string())
        .filter(|code| seen.insert(code.clone()))
        .collect()
}

#[derive(Default)]
struct Offense {
    /// Distinct channels this (user, code) pair was posted in.
    channels: HashSet<String>,
    /// (channel_id, message_id) of offending messages not yet handed to a
    /// delete attempt. Drained when escalation fires; failed deletes are
    /// not retried.
    messages: Vec<(String, String)>,
    /// The operator DM has been sent for this pair.
    escalated: bool,
}

#[derive(Debug, PartialEq)]
enum Action {
    /// Single-channel offense so far: warn in the channel.
    Warn,
    /// Cross-channel offense: delete these messages; DM the operator when
    /// `dm` is true (first escalation for the pair only).
    Delete {
        messages: Vec<(String, String)>,
        dm: bool,
    },
}

#[derive(Default)]
struct Tracker {
    offenses: HashMap<(String, String), Offense>,
    /// Users for whom the "couldn't delete" notice was already posted.
    delete_failure_notified: HashSet<String>,
}

impl Tracker {
    fn record(&mut self, user_id: &str, code: &str, channel_id: &str, message_id: &str) -> Action {
        let offense = self
            .offenses
            .entry((user_id.to_string(), code.to_string()))
            .or_default();
        offense.channels.insert(channel_id.to_string());
        offense
            .messages
            .push((channel_id.to_string(), message_id.to_string()));

        if offense.channels.len() >= 2 {
            let dm = !offense.escalated;
            offense.escalated = true;
            Action::Delete {
                messages: std::mem::take(&mut offense.messages),
                dm,
            }
        } else {
            Action::Warn
        }
    }

    /// True the first time it's called for a given user, false after.
    fn should_notify_delete_failure(&mut self, user_id: &str) -> bool {
        self.delete_failure_notified.insert(user_id.to_string())
    }
}

pub struct Moderator {
    client: Arc<discord::Client>,
    log: Arc<Mutex<VecDeque<String>>>,
    tracker: Mutex<Tracker>,
}

/// Gateway hook: cheap synchronous checks on the raw MESSAGE_CREATE payload,
/// then spawn the (network-bound) enforcement so the gateway receive loop
/// isn't blocked behind REST calls.
pub fn inspect_create(moderator: &Arc<Moderator>, d: &Value) {
    if d["author"]["bot"].as_bool().unwrap_or(false) {
        return;
    }
    let (Some(channel_id), Some(message_id), Some(user_id)) = (
        d["channel_id"].as_str(),
        d["id"].as_str(),
        d["author"]["id"].as_str(),
    ) else {
        return;
    };
    let content = d["content"].as_str().unwrap_or("");
    // Without the MESSAGE_CONTENT intent, `content` arrives empty and the
    // task below re-fetches the body via REST. With the intent, skip clean
    // messages here without spawning anything.
    if !content.is_empty() && find_invite_codes(content).is_empty() {
        return;
    }

    let moderator = moderator.clone();
    let channel_id = channel_id.to_string();
    let message_id = message_id.to_string();
    let user_id = user_id.to_string();
    let content = content.to_string();
    tokio::spawn(async move {
        moderator
            .process(&channel_id, &message_id, &user_id, content)
            .await;
    });
}

impl Moderator {
    pub fn new(client: Arc<discord::Client>, log: Arc<Mutex<VecDeque<String>>>) -> Self {
        Self {
            client,
            log,
            tracker: Mutex::new(Tracker::default()),
        }
    }

    async fn process(&self, channel_id: &str, message_id: &str, user_id: &str, content: String) {
        let content = if content.is_empty() {
            match self.client.fetch_message(channel_id, message_id).await {
                Ok(m) => m.content,
                Err(_) => return,
            }
        } else {
            content
        };
        let codes = find_invite_codes(&content);
        if codes.is_empty() {
            return;
        }

        let actions: Vec<(String, Action)> = {
            let mut tracker = self.tracker.lock().await;
            codes
                .into_iter()
                .map(|code| {
                    let action = tracker.record(user_id, &code, channel_id, message_id);
                    (code, action)
                })
                .collect()
        };

        let mut deletes: Vec<(String, String)> = Vec::new();
        let mut dm_codes: Vec<String> = Vec::new();
        for (code, action) in actions {
            if let Action::Delete { messages, dm } = action {
                if dm {
                    dm_codes.push(code);
                }
                deletes.extend(messages);
            }
        }

        // A message that only warrants warnings (all its codes are still
        // single-channel) gets exactly one warning, however many links it has.
        if deletes.is_empty() {
            let warning = format!(
                "⚠️ <@{user_id}> Les liens d'invitation Discord ne sont pas autorisés ici."
            );
            match self.client.send_message(channel_id, &warning).await {
                Ok(()) => {
                    self.push(&format!(
                        "moderation: warned {user_id} in {channel_id} (invite link)"
                    ))
                    .await
                }
                Err(e) => {
                    self.push(&format!("moderation: warn failed in {channel_id}: {e}"))
                        .await
                }
            }
            return;
        }

        // DM the operator before deleting, so the alert lands even when the
        // deletes are about to fail on permissions.
        for code in dm_codes {
            let alert = format!(
                "🚨 <@{user_id}> (id {user_id}) a posté le lien d'invitation \
                 discord.gg/{code} dans plusieurs salons. Suppression des messages tentée."
            );
            let sent = match self.client.create_dm(ALERT_USER_ID).await {
                Ok(dm_channel) => self.client.send_message(&dm_channel, &alert).await,
                Err(e) => Err(e),
            };
            match sent {
                Ok(()) => {
                    self.push(&format!(
                        "moderation: alerted operator — {user_id} spammed {code} cross-channel"
                    ))
                    .await
                }
                Err(e) => {
                    self.push(&format!("moderation: operator DM failed: {e}"))
                        .await
                }
            }
        }

        deletes.sort();
        deletes.dedup();
        let mut any_failed = false;
        for (del_channel, del_message) in &deletes {
            match self.client.delete_message(del_channel, del_message).await {
                Ok(()) => {
                    self.push(&format!(
                        "moderation: deleted invite message {del_message} in {del_channel}"
                    ))
                    .await
                }
                Err(e) => {
                    any_failed = true;
                    self.push(&format!(
                        "moderation: delete of {del_message} in {del_channel} failed: {e}"
                    ))
                    .await;
                }
            }
        }

        if any_failed {
            let first_failure = {
                let mut tracker = self.tracker.lock().await;
                tracker.should_notify_delete_failure(user_id)
            };
            if first_failure {
                let notice = format!(
                    "⚠️ Impossible de supprimer le(s) message(s) d'invitation de <@{user_id}> \
                     — permission « Gérer les messages » manquante ?"
                );
                if let Err(e) = self.client.send_message(channel_id, &notice).await {
                    self.push(&format!(
                        "moderation: delete-failure notice failed in {channel_id}: {e}"
                    ))
                    .await;
                }
            }
        }
    }

    async fn push(&self, line: &str) {
        const LOG_MAX_LINES: usize = 40;
        let mut log = self.log.lock().await;
        while log.len() >= LOG_MAX_LINES {
            log.pop_front();
        }
        log.push_back(line.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_invite_codes_all_hosts() {
        assert_eq!(find_invite_codes("join discord.gg/abc123"), vec!["abc123"]);
        assert_eq!(
            find_invite_codes("https://discord.com/invite/xYz-9"),
            vec!["xYz-9"]
        );
        assert_eq!(
            find_invite_codes("DISCORDAPP.COM/INVITE/Old1"),
            vec!["Old1"]
        );
        assert_eq!(
            find_invite_codes("<https://canary.discord.gg/sub>"),
            vec!["sub"]
        );
    }

    #[test]
    fn ignores_non_invites() {
        assert!(find_invite_codes("bonjour tout le monde").is_empty());
        assert!(find_invite_codes("https://discord.com/channels/1/2/3").is_empty());
        assert!(find_invite_codes("mydiscord.gg/fake").is_empty());
    }

    #[test]
    fn dedups_and_stops_at_punctuation() {
        assert_eq!(
            find_invite_codes("discord.gg/abc discord.gg/abc!"),
            vec!["abc"]
        );
        assert_eq!(
            find_invite_codes("discord.gg/one et discord.gg/two"),
            vec!["one", "two"]
        );
    }

    #[test]
    fn same_channel_repeats_only_warn() {
        let mut t = Tracker::default();
        assert_eq!(t.record("u1", "code", "chan1", "m1"), Action::Warn);
        assert_eq!(t.record("u1", "code", "chan1", "m2"), Action::Warn);
    }

    #[test]
    fn second_channel_escalates_with_all_messages_and_one_dm() {
        let mut t = Tracker::default();
        assert_eq!(t.record("u1", "code", "chan1", "m1"), Action::Warn);
        assert_eq!(
            t.record("u1", "code", "chan2", "m2"),
            Action::Delete {
                messages: vec![("chan1".into(), "m1".into()), ("chan2".into(), "m2".into())],
                dm: true,
            }
        );
        // Later messages for the same pair: delete only the new one, no DM.
        assert_eq!(
            t.record("u1", "code", "chan1", "m3"),
            Action::Delete {
                messages: vec![("chan1".into(), "m3".into())],
                dm: false,
            }
        );
    }

    #[test]
    fn different_code_or_user_tracked_separately() {
        let mut t = Tracker::default();
        assert_eq!(t.record("u1", "code", "chan1", "m1"), Action::Warn);
        assert_eq!(t.record("u1", "other", "chan2", "m2"), Action::Warn);
        assert_eq!(t.record("u2", "code", "chan2", "m3"), Action::Warn);
    }

    #[test]
    fn delete_failure_notice_once_per_user() {
        let mut t = Tracker::default();
        assert!(t.should_notify_delete_failure("u1"));
        assert!(!t.should_notify_delete_failure("u1"));
        assert!(t.should_notify_delete_failure("u2"));
    }
}
