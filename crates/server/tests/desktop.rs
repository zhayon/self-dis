//! Нативный клиент против живого сервера: чат и голос без микрофона.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use self_dis_client::{Command, Event, MicMode};
use self_dis_protocol::{ChannelKind, ClientMessage, Role, ServerMessage};
use self_dis_server::{router, AppState};

#[test]
fn desktop_client_chats_and_hears() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = std::env::temp_dir().join(format!(
        "self-dis-desktop-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _cleanup = Cleanup(&dir);
    let base = runtime.block_on(async {
        let state = AppState::open(&dir).await.unwrap();
        state
            .create_user("ada", "password123", Role::Admin)
            .await
            .unwrap();
        state
            .create_user("bob", "password123", Role::Member)
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
        runtime.spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        format!("http://{addr}")
    });

    let ada = Client::start();
    let bob = Client::start();
    ada.login(&base, "ada");
    bob.login(&base, "bob");
    let text_id = text_channel(ada.wait_ready());
    bob.wait_ready();

    ada.send(Command::Send(ClientMessage::SendMessage {
        channel_id: text_id,
        body: "  привет  ".into(),
    }));
    bob.wait(|event| {
        matches!(
            event,
            Event::Server(ServerMessage::MessageCreated { message }) if message.body == "привет"
        )
    });

    ada.send(Command::Send(ClientMessage::CreateVoiceChannel {
        name: "болталка".into(),
    }));
    let voice_id = match ada.wait(|event| {
        matches!(
            event,
            Event::Server(ServerMessage::ChannelCreated { channel })
                if channel.kind == ChannelKind::Voice
        )
    }) {
        Event::Server(ServerMessage::ChannelCreated { channel }) => channel.id,
        other => panic!("ожидалась комната, пришло {other:?}"),
    };

    ada.send(Command::JoinVoice {
        channel_id: voice_id,
        mic: MicMode::Tone,
        user_id: 1,
    });
    bob.send(Command::JoinVoice {
        channel_id: voice_id,
        mic: MicMode::Silence,
        user_id: 2,
    });
    bob.wait(|event| matches!(event, Event::RemoteAudio));
}

fn text_channel(event: Event) -> i64 {
    match event {
        Event::Server(ServerMessage::Ready { channels, .. }) => {
            channels
                .iter()
                .find(|channel| channel.kind == ChannelKind::Text)
                .unwrap()
                .id
        }
        other => panic!("ожидался ready, пришло {other:?}"),
    }
}

struct Client {
    commands: mpsc::Sender<Command>,
    events: Receiver<Event>,
}

impl Client {
    fn start() -> Self {
        let (commands, rx) = mpsc::channel();
        let (tx, events) = mpsc::channel();
        thread::spawn(move || self_dis_client::run_session(rx, tx, false));
        Self { commands, events }
    }

    fn login(&self, server: &str, username: &str) {
        self.send(Command::Login {
            server: server.into(),
            username: username.into(),
            password: "password123".into(),
        });
    }

    fn send(&self, command: Command) {
        self.commands.send(command).unwrap();
    }

    fn wait_ready(&self) -> Event {
        self.wait(|event| matches!(event, Event::Server(ServerMessage::Ready { .. })))
    }

    fn wait(&self, mut pred: impl FnMut(&Event) -> bool) -> Event {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(200)) {
                Ok(event) if pred(&event) => return event,
                Ok(event) => seen.push(format!("{event:?}")),
                Err(RecvTimeoutError::Timeout) => {}
                Err(err) => panic!("{err}"),
            }
        }
        panic!("таймаут ожидания события, видел: {seen:?}");
    }
}

struct Cleanup<'a>(&'a std::path::Path);

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}
