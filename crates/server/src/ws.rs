use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use self_dis_protocol::{
    ChannelKind, ClientMessage, ServerMessage, User, PROTOCOL_VERSION, WS_AUTH_TIMEOUT_SECS,
};
use tokio::sync::broadcast;

use crate::db;
use crate::error::Error;
use crate::state::{mark_voice_join, mark_voice_leave, AppState, PresenceGuard};

const MAX_FRAME_BYTES: usize = 256 * 1024;

pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle(socket, state))
}

async fn handle(mut socket: WebSocket, state: AppState) {
    let user = match authenticate(&mut socket, &state).await {
        Ok(user) => user,
        Err(error) => {
            let _ = send_message(&mut socket, &error.to_server_message()).await;
            return;
        }
    };

    let mut events = state.subscribe();
    let signal = state.voice.register_socket();
    let mut signal_rx = signal.rx;
    let joined_voice = Arc::new(AtomicBool::new(false));
    let first = state.join(&user);
    let _guard = PresenceGuard {
        state: state.clone(),
        user_id: user.id,
        signal_id: signal.id,
        joined_voice: Arc::clone(&joined_voice),
    };
    if first {
        state.publish(&ServerMessage::UserOnline { user: user.clone() });
    }

    let channels = match db::list_channels(&state.db).await {
        Ok(channels) => channels,
        Err(error) => {
            let _ = send_message(&mut socket, &error.to_server_message()).await;
            return;
        }
    };
    let ready = ServerMessage::Ready {
        protocol: PROTOCOL_VERSION,
        user: user.clone(),
        channels,
        online: state.online_users(),
        voice: state.voice_snapshot(),
    };
    if send_message(&mut socket, &ready).await.is_err() {
        return;
    }
    tracing::info!(user_id = user.id, username = %user.username, "сокет открыт");

    let why;
    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        if !handle_text(
                            &state,
                            &user,
                            signal.id,
                            &joined_voice,
                            text.as_str(),
                            &mut socket,
                        )
                        .await
                        {
                            why = "не удалось ответить на кадр".into();
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        let error = Error::Invalid("бинарные кадры не поддерживаются".into());
                        if send_message(&mut socket, &error.to_server_message()).await.is_err() {
                            why = "не удалось отправить ошибку".into();
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        why = match frame {
                            Some(frame) => {
                                format!("клиент закрыл: {} {}", frame.code, frame.reason)
                            }
                            None => "клиент закрыл".into(),
                        };
                        break;
                    }
                    None => {
                        why = "поток оборвался".into();
                        break;
                    }
                    Some(Err(error)) => {
                        why = format!("ошибка сокета: {error}");
                        break;
                    }
                }
            }
            event = events.recv() => {
                if !forward_event(&mut socket, event).await {
                    why = "не удалось переслать событие".into();
                    break;
                }
            }
            signal = signal_rx.recv() => {
                match signal {
                    Some(message) => {
                        if send_message(&mut socket, &message).await.is_err() {
                            why = "не удалось отправить сигнал голоса".into();
                            break;
                        }
                    }
                    None => {
                        why = "канал голоса закрыт".into();
                        break;
                    }
                }
            }
        }
    }
    close_socket(&user, &why);
}

fn close_socket(user: &User, reason: &str) {
    tracing::info!(user_id = user.id, username = %user.username, reason, "сокет закрыт");
}

async fn authenticate(socket: &mut WebSocket, state: &AppState) -> Result<User, Error> {
    let frame = tokio::time::timeout(Duration::from_secs(WS_AUTH_TIMEOUT_SECS), socket.recv())
        .await
        .map_err(|_| Error::Unauthenticated)?;

    let text = match frame {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(_)) => return Err(Error::Invalid("ожидался текстовый кадр auth".into())),
        Some(Err(error)) => return Err(Error::Internal(error.to_string())),
        None => return Err(Error::Unauthenticated),
    };
    if text.as_str().len() > MAX_FRAME_BYTES {
        return Err(Error::Invalid("слишком большой кадр".into()));
    }
    let message: ClientMessage = serde_json::from_str(text.as_str())
        .map_err(|_| Error::Invalid("некорректный json".into()))?;
    let ClientMessage::Auth { token } = message else {
        return Err(Error::Unauthenticated);
    };
    db::user_for_token(&state.db, &token).await
}

/// `false` means the socket should close.
async fn handle_text(
    state: &AppState,
    user: &User,
    signal_id: u64,
    joined_voice: &AtomicBool,
    text: &str,
    socket: &mut WebSocket,
) -> bool {
    if text.len() > MAX_FRAME_BYTES {
        return reply(socket, &Error::Invalid("слишком большой кадр".into())).await;
    }
    let message: ClientMessage = match serde_json::from_str(text) {
        Ok(message) => message,
        Err(_) => return reply(socket, &Error::Invalid("некорректный json".into())).await,
    };
    let result = dispatch(state, user, signal_id, joined_voice, message).await;
    match result {
        Ok(Some(event)) => {
            state.publish(&event);
            true
        }
        Ok(None) => true,
        Err(error) => reply(socket, &error).await,
    }
}

async fn dispatch(
    state: &AppState,
    user: &User,
    signal_id: u64,
    joined_voice: &AtomicBool,
    message: ClientMessage,
) -> Result<Option<ServerMessage>, Error> {
    match message {
        ClientMessage::Auth { .. } => Err(Error::Invalid("соединение уже авторизовано".into())),
        ClientMessage::SendMessage { channel_id, body } => {
            let message = db::insert_message(&state.db, channel_id, user, &body).await?;
            Ok(Some(ServerMessage::MessageCreated { message }))
        }
        ClientMessage::CreateVoiceChannel { name } => {
            ensure_admin(user)?;
            let channel = db::create_voice_channel(&state.db, &name).await?;
            Ok(Some(ServerMessage::ChannelCreated { channel }))
        }
        ClientMessage::RenameChannel { channel_id, name } => {
            ensure_admin(user)?;
            let channel = db::rename_channel(&state.db, channel_id, &name).await?;
            Ok(Some(ServerMessage::ChannelUpdated { channel }))
        }
        ClientMessage::DeleteChannel { channel_id } => {
            ensure_admin(user)?;
            db::delete_channel(&state.db, channel_id).await?;
            state.voice.drop_channel(channel_id);
            Ok(Some(ServerMessage::ChannelDeleted { channel_id }))
        }
        ClientMessage::JoinVoice { channel_id } => {
            let channel = db::find_channel(&state.db, channel_id).await?;
            if channel.kind != ChannelKind::Voice {
                return Err(Error::Invalid("это не голосовая комната".into()));
            }
            state
                .voice
                .join(signal_id, user.id, &user.username, channel_id)?;
            mark_voice_join(joined_voice);
            Ok(None)
        }
        ClientMessage::LeaveVoice => {
            mark_voice_leave(joined_voice);
            state.voice.leave(user.id);
            Ok(None)
        }
        ClientMessage::SetMute { muted } => {
            state.voice.mute(user.id, muted)?;
            Ok(None)
        }
        ClientMessage::RtcOffer { sdp } => {
            state.voice.offer(user.id, signal_id, sdp)?;
            Ok(None)
        }
        ClientMessage::RtcAnswer { sdp } => {
            state.voice.answer(user.id, sdp)?;
            Ok(None)
        }
        ClientMessage::RtcIce {
            candidate: Some(candidate),
            ..
        } if !candidate.is_empty() => {
            state.voice.ice(user.id, candidate)?;
            Ok(None)
        }
        ClientMessage::RtcIce { .. } => Ok(None),
    }
}

fn ensure_admin(user: &User) -> Result<(), Error> {
    if user.role == self_dis_protocol::Role::Admin {
        Ok(())
    } else {
        Err(Error::Forbidden)
    }
}

async fn reply(socket: &mut WebSocket, error: &Error) -> bool {
    send_message(socket, &error.to_server_message())
        .await
        .is_ok()
}

async fn forward_event(
    socket: &mut WebSocket,
    event: Result<std::sync::Arc<str>, broadcast::error::RecvError>,
) -> bool {
    match event {
        Ok(payload) => socket
            .send(Message::Text(payload.as_ref().into()))
            .await
            .is_ok(),
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            tracing::warn!(skipped, "клиент отстал от событий");
            let error = Error::Invalid("соединение отстало, подключитесь снова".into());
            let _ = send_message(socket, &error.to_server_message()).await;
            false
        }
        Err(broadcast::error::RecvError::Closed) => false,
    }
}

async fn send_message(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), ()> {
    let text = serde_json::to_string(message).map_err(|err| {
        tracing::error!(%err, "не удалось сериализовать кадр");
    })?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}
