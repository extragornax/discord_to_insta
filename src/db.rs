//! Postgres logging of every Discord message the bot sees (creates,
//! edits, deletes) plus user/guild/member metadata with name-change
//! history. Schema and helpers are ported from
//! `../rust-discord-logger/src/main.rs` so the two projects share a
//! compatible database layout.
//!
//! The whole module is no-op'd by `main.rs` when `DATABASE_URL` is
//! empty — logging is opt-in.

use chrono::{DateTime, NaiveDateTime, Utc};
use sqlx::{Pool, Postgres, Row, postgres::PgPoolOptions};

#[derive(Clone)]
pub struct Db {
    pool: Pool<Postgres>,
}

#[derive(Debug, Clone)]
pub struct UserStats {
    pub user_id: i64,
    pub messages: i64,
    pub edits: i64,
    pub deletions: i64,
    /// (guild_id, guild_name_or_None, nickname)
    pub nicknames: Vec<(i64, Option<String>, String)>,
}

#[derive(Debug, Clone)]
pub struct OverviewStats {
    pub messages: i64,
    pub edits: i64,
    pub deletions: i64,
    pub users: i64,
    pub guilds: i64,
}

#[derive(Debug, Clone)]
pub struct TopAuthor {
    pub user_id: i64,
    pub username: Option<String>,
    pub messages: i64,
}

impl Db {
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<(), sqlx::Error> {
        let stmts = [
            "CREATE TABLE IF NOT EXISTS messages (
                message_id BIGINT PRIMARY KEY,
                user_id BIGINT NOT NULL,
                channel_id BIGINT NOT NULL,
                guild_id BIGINT,
                content TEXT NOT NULL,
                created_at TIMESTAMP NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS message_edits (
                id SERIAL PRIMARY KEY,
                message_id BIGINT NOT NULL,
                channel_id BIGINT NOT NULL,
                guild_id BIGINT,
                new_content TEXT NOT NULL,
                edited_at TIMESTAMP NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS message_deletions (
                id SERIAL PRIMARY KEY,
                message_id BIGINT NOT NULL,
                channel_id BIGINT NOT NULL,
                guild_id BIGINT,
                deleted_at TIMESTAMP NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS users (
                user_id BIGINT PRIMARY KEY,
                username TEXT NOT NULL,
                last_seen TIMESTAMP NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS user_names (
                id SERIAL PRIMARY KEY,
                user_id BIGINT NOT NULL,
                username TEXT NOT NULL,
                recorded_at TIMESTAMP NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS guilds (
                guild_id BIGINT PRIMARY KEY,
                guild_name TEXT NOT NULL,
                last_seen TIMESTAMP NOT NULL
            )",
            "CREATE TABLE IF NOT EXISTS guild_members (
                user_id BIGINT NOT NULL,
                guild_id BIGINT NOT NULL,
                nickname TEXT,
                last_seen TIMESTAMP NOT NULL,
                PRIMARY KEY (user_id, guild_id)
            )",
            "CREATE TABLE IF NOT EXISTS guild_member_nicknames (
                id SERIAL PRIMARY KEY,
                user_id BIGINT NOT NULL,
                guild_id BIGINT NOT NULL,
                nickname TEXT,
                recorded_at TIMESTAMP NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS idx_messages_user_id ON messages(user_id)",
            "CREATE INDEX IF NOT EXISTS idx_message_edits_message_id ON message_edits(message_id)",
            "CREATE INDEX IF NOT EXISTS idx_message_deletions_message_id ON message_deletions(message_id)",
        ];
        for stmt in stmts {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
    }

    pub async fn log_user(&self, user_id: i64, username: &str) {
        let now = Utc::now().naive_utc();
        match sqlx::query("SELECT username FROM users WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(row)) => {
                let stored: String = row.get("username");
                if stored != username {
                    let _ = sqlx::query(
                        "UPDATE users SET username = $1, last_seen = $2 WHERE user_id = $3",
                    )
                    .bind(username)
                    .bind(now)
                    .bind(user_id)
                    .execute(&self.pool)
                    .await;
                    let _ = sqlx::query(
                        "INSERT INTO user_names (user_id, username, recorded_at) VALUES ($1, $2, $3)",
                    )
                    .bind(user_id)
                    .bind(username)
                    .bind(now)
                    .execute(&self.pool)
                    .await;
                } else {
                    let _ = sqlx::query("UPDATE users SET last_seen = $1 WHERE user_id = $2")
                        .bind(now)
                        .bind(user_id)
                        .execute(&self.pool)
                        .await;
                }
            }
            Ok(None) => {
                let _ = sqlx::query(
                    "INSERT INTO users (user_id, username, last_seen) VALUES ($1, $2, $3)
                     ON CONFLICT (user_id) DO NOTHING",
                )
                .bind(user_id)
                .bind(username)
                .bind(now)
                .execute(&self.pool)
                .await;
                let _ = sqlx::query(
                    "INSERT INTO user_names (user_id, username, recorded_at) VALUES ($1, $2, $3)",
                )
                .bind(user_id)
                .bind(username)
                .bind(now)
                .execute(&self.pool)
                .await;
            }
            Err(e) => eprintln!("db: log_user lookup failed: {e}"),
        }
    }

    pub async fn log_guild(&self, guild_id: i64, guild_name: &str) {
        let now = Utc::now().naive_utc();
        match sqlx::query("SELECT guild_name FROM guilds WHERE guild_id = $1")
            .bind(guild_id)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(row)) => {
                let stored: String = row.get("guild_name");
                if stored != guild_name {
                    let _ = sqlx::query(
                        "UPDATE guilds SET guild_name = $1, last_seen = $2 WHERE guild_id = $3",
                    )
                    .bind(guild_name)
                    .bind(now)
                    .bind(guild_id)
                    .execute(&self.pool)
                    .await;
                } else {
                    let _ = sqlx::query("UPDATE guilds SET last_seen = $1 WHERE guild_id = $2")
                        .bind(now)
                        .bind(guild_id)
                        .execute(&self.pool)
                        .await;
                }
            }
            Ok(None) => {
                let _ = sqlx::query(
                    "INSERT INTO guilds (guild_id, guild_name, last_seen) VALUES ($1, $2, $3)
                     ON CONFLICT (guild_id) DO NOTHING",
                )
                .bind(guild_id)
                .bind(guild_name)
                .bind(now)
                .execute(&self.pool)
                .await;
            }
            Err(e) => eprintln!("db: log_guild lookup failed: {e}"),
        }
    }

    pub async fn log_member(&self, user_id: i64, guild_id: i64, nickname: Option<&str>) {
        let now = Utc::now().naive_utc();
        match sqlx::query(
            "SELECT nickname FROM guild_members WHERE user_id = $1 AND guild_id = $2",
        )
        .bind(user_id)
        .bind(guild_id)
        .fetch_optional(&self.pool)
        .await
        {
            Ok(Some(row)) => {
                let stored: Option<String> = row.get("nickname");
                let changed = match (&stored, nickname) {
                    (Some(s), Some(n)) => s != n,
                    (None, None) => false,
                    _ => true,
                };
                if changed {
                    let _ = sqlx::query(
                        "UPDATE guild_members SET nickname = $1, last_seen = $2 WHERE user_id = $3 AND guild_id = $4",
                    )
                    .bind(nickname)
                    .bind(now)
                    .bind(user_id)
                    .bind(guild_id)
                    .execute(&self.pool)
                    .await;
                    let _ = sqlx::query(
                        "INSERT INTO guild_member_nicknames (user_id, guild_id, nickname, recorded_at) VALUES ($1, $2, $3, $4)",
                    )
                    .bind(user_id)
                    .bind(guild_id)
                    .bind(nickname)
                    .bind(now)
                    .execute(&self.pool)
                    .await;
                } else {
                    let _ = sqlx::query(
                        "UPDATE guild_members SET last_seen = $1 WHERE user_id = $2 AND guild_id = $3",
                    )
                    .bind(now)
                    .bind(user_id)
                    .bind(guild_id)
                    .execute(&self.pool)
                    .await;
                }
            }
            Ok(None) => {
                let _ = sqlx::query(
                    "INSERT INTO guild_members (user_id, guild_id, nickname, last_seen) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (user_id, guild_id) DO NOTHING",
                )
                .bind(user_id)
                .bind(guild_id)
                .bind(nickname)
                .bind(now)
                .execute(&self.pool)
                .await;
                let _ = sqlx::query(
                    "INSERT INTO guild_member_nicknames (user_id, guild_id, nickname, recorded_at) VALUES ($1, $2, $3, $4)",
                )
                .bind(user_id)
                .bind(guild_id)
                .bind(nickname)
                .bind(now)
                .execute(&self.pool)
                .await;
            }
            Err(e) => eprintln!("db: log_member lookup failed: {e}"),
        }
    }

    pub async fn log_message(
        &self,
        message_id: i64,
        user_id: i64,
        channel_id: i64,
        guild_id: Option<i64>,
        content: &str,
        created_at: DateTime<Utc>,
    ) {
        let ts: NaiveDateTime = created_at.naive_utc();
        let res = sqlx::query(
            "INSERT INTO messages (message_id, user_id, channel_id, guild_id, content, created_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (message_id) DO NOTHING",
        )
        .bind(message_id)
        .bind(user_id)
        .bind(channel_id)
        .bind(guild_id)
        .bind(content)
        .bind(ts)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            eprintln!("db: log_message failed: {e}");
        }
    }

    pub async fn log_edit(
        &self,
        message_id: i64,
        channel_id: i64,
        guild_id: Option<i64>,
        new_content: &str,
    ) {
        let now = Utc::now().naive_utc();
        let res = sqlx::query(
            "INSERT INTO message_edits (message_id, channel_id, guild_id, new_content, edited_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(message_id)
        .bind(channel_id)
        .bind(guild_id)
        .bind(new_content)
        .bind(now)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            eprintln!("db: log_edit failed: {e}");
        }
    }

    pub async fn log_delete(
        &self,
        message_id: i64,
        channel_id: i64,
        guild_id: Option<i64>,
    ) {
        let now = Utc::now().naive_utc();
        let res = sqlx::query(
            "INSERT INTO message_deletions (message_id, channel_id, guild_id, deleted_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(message_id)
        .bind(channel_id)
        .bind(guild_id)
        .bind(now)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            eprintln!("db: log_delete failed: {e}");
        }
    }

    pub async fn user_stats(&self, user_id: i64) -> Result<UserStats, sqlx::Error> {
        let messages: i64 = sqlx::query("SELECT COUNT(*) FROM messages WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?
            .get(0);
        let edits: i64 = sqlx::query(
            "SELECT COUNT(*) FROM message_edits me
             JOIN messages m ON me.message_id = m.message_id
             WHERE m.user_id = $1",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?
        .get(0);
        let deletions: i64 = sqlx::query(
            "SELECT COUNT(*) FROM message_deletions md
             JOIN messages m ON md.message_id = m.message_id
             WHERE m.user_id = $1",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?
        .get(0);
        let rows = sqlx::query(
            "SELECT gm.guild_id, gm.nickname, g.guild_name
             FROM guild_members gm
             LEFT JOIN guilds g ON gm.guild_id = g.guild_id
             WHERE gm.user_id = $1 AND gm.nickname IS NOT NULL",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        let mut nicknames = Vec::with_capacity(rows.len());
        for row in rows {
            let gid: i64 = row.get("guild_id");
            let nick: String = row.get("nickname");
            let gname: Option<String> = row.get("guild_name");
            nicknames.push((gid, gname, nick));
        }
        Ok(UserStats {
            user_id,
            messages,
            edits,
            deletions,
            nicknames,
        })
    }

    pub async fn overview_stats(&self) -> Result<OverviewStats, sqlx::Error> {
        let messages: i64 = sqlx::query("SELECT COUNT(*) FROM messages")
            .fetch_one(&self.pool)
            .await?
            .get(0);
        let edits: i64 = sqlx::query("SELECT COUNT(*) FROM message_edits")
            .fetch_one(&self.pool)
            .await?
            .get(0);
        let deletions: i64 = sqlx::query("SELECT COUNT(*) FROM message_deletions")
            .fetch_one(&self.pool)
            .await?
            .get(0);
        let users: i64 = sqlx::query("SELECT COUNT(*) FROM users")
            .fetch_one(&self.pool)
            .await?
            .get(0);
        let guilds: i64 = sqlx::query("SELECT COUNT(*) FROM guilds")
            .fetch_one(&self.pool)
            .await?
            .get(0);
        Ok(OverviewStats {
            messages,
            edits,
            deletions,
            users,
            guilds,
        })
    }

    pub async fn top_authors(&self, limit: i64) -> Result<Vec<TopAuthor>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT m.user_id, u.username, COUNT(*) AS msg_count
             FROM messages m
             LEFT JOIN users u ON u.user_id = m.user_id
             GROUP BY m.user_id, u.username
             ORDER BY msg_count DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(TopAuthor {
                user_id: row.get("user_id"),
                username: row.get("username"),
                messages: row.get("msg_count"),
            });
        }
        Ok(out)
    }
}
