use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use self_dis_protocol::{Channel, ChannelKind, ChatMessage, Role, User};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

use crate::error::Error;
use crate::validate;

const SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn format_ms(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}

pub async fn open(data_dir: &Path) -> Result<SqlitePool, Error> {
    tokio::fs::create_dir_all(data_dir)
        .await
        .map_err(|err| Error::Internal(format!("не удалось создать каталог данных: {err}")))?;

    let options = SqliteConnectOptions::new()
        .filename(data_dir.join("app.db"))
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .map_err(|err| Error::Internal(format!("не удалось открыть базу: {err}")))?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .map_err(|err| Error::Internal(format!("миграция базы: {err}")))?;

    seed_text_channel(&pool).await?;
    Ok(pool)
}

async fn seed_text_channel(pool: &SqlitePool) -> Result<(), Error> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channels WHERE kind = 'text'")
        .fetch_one(pool)
        .await
        .map_err(map_sql)?;
    if count == 0 {
        sqlx::query(
            "INSERT INTO channels (name, kind, position, created_at) VALUES ('общий', 'text', 0, ?)",
        )
        .bind(now_ms())
        .execute(pool)
        .await
        .map_err(map_sql)?;
    }
    Ok(())
}

pub async fn create_user(
    pool: &SqlitePool,
    username: &str,
    password: &str,
    role: Role,
) -> Result<User, Error> {
    let username = validate::username(username)?;
    let password = validate::password(password)?;
    let password_hash = hash_password(password)?;
    let result = sqlx::query(
        "INSERT INTO users (username, password_hash, role, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&username)
    .bind(password_hash)
    .bind(role_name(role))
    .bind(now_ms())
    .execute(pool)
    .await
    .map_err(map_sql)?;

    Ok(User {
        id: result.last_insert_rowid(),
        username,
        role,
    })
}

pub async fn set_password(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<User, Error> {
    let username = validate::username(username)?;
    let password = validate::password(password)?;
    let password_hash = hash_password(password)?;
    let mut tx = pool.begin().await.map_err(map_sql)?;
    let row: Option<UserRow> =
        sqlx::query_as("SELECT id, username, role FROM users WHERE username = ?")
            .bind(&username)
            .fetch_optional(&mut *tx)
            .await
            .map_err(map_sql)?;
    let Some(row) = row else {
        return Err(Error::Invalid("нет пользователя с таким именем".into()));
    };
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(password_hash)
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .map_err(map_sql)?;
    sqlx::query("DELETE FROM sessions WHERE user_id = ?")
        .bind(row.id)
        .execute(&mut *tx)
        .await
        .map_err(map_sql)?;
    tx.commit().await.map_err(map_sql)?;
    row.into_user()
}

pub async fn list_users(pool: &SqlitePool) -> Result<Vec<User>, Error> {
    let rows: Vec<UserRow> = sqlx::query_as("SELECT id, username, role FROM users ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(map_sql)?;
    rows.into_iter().map(UserRow::into_user).collect()
}

pub async fn login(
    pool: &SqlitePool,
    username: &str,
    password: &str,
) -> Result<(String, User), Error> {
    let username = username.trim();
    let row: Option<CredentialRow> =
        sqlx::query_as("SELECT id, username, password_hash, role FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(pool)
            .await
            .map_err(map_sql)?;

    let Some(row) = row else {
        let _ = verify_password(password, dummy_hash());
        return Err(Error::InvalidCredentials);
    };
    verify_password(password, &row.password_hash)?;

    sqlx::query("DELETE FROM sessions WHERE expires_at <= ?")
        .bind(now_ms())
        .execute(pool)
        .await
        .map_err(map_sql)?;

    let token = new_token()?;
    let now = now_ms();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
    )
    .bind(token_hash(&token))
    .bind(row.id)
    .bind(now)
    .bind(now + SESSION_TTL_MS)
    .execute(pool)
    .await
    .map_err(map_sql)?;

    Ok((
        token,
        User {
            id: row.id,
            username: row.username,
            role: parse_role(&row.role)?,
        },
    ))
}

pub async fn logout(pool: &SqlitePool, token: &str) -> Result<(), Error> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = ?")
        .bind(token_hash(token))
        .execute(pool)
        .await
        .map_err(map_sql)?;
    Ok(())
}

pub async fn user_for_token(pool: &SqlitePool, token: &str) -> Result<User, Error> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.role
         FROM sessions s
         JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = ? AND s.expires_at > ?",
    )
    .bind(token_hash(token))
    .bind(now_ms())
    .fetch_optional(pool)
    .await
    .map_err(map_sql)?;
    row.ok_or(Error::Unauthenticated)?.into_user()
}

pub async fn list_channels(pool: &SqlitePool) -> Result<Vec<Channel>, Error> {
    let rows: Vec<ChannelRow> =
        sqlx::query_as("SELECT id, name, kind, position FROM channels ORDER BY position, id")
            .fetch_all(pool)
            .await
            .map_err(map_sql)?;
    rows.into_iter().map(ChannelRow::into_channel).collect()
}

pub async fn create_voice_channel(pool: &SqlitePool, name: &str) -> Result<Channel, Error> {
    let name = validate::channel_name(name)?;
    let position: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(position), -1) + 1 FROM channels")
        .fetch_one(pool)
        .await
        .map_err(map_sql)?;
    let result = sqlx::query(
        "INSERT INTO channels (name, kind, position, created_at) VALUES (?, 'voice', ?, ?)",
    )
    .bind(&name)
    .bind(position)
    .bind(now_ms())
    .execute(pool)
    .await
    .map_err(map_sql)?;
    Ok(Channel {
        id: result.last_insert_rowid(),
        name,
        kind: ChannelKind::Voice,
        position,
    })
}

pub async fn rename_channel(
    pool: &SqlitePool,
    channel_id: i64,
    name: &str,
) -> Result<Channel, Error> {
    let name = validate::channel_name(name)?;
    let current = find_channel(pool, channel_id).await?;
    let changed = sqlx::query("UPDATE channels SET name = ? WHERE id = ?")
        .bind(&name)
        .bind(channel_id)
        .execute(pool)
        .await
        .map_err(map_sql)?;
    if changed.rows_affected() == 0 {
        return Err(Error::NotFound);
    }
    Ok(Channel { name, ..current })
}

pub async fn delete_channel(pool: &SqlitePool, channel_id: i64) -> Result<(), Error> {
    let channel = find_channel(pool, channel_id).await?;
    if channel.kind != ChannelKind::Voice {
        return Err(Error::Invalid("текстовый канал нельзя удалить".into()));
    }
    sqlx::query("DELETE FROM channels WHERE id = ?")
        .bind(channel_id)
        .execute(pool)
        .await
        .map_err(map_sql)?;
    Ok(())
}

pub async fn find_channel(pool: &SqlitePool, channel_id: i64) -> Result<Channel, Error> {
    let row: Option<ChannelRow> =
        sqlx::query_as("SELECT id, name, kind, position FROM channels WHERE id = ?")
            .bind(channel_id)
            .fetch_optional(pool)
            .await
            .map_err(map_sql)?;
    row.ok_or(Error::NotFound)?.into_channel()
}

pub async fn insert_message(
    pool: &SqlitePool,
    channel_id: i64,
    author: &User,
    body: &str,
) -> Result<ChatMessage, Error> {
    let channel = find_channel(pool, channel_id).await?;
    if channel.kind != ChannelKind::Text {
        return Err(Error::Invalid("в голосовой канал нельзя писать".into()));
    }
    let body = validate::message_body(body)?;
    let created_at = now_ms();
    let result = sqlx::query(
        "INSERT INTO messages (channel_id, user_id, body, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(channel_id)
    .bind(author.id)
    .bind(&body)
    .bind(created_at)
    .execute(pool)
    .await
    .map_err(map_sql)?;
    Ok(ChatMessage {
        id: result.last_insert_rowid(),
        channel_id,
        user_id: author.id,
        username: author.username.clone(),
        body,
        created_at: format_ms(created_at),
    })
}

pub async fn list_messages(
    pool: &SqlitePool,
    channel_id: i64,
    limit: u32,
    before_id: Option<i64>,
) -> Result<Vec<ChatMessage>, Error> {
    let channel = find_channel(pool, channel_id).await?;
    if channel.kind != ChannelKind::Text {
        return Err(Error::Invalid(
            "у голосовой комнаты нет истории сообщений".into(),
        ));
    }
    let rows: Vec<MessageRow> = sqlx::query_as(
        "SELECT m.id, m.channel_id, m.user_id, u.username, m.body, m.created_at
         FROM messages m
         JOIN users u ON u.id = m.user_id
         WHERE m.channel_id = ?1 AND (?2 IS NULL OR m.id < ?2)
         ORDER BY m.id DESC
         LIMIT ?3",
    )
    .bind(channel_id)
    .bind(before_id)
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await
    .map_err(map_sql)?;
    let mut messages: Vec<_> = rows.into_iter().map(MessageRow::into_message).collect();
    messages.reverse();
    Ok(messages)
}

fn hash_password(password: &str) -> Result<String, Error> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|err| Error::Internal(format!("не удалось захешировать пароль: {err}")))
}

fn verify_password(password: &str, encoded: &str) -> Result<(), Error> {
    let parsed = PasswordHash::new(encoded)
        .map_err(|err| Error::Internal(format!("повреждён хеш пароля: {err}")))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(()),
        Err(argon2::password_hash::Error::PasswordInvalid) => Err(Error::InvalidCredentials),
        Err(err) => Err(Error::Internal(format!("проверка пароля: {err}"))),
    }
}

fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY
        .get_or_init(|| hash_password("not-a-real-account-password").expect("эталонный хеш argon2"))
}

fn new_token() -> Result<String, Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|err| Error::Internal(format!("генератор случайных чисел: {err}")))?;
    Ok(format!("sds_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        encoded.push(char::from(b"0123456789abcdef"[(byte & 0xf) as usize]));
    }
    encoded
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

fn parse_role(role: &str) -> Result<Role, Error> {
    match role {
        "admin" => Ok(Role::Admin),
        "member" => Ok(Role::Member),
        other => Err(Error::Internal(format!("неизвестная роль {other}"))),
    }
}

fn parse_kind(kind: &str) -> Result<ChannelKind, Error> {
    match kind {
        "text" => Ok(ChannelKind::Text),
        "voice" => Ok(ChannelKind::Voice),
        other => Err(Error::Internal(format!("неизвестный тип канала {other}"))),
    }
}

fn map_sql(err: sqlx::Error) -> Error {
    if let Some(db_err) = err.as_database_error() {
        if db_err.is_unique_violation() {
            return Error::Invalid("пользователь с таким именем уже есть".into());
        }
    }
    Error::Internal(err.to_string())
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: i64,
    username: String,
    role: String,
}

impl UserRow {
    fn into_user(self) -> Result<User, Error> {
        Ok(User {
            id: self.id,
            username: self.username,
            role: parse_role(&self.role)?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct CredentialRow {
    id: i64,
    username: String,
    password_hash: String,
    role: String,
}

#[derive(sqlx::FromRow)]
struct ChannelRow {
    id: i64,
    name: String,
    kind: String,
    position: i64,
}

impl ChannelRow {
    fn into_channel(self) -> Result<Channel, Error> {
        Ok(Channel {
            id: self.id,
            name: self.name,
            kind: parse_kind(&self.kind)?,
            position: self.position,
        })
    }
}

#[derive(sqlx::FromRow)]
struct MessageRow {
    id: i64,
    channel_id: i64,
    user_id: i64,
    username: String,
    body: String,
    created_at: i64,
}

impl MessageRow {
    fn into_message(self) -> ChatMessage {
        ChatMessage {
            id: self.id,
            channel_id: self.channel_id,
            user_id: self.user_id,
            username: self.username,
            body: self.body,
            created_at: format_ms(self.created_at),
        }
    }
}
