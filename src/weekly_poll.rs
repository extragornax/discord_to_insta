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
                post_polls(&ctx, target.date()).await;
                let _guard = ctx.state_write_lock.lock().await;
                let mut st = AppState::load(&ctx.state_path);
                st.last_weekly_poll = Some(date_key);
                if let Err(e) = st.save(&ctx.state_path) {
                    push(&ctx.log, &format!("weekly-poll: state save failed: {e}")).await;
                }
            }
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

async fn post_polls(ctx: &WeeklyPollCtx, sunday: NaiveDate) {
    let monday = sunday + ChronoDuration::days(1);
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
}
