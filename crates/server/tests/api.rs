use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use self_dis_protocol::{
    Channel, ChannelKind, ChatMessage, ClientMessage, LoginRequest, LoginResponse, Role,
    ServerMessage, PROTOCOL_VERSION,
};
use self_dis_server::{router, AppState};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn login_chat_and_voice_rooms() {
    let dir = std::env::temp_dir().join(format!(
        "self-dis-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _cleanup = Cleanup(&dir);
    let state = AppState::open(&dir).await.unwrap();
    state
        .create_user("ada", "password123", Role::Admin)
        .await
        .unwrap();
    state
        .create_user("bob", "password123", Role::Member)
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");

    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    let page = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains("self-dis"));
    let script = client.get(format!("{base}/app.js")).send().await.unwrap();
    assert_eq!(script.status(), 200);
    assert!(script.text().await.unwrap().contains("self-dis-token"));

    let anonymous = client.get(format!("{base}/api/me")).send().await.unwrap();
    assert_eq!(anonymous.status(), 401);

    let wrong = client
        .post(format!("{base}/api/login"))
        .json(&LoginRequest {
            username: "ada".into(),
            password: "not-the-password".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);

    let ada = login(&client, &base, "Ada", "password123").await;
    assert_eq!(ada.user.role, Role::Admin);
    let bob = login(&client, &base, "bob", "password123").await;

    let channels: Vec<Channel> = client
        .get(format!("{base}/api/channels"))
        .bearer_auth(&ada.token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].name, "общий");
    assert_eq!(channels[0].kind, ChannelKind::Text);
    let text_id = channels[0].id;

    let mut ada_ws = connect(addr, &ada.token).await;
    match recv(&mut ada_ws).await {
        ServerMessage::Ready {
            protocol,
            user,
            online,
            voice,
            ..
        } => {
            assert_eq!(protocol, PROTOCOL_VERSION);
            assert_eq!(user.username, "ada");
            assert_eq!(online.len(), 1);
            assert!(voice.is_empty());
        }
        other => panic!("ожидался ready, пришло {other:?}"),
    }

    let mut bob_ws = connect(addr, &bob.token).await;
    match recv(&mut bob_ws).await {
        ServerMessage::Ready { online, .. } => assert_eq!(online.len(), 2),
        other => panic!("ожидался ready, пришло {other:?}"),
    }

    send(
        &mut bob_ws,
        &ClientMessage::SendMessage {
            channel_id: text_id,
            body: "  привет  ".into(),
        },
    )
    .await;
    match until_ada_message(&mut ada_ws).await {
        ServerMessage::MessageCreated { message } => {
            assert_eq!(message.body, "привет");
            assert_eq!(message.username, "bob");
        }
        other => panic!("ожидалось сообщение, пришло {other:?}"),
    }

    let history: Vec<ChatMessage> = client
        .get(format!("{base}/api/channels/{text_id}/messages"))
        .bearer_auth(&bob.token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].body, "привет");

    send(
        &mut bob_ws,
        &ClientMessage::CreateVoiceChannel {
            name: "болталка".into(),
        },
    )
    .await;
    match until(&mut bob_ws, is_error).await {
        ServerMessage::Error { code, .. } => assert_eq!(code, "forbidden"),
        other => panic!("ожидался отказ, пришло {other:?}"),
    }

    send(
        &mut ada_ws,
        &ClientMessage::CreateVoiceChannel {
            name: "болталка".into(),
        },
    )
    .await;
    let voice_id = match until(&mut bob_ws, is_channel_created).await {
        ServerMessage::ChannelCreated { channel } => {
            assert_eq!(channel.kind, ChannelKind::Voice);
            assert_eq!(channel.name, "болталка");
            channel.id
        }
        other => panic!("ожидалось создание комнаты, пришло {other:?}"),
    };

    send(
        &mut ada_ws,
        &ClientMessage::JoinVoice {
            channel_id: voice_id,
        },
    )
    .await;
    match until(&mut ada_ws, is_error).await {
        ServerMessage::Error { code, .. } => assert_eq!(code, "voice_not_ready"),
        other => panic!("ожидался voice_not_ready, пришло {other:?}"),
    }

    send(
        &mut ada_ws,
        &ClientMessage::JoinVoice {
            channel_id: text_id,
        },
    )
    .await;
    match until(&mut ada_ws, is_error).await {
        ServerMessage::Error { code, .. } => assert_eq!(code, "invalid"),
        other => panic!("ожидался отказ войти в текстовый канал, пришло {other:?}"),
    }

    send(
        &mut ada_ws,
        &ClientMessage::DeleteChannel {
            channel_id: text_id,
        },
    )
    .await;
    match until(&mut ada_ws, is_error).await {
        ServerMessage::Error { code, .. } => assert_eq!(code, "invalid"),
        other => panic!("ожидался отказ удалить чат, пришло {other:?}"),
    }
}

#[tokio::test]
async fn voice_join_updates_roster() {
    let dir = std::env::temp_dir().join(format!(
        "self-dis-voice-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _cleanup = Cleanup(&dir);
    let state = AppState::open(&dir).await.unwrap();
    state
        .create_user("ada", "password123", Role::Admin)
        .await
        .unwrap();
    state
        .start_voice(
            "127.0.0.1:0".parse().unwrap(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });

    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let ada = login(&client, &base, "ada", "password123").await;
    let mut socket = connect(addr, &ada.token).await;
    let text_id = match recv(&mut socket).await {
        ServerMessage::Ready {
            channels, voice, ..
        } => {
            assert!(voice.is_empty());
            channels
                .iter()
                .find(|channel| channel.kind == ChannelKind::Text)
                .unwrap()
                .id
        }
        other => panic!("ожидался ready, пришло {other:?}"),
    };

    send(
        &mut socket,
        &ClientMessage::CreateVoiceChannel {
            name: "болталка".into(),
        },
    )
    .await;
    let voice_id = match until(&mut socket, is_channel_created).await {
        ServerMessage::ChannelCreated { channel } => channel.id,
        other => panic!("ожидалось создание комнаты, пришло {other:?}"),
    };

    send(
        &mut socket,
        &ClientMessage::JoinVoice {
            channel_id: text_id,
        },
    )
    .await;
    match until(&mut socket, is_error).await {
        ServerMessage::Error { code, .. } => assert_eq!(code, "invalid"),
        other => panic!("ожидался отказ, пришло {other:?}"),
    }

    send(
        &mut socket,
        &ClientMessage::JoinVoice {
            channel_id: voice_id,
        },
    )
    .await;
    match until(&mut socket, is_voice_state).await {
        ServerMessage::VoiceState {
            channel_id,
            participants,
        } => {
            assert_eq!(channel_id, voice_id);
            assert_eq!(participants.len(), 1);
            assert_eq!(participants[0].user_id, ada.user.id);
            assert_eq!(participants[0].username, "ada");
            assert!(!participants[0].muted);
        }
        other => panic!("ожидался состав комнаты, пришло {other:?}"),
    }

    send(&mut socket, &ClientMessage::SetMute { muted: true }).await;
    match until(&mut socket, is_voice_state).await {
        ServerMessage::VoiceState { participants, .. } => {
            assert!(participants[0].muted);
        }
        other => panic!("ожидался состав комнаты, пришло {other:?}"),
    }

    send(&mut socket, &ClientMessage::LeaveVoice).await;
    match until(&mut socket, is_voice_state).await {
        ServerMessage::VoiceState { participants, .. } => {
            assert!(participants.is_empty());
        }
        other => panic!("ожидалась пустая комната, пришло {other:?}"),
    }
}

async fn login(
    client: &reqwest::Client,
    base: &str,
    username: &str,
    password: &str,
) -> LoginResponse {
    client
        .post(format!("{base}/api/login"))
        .json(&LoginRequest {
            username: username.into(),
            password: password.into(),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn connect(
    addr: std::net::SocketAddr,
    token: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/api/ws"))
        .await
        .unwrap();
    send(
        &mut socket,
        &ClientMessage::Auth {
            token: token.into(),
        },
    )
    .await;
    socket
}

async fn send(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    message: &ClientMessage,
) {
    let text = serde_json::to_string(message).unwrap();
    socket.send(Message::Text(text.into())).await.unwrap();
}

async fn recv(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> ServerMessage {
    let message = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let frame = socket.next().await.unwrap().unwrap();
            if let Message::Text(text) = frame {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    })
    .await
    .expect("сервер не прислал кадр");
    message
}

async fn until(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    pred: fn(&ServerMessage) -> bool,
) -> ServerMessage {
    loop {
        let message = recv(socket).await;
        if pred(&message) {
            return message;
        }
    }
}

async fn until_ada_message(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> ServerMessage {
    until(socket, |message| {
        matches!(message, ServerMessage::MessageCreated { .. })
    })
    .await
}

fn is_error(message: &ServerMessage) -> bool {
    matches!(message, ServerMessage::Error { .. })
}

fn is_channel_created(message: &ServerMessage) -> bool {
    matches!(message, ServerMessage::ChannelCreated { .. })
}

fn is_voice_state(message: &ServerMessage) -> bool {
    matches!(message, ServerMessage::VoiceState { .. })
}

struct Cleanup<'a>(&'a std::path::Path);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}
