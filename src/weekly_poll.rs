//! Weekly attendance polls. Every Sunday at 18:00 (server-local time) the
//! bot posts two native Discord polls to the configured channel:
//!
//! 1. « Qui sera présent lundi {date} ? » — single choice, the date being
//!    the Monday right after the posting Sunday;
//! 2. « Ceux/Celles présent•e•s, quel(s) rôles(s) êtes-vous prêt•e à
//!    remplir ? » — multi-select.
//!
//! Both polls run for 24 h. `state.last_weekly_poll` records the Sunday
//! (ISO date) the polls were last posted for, so a restart doesn't
//! double-post. A launch that missed the slot still posts as long as the
//! Sunday-18:00 target is less than 24 h in the past — after that the
//! window is gone until next week (the poll would outlive its Monday).
//! Send failures are logged, not retried; the date is marked handled
//! either way so a partial failure can't spam the channel every minute.
//!
//! **Manual trigger**: the `/poll` slash command (registered per-guild on
//! gateway READY, so it shows up in Discord's command picker with
//! autocompletion) posts both polls immediately, dated the coming Monday.
//! Only accepted from the poll channel — elsewhere it gets an ephemeral
//! "wrong channel" reply. It does NOT touch `state.last_weekly_poll`, so
//! the Sunday auto-post still fires on schedule.
//!
//! Times use the process-local timezone — set `TZ` (e.g. `Europe/Paris`)
//! in the container or the slot means 18:00 UTC.

use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, NaiveDateTime, NaiveTime};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::discord;
use crate::state::AppState;

/// How long each poll accepts votes, in hours.
const POLL_DURATION_HOURS: u32 = 24;
/// How often the scheduler re-checks the clock. Coarse on purpose — the
/// slot is a whole day wide, a minute of jitter is irrelevant.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// (emoji, label) pairs for the presence poll — single choice.
const PRESENCE_ANSWERS: &[(&str, &str)] = &[
    ("✅", "Présent"),
    ("🚫", "Absent"),
    ("🤔", "Ne sais pas encore"),
    ("😎", "En mode touriste"),
];

/// (emoji, label) pairs for the roles poll — multi-select.
const ROLES_ANSWERS: &[(&str, &str)] = &[
    ("🚂", "Guides"),
    ("⚔️", "Encadrants/Bloquants"),
    ("🔚", "Fermants"),
];

const ROLES_QUESTION: &str =
    "Ceux/Celles présent•e•s, quel(s) rôles(s) êtes-vous prêt•e à remplir ?";

pub struct WeeklyPollCtx {
    pub client: Arc<discord::Client>,
    pub log: Arc<Mutex<VecDeque<String>>>,
    pub channel_id: String,
    pub state_path: PathBuf,
    /// Same lock as every other state.json writer — see `AppCtx`.
    pub state_write_lock: Arc<Mutex<()>>,
    /// Poked by the gateway when someone uses `/poll` in the channel.
    pub trigger: Arc<tokio::sync::Notify>,
}

/// Handed to the gateway so it can register the slash command on READY and
/// fire the manual trigger on INTERACTION_CREATE, without depending on the
/// whole `WeeklyPollCtx`.
pub struct ManualTrigger {
    pub channel_id: String,
    /// Guild the `/poll` command is registered in. Guild commands are
    /// instant; global ones take up to an hour to propagate.
    pub guild_id: String,
    pub notify: Arc<tokio::sync::Notify>,
}

const COMMAND_NAME: &str = "poll";
const COMMAND_DESCRIPTION: &str = "Poste les sondages de présence de la semaine";

/// Gateway hook, called on READY. Registers the `/poll` guild command so it
/// autocompletes in Discord's command picker. Registration is idempotent
/// (same name → update), so re-running on every reconnect is fine.
pub fn register_command(
    trigger: &ManualTrigger,
    client: &Arc<discord::Client>,
    application_id: &str,
    log: &Arc<Mutex<VecDeque<String>>>,
) {
    if trigger.guild_id.is_empty() || application_id.is_empty() {
        return;
    }
    let client = client.clone();
    let application_id = application_id.to_string();
    let guild_id = trigger.guild_id.clone();
    let log = log.clone();
    tokio::spawn(async move {
        match client
            .register_guild_command(
                &application_id,
                &guild_id,
                COMMAND_NAME,
                COMMAND_DESCRIPTION,
            )
            .await
        {
            Ok(()) => push(&log, "weekly-poll: /poll command registered").await,
            Err(e) => {
                push(
                    &log,
                    &format!("weekly-poll: /poll registration failed: {e}"),
                )
                .await
            }
        }
    });
}

/// Gateway hook, called on INTERACTION_CREATE. Acknowledges the `/poll`
/// command (ephemeral) and fires the trigger when it comes from the poll
/// channel; elsewhere it answers with a redirect. Interactions must be
/// acked within 3 s, hence the immediate spawn.
pub fn handle_interaction(
    trigger: &ManualTrigger,
    d: &serde_json::Value,
    client: Option<&Arc<discord::Client>>,
    log: &Arc<Mutex<VecDeque<String>>>,
) {
    // Type 2 = APPLICATION_COMMAND.
    if d["type"].as_u64() != Some(2) || d["data"]["name"].as_str() != Some(COMMAND_NAME) {
        return;
    }
    let (Some(interaction_id), Some(token), Some(client)) =
        (d["id"].as_str(), d["token"].as_str(), client)
    else {
        return;
    };
    let in_poll_channel = d["channel_id"].as_str() == Some(trigger.channel_id.as_str());

    let client = client.clone();
    let interaction_id = interaction_id.to_string();
    let token = token.to_string();
    let notify = trigger.notify.clone();
    let poll_channel = trigger.channel_id.clone();
    let log = log.clone();
    tokio::spawn(async move {
        let reply = if in_poll_channel {
            "📊 Sondages en route !".to_string()
        } else {
            format!("À utiliser dans <#{poll_channel}>.")
        };
        if let Err(e) = client
            .interaction_reply(&interaction_id, &token, &reply, true)
            .await
        {
            push(&log, &format!("weekly-poll: interaction ack failed: {e}")).await;
        }
        if in_poll_channel {
            notify.notify_one();
        }
    });
}

/// Scheduler loop, spawned once at startup when the feature is configured.
/// Lives for the process, like the gateway task.
pub async fn run(ctx: WeeklyPollCtx) {
    push(
        &ctx.log,
        &format!("weekly-poll: armed for channel {}", ctx.channel_id),
    )
    .await;
    loop {
        let now = Local::now().naive_local();
        let target = last_sunday_1800(now);
        if now - target < ChronoDuration::hours(24) {
            let date_key = target.date().to_string();
            let already = {
                let _guard = ctx.state_write_lock.lock().await;
                AppState::load(&ctx.state_path).last_weekly_poll.as_deref()
                    == Some(date_key.as_str())
            };
            if !already {
                post_polls(&ctx, target.date() + ChronoDuration::days(1)).await;
                let _guard = ctx.state_write_lock.lock().await;
                let mut st = AppState::load(&ctx.state_path);
                st.last_weekly_poll = Some(date_key);
                if let Err(e) = st.save(&ctx.state_path) {
                    push(&ctx.log, &format!("weekly-poll: state save failed: {e}")).await;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(CHECK_INTERVAL) => {}
            _ = ctx.trigger.notified() => {
                push(&ctx.log, "weekly-poll: manual trigger (/poll)").await;
                post_polls(&ctx, next_monday(Local::now().date_naive())).await;
            }
        }
    }
}

/// The coming Monday: `date` itself when it already is one.
fn next_monday(date: NaiveDate) -> NaiveDate {
    let days_ahead = (7 - date.weekday().num_days_from_monday() as i64) % 7;
    date + ChronoDuration::days(days_ahead)
}

async fn post_polls(ctx: &WeeklyPollCtx, monday: NaiveDate) {
    let polls: [(String, &[(&str, &str)], bool); 2] = [
        (presence_question(monday), PRESENCE_ANSWERS, false),
        (ROLES_QUESTION.to_string(), ROLES_ANSWERS, true),
    ];
    for (question, answers, multiselect) in polls {
        match ctx
            .client
            .send_poll(
                &ctx.channel_id,
                &question,
                answers,
                POLL_DURATION_HOURS,
                multiselect,
            )
            .await
        {
            Ok(()) => push(&ctx.log, &format!("weekly-poll: posted « {question} »")).await,
            Err(e) => {
                push(
                    &ctx.log,
                    &format!("weekly-poll: post failed for « {question} »: {e}"),
                )
                .await
            }
        }
        // Space the two sends out a little, same spirit as REACT_DELAY.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// The most recent Sunday 18:00 at or before `now`.
fn last_sunday_1800(now: NaiveDateTime) -> NaiveDateTime {
    let days_since_sunday = now.date().weekday().num_days_from_sunday() as i64;
    let sunday = now.date() - ChronoDuration::days(days_since_sunday);
    let candidate = sunday.and_time(NaiveTime::from_hms_opt(18, 0, 0).expect("valid time"));
    if candidate > now {
        candidate - ChronoDuration::days(7)
    } else {
        candidate
    }
}

/// « Qui sera présent lundi 20 juillet 2026 ? »
fn presence_question(monday: NaiveDate) -> String {
    format!(
        "Qui sera présent lundi {} {} {} ?",
        monday.day(),
        french_month(monday.month()),
        monday.year()
    )
}

fn french_month(month: u32) -> &'static str {
    match month {
        1 => "janvier",
        2 => "février",
        3 => "mars",
        4 => "avril",
        5 => "mai",
        6 => "juin",
        7 => "juillet",
        8 => "août",
        9 => "septembre",
        10 => "octobre",
        11 => "novembre",
        12 => "décembre",
        _ => unreachable!("chrono months are 1-12"),
    }
}

async fn push(log: &Arc<Mutex<VecDeque<String>>>, line: &str) {
    const LOG_MAX_LINES: usize = 40;
    let mut l = log.lock().await;
    while l.len() >= LOG_MAX_LINES {
        l.pop_front();
    }
    l.push_back(line.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_time(NaiveTime::from_hms_opt(h, min, 0).unwrap())
    }

    #[test]
    fn presence_question_french_date() {
        // 2026-07-20 is a Monday.
        let monday = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap();
        assert_eq!(
            presence_question(monday),
            "Qui sera présent lundi 20 juillet 2026 ?"
        );
    }

    #[test]
    fn presence_question_no_leading_zero() {
        let monday = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        assert_eq!(
            presence_question(monday),
            "Qui sera présent lundi 3 août 2026 ?"
        );
    }

    #[test]
    fn last_sunday_midweek_goes_back_to_sunday() {
        // Wednesday 2026-07-22 → Sunday 2026-07-19 18:00.
        assert_eq!(
            last_sunday_1800(dt(2026, 7, 22, 12, 0)),
            dt(2026, 7, 19, 18, 0)
        );
    }

    #[test]
    fn last_sunday_before_1800_uses_previous_week() {
        // Sunday 2026-07-19 at 17:59 → previous Sunday 2026-07-12 18:00.
        assert_eq!(
            last_sunday_1800(dt(2026, 7, 19, 17, 59)),
            dt(2026, 7, 12, 18, 0)
        );
    }

    #[test]
    fn last_sunday_at_1800_is_that_sunday() {
        assert_eq!(
            last_sunday_1800(dt(2026, 7, 19, 18, 0)),
            dt(2026, 7, 19, 18, 0)
        );
    }

    #[test]
    fn monday_after_sunday() {
        let sunday = NaiveDate::from_ymd_opt(2026, 7, 19).unwrap();
        let monday = sunday + ChronoDuration::days(1);
        assert_eq!(monday, NaiveDate::from_ymd_opt(2026, 7, 20).unwrap());
    }

    #[test]
    fn next_monday_from_each_weekday() {
        let monday = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap();
        // A Monday maps to itself.
        assert_eq!(next_monday(monday), monday);
        // Tuesday → the following Monday.
        assert_eq!(
            next_monday(NaiveDate::from_ymd_opt(2026, 7, 21).unwrap()),
            NaiveDate::from_ymd_opt(2026, 7, 27).unwrap()
        );
        // Sunday → the very next day.
        assert_eq!(
            next_monday(NaiveDate::from_ymd_opt(2026, 7, 19).unwrap()),
            monday
        );
    }
}
