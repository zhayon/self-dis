//! Протокол self-dis версии [`PROTOCOL_VERSION`].
//!
//! Транспорт разделён на два канала:
//!
//! - HTTP JSON:
//!   - `POST /api/login` — тело [`LoginRequest`], ответ [`LoginResponse`].
//!   - `POST /api/logout` — заголовок `Authorization: Bearer <token>`, пустой ответ.
//!   - `GET /api/me` — ответ [`User`].
//!   - `GET /api/channels` — ответ `Vec<`[`Channel`]`>`.
//!   - `GET /api/channels/:id/messages?limit&before_id` — история текстового
//!     канала, старые сообщения первыми. `limit` по умолчанию
//!     [`HISTORY_DEFAULT_LIMIT`], потолок [`HISTORY_MAX_LIMIT`]. `before_id`
//!     берёт сообщения старше этого id.
//!   - Ошибка: [`ErrorBody`], код в `error.code`.
//! - `GET /api/ws` — WebSocket с текстовыми JSON-кадрами. Первым кадром клиент
//!   отправляет [`ClientMessage::Auth`]. Пока кадр не принят, сервер закрывает
//!   соединение через [`WS_AUTH_TIMEOUT_SECS`] секунд.
//!
//! Браузерный `WebSocket` не умеет ставить заголовок `Authorization`, поэтому
//! токен передаётся кадром, а не заголовком. HTTP-методы токен принимают
//! только как `Authorization: Bearer <token>`.
//!
//! Идентификаторы — целые числа в безопасном диапазоне JavaScript (SQLite
//! rowid). Время в JSON — RFC 3339 в UTC, например `2026-11-09T10:00:00Z`.
//! Неизвестное поле клиент и сервер игнорируют. Неизвестный `type` — ошибка.
//!
//! Голос идёт отдельным UDP. Клиент поднимает одно `RTCPeerConnection` к серверу.
//! Если голосовой сокет не запущен, сервер отвечает [`ServerMessage::Error`]
//! с кодом `voice_not_ready`.
//!
//! Клиент шлёт [`ClientMessage::JoinVoice`], затем [`ClientMessage::RtcOffer`].
//! Первым SDP-кадром клиент шлёт [`ClientMessage::RtcOffer`]. Сервер отвечает
//! [`ServerMessage::RtcAnswer`]. Обе стороны шлют `rtc_ice`. Чтобы добавить
//! или убрать дорожку, сервер присылает [`ServerMessage::RtcOffer`], а клиент
//! отвечает [`ClientMessage::RtcAnswer`]. Состав комнаты сервер каждый раз
//! присылает целиком в [`ServerMessage::VoiceState`]; клиент подменяет им
//! прошлый список этой комнаты. В [`ServerMessage::Ready`] поле `voice` —
//! снимок всех комнат на момент подключения.

use serde::{Deserialize, Serialize};

/// Версия JSON-протокола в [`ServerMessage::Ready`].
pub const PROTOCOL_VERSION: u32 = 1;

/// Сколько секунд сервер ждёт кадр [`ClientMessage::Auth`].
pub const WS_AUTH_TIMEOUT_SECS: u64 = 10;

pub const USERNAME_MIN_CHARS: usize = 2;
pub const USERNAME_MAX_CHARS: usize = 32;
pub const PASSWORD_MIN_CHARS: usize = 8;
pub const PASSWORD_MAX_CHARS: usize = 128;
pub const CHANNEL_NAME_MAX_CHARS: usize = 64;
pub const MESSAGE_MAX_CHARS: usize = 4000;
pub const HISTORY_DEFAULT_LIMIT: u32 = 50;
pub const HISTORY_MAX_LIMIT: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    Member,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: i64,
    pub name: String,
    pub kind: ChannelKind,
    pub position: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub id: i64,
    pub channel_id: i64,
    pub user_id: i64,
    pub username: String,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceParticipant {
    pub user_id: i64,
    pub username: String,
    pub muted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoiceRoom {
    pub channel_id: i64,
    pub participants: Vec<VoiceParticipant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ErrorDetail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub before_id: Option<i64>,
}

/// Кадр клиента. `type` — тег в snake_case, остальные поля лежат рядом.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Auth {
        token: String,
    },
    SendMessage {
        channel_id: i64,
        body: String,
    },
    CreateVoiceChannel {
        name: String,
    },
    RenameChannel {
        channel_id: i64,
        name: String,
    },
    DeleteChannel {
        channel_id: i64,
    },
    JoinVoice {
        channel_id: i64,
    },
    LeaveVoice,
    SetMute {
        muted: bool,
    },
    RtcOffer {
        sdp: String,
    },
    RtcAnswer {
        sdp: String,
    },
    RtcIce {
        candidate: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sdp_mid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sdp_mline_index: Option<u16>,
    },
}

/// Кадр сервера. Рассылка (`message_created`, каналы, присутствие) приходит
/// и отправителю тоже. Одно сообщение может прийти и в истории HTTP, и живым
/// кадром: повтор `message.id` клиент пропускает. Канал и пользователя с тем
/// же id клиент обновляет.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Ready {
        protocol: u32,
        user: User,
        channels: Vec<Channel>,
        online: Vec<User>,
        voice: Vec<VoiceRoom>,
    },
    Error {
        code: String,
        message: String,
    },
    MessageCreated {
        message: ChatMessage,
    },
    ChannelCreated {
        channel: Channel,
    },
    ChannelUpdated {
        channel: Channel,
    },
    ChannelDeleted {
        channel_id: i64,
    },
    UserOnline {
        user: User,
    },
    UserOffline {
        user_id: i64,
    },
    VoiceState {
        channel_id: i64,
        participants: Vec<VoiceParticipant>,
    },
    RtcOffer {
        sdp: String,
    },
    RtcAnswer {
        sdp: String,
    },
    RtcIce {
        candidate: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sdp_mid: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sdp_mline_index: Option<u16>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_messages_use_snake_case_tags() {
        let message = ClientMessage::SendMessage {
            channel_id: 1,
            body: "привет".into(),
        };
        let json = serde_json::to_value(&message).unwrap();
        assert_eq!(json["type"], "send_message");
        assert_eq!(json["channel_id"], 1);

        let ice = ClientMessage::RtcIce {
            candidate: None,
            sdp_mid: None,
            sdp_mline_index: None,
        };
        let json = serde_json::to_string(&ice).unwrap();
        assert_eq!(json, r#"{"type":"rtc_ice","candidate":null}"#);
    }

    #[test]
    fn ready_roundtrip() {
        let ready = ServerMessage::Ready {
            protocol: PROTOCOL_VERSION,
            user: User {
                id: 1,
                username: "ada".into(),
                role: Role::Admin,
            },
            channels: vec![Channel {
                id: 1,
                name: "общий".into(),
                kind: ChannelKind::Text,
                position: 0,
            }],
            online: vec![],
            voice: vec![],
        };
        let text = serde_json::to_string(&ready).unwrap();
        let parsed: ServerMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, ready);
    }
}
