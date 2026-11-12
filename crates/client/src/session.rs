//! Вход, WebSocket и переподключение. Голос живёт в соседнем потоке.

use std::net::TcpStream;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use self_dis_protocol::{ChannelKind, ClientMessage, ServerMessage, User, PROTOCOL_VERSION};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

use crate::http;
use crate::store;
use crate::voice::{self, MicMode, VoiceCommand, VoiceEvent};

#[derive(Debug, Clone)]
pub enum Command {
    Login {
        server: String,
        username: String,
        password: String,
    },
    Logout,
    Send(ClientMessage),
    OpenText(i64),
    JoinVoice {
        channel_id: i64,
        mic: MicMode,
        user_id: i64,
    },
    LeaveVoice,
    StopVoice,
    SetMute(bool),
    SetDeafen(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Prefill {
        server: String,
    },
    LoggedIn {
        server: String,
        user: User,
    },
    LoggedOut,
    Server(ServerMessage),
    History {
        channel_id: i64,
        messages: Vec<self_dis_protocol::ChatMessage>,
    },
    Toast(String),
    VoiceStatus(String),
    VoiceFailed(String),
    VoiceDropped,
    RemoteAudio,
    Level {
        user_id: i64,
        milli: u16,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Logout,
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Continue,
    Logout,
    Dropped,
}

struct Link {
    commands: Sender<VoiceCommand>,
    events: Receiver<VoiceEvent>,
}

pub fn run(commands: Receiver<Command>, events: Sender<Event>, persist: bool) {
    if persist {
        let server = store::load_server();
        let _ = events.send(Event::Prefill {
            server: server.clone(),
        });
        if let Some(token) = store::load_token(&server) {
            match http::me(&server, &token) {
                Ok(user) => {
                    let _ = events.send(Event::LoggedIn {
                        server: server.clone(),
                        user,
                    });
                    if connected(&server, &token, &commands, &events, persist) == End::Closed {
                        return;
                    }
                    let _ = events.send(Event::LoggedOut);
                }
                Err(_) => store::forget_token(&server),
            }
        }
    }
    while let Ok(command) = commands.recv() {
        let Command::Login {
            server,
            username,
            password,
        } = command
        else {
            continue;
        };
        let server = match http::normalize_server(&server) {
            Ok(server) => server,
            Err(message) => {
                let _ = events.send(Event::Toast(message));
                continue;
            }
        };
        let session = match http::login(&server, &username, &password) {
            Ok(session) => session,
            Err(message) => {
                let _ = events.send(Event::Toast(message));
                continue;
            }
        };
        if persist {
            store::save_server(&server);
            if !store::save_token(&server, &session.token) {
                let _ = events.send(Event::Toast(
                    "не удалось сохранить вход в связке ключей".into(),
                ));
            }
        }
        let _ = events.send(Event::LoggedIn {
            server: server.clone(),
            user: session.user,
        });
        if connected(&server, &session.token, &commands, &events, persist) == End::Closed {
            return;
        }
        let _ = events.send(Event::LoggedOut);
    }
}

fn connected(
    server: &str,
    token: &str,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
    persist: bool,
) -> End {
    loop {
        let url = match http::ws_url(server) {
            Ok(url) => url,
            Err(message) => {
                let _ = events.send(Event::Toast(message));
                return End::Logout;
            }
        };
        match connect(&url) {
            Ok((mut socket, _)) => {
                set_timeout(&mut socket, Some(Duration::from_millis(20)));
                if ws_send(
                    &mut socket,
                    &ClientMessage::Auth {
                        token: token.to_string(),
                    },
                )
                .is_err()
                {
                    let _ = events.send(Event::Toast("не удалось подключиться".into()));
                    if let Some(end) = wait(commands, Duration::from_secs(1)) {
                        return sign_out(end, server, token, persist);
                    }
                    continue;
                }
                match pump(server, token, &mut socket, commands, events) {
                    Step::Continue => {}
                    Step::Logout => return sign_out(End::Logout, server, token, persist),
                    Step::Dropped => {
                        let _ = events.send(Event::VoiceDropped);
                        let _ =
                            events.send(Event::Toast("связь прервалась, подключаюсь снова".into()));
                        if let Some(end) = wait(commands, Duration::from_secs(1)) {
                            return sign_out(end, server, token, persist);
                        }
                    }
                }
            }
            Err(err) => {
                tracing::info!(error = %err, "сокет");
                let _ = events.send(Event::Toast("не удалось подключиться".into()));
                if let Some(end) = wait(commands, Duration::from_secs(1)) {
                    return sign_out(end, server, token, persist);
                }
            }
        }
    }
}

fn pump(
    server: &str,
    token: &str,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
) -> Step {
    let mut voice: Option<Link> = None;
    let mut warned = false;
    loop {
        match commands.try_recv() {
            Ok(command) => match on_command(server, token, socket, &mut voice, command, events) {
                Step::Continue => {}
                other => return other,
            },
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Step::Logout,
        }
        if let Some(step) = poll_voice(socket, &mut voice, events) {
            return step;
        }
        match socket.read() {
            Ok(Message::Text(text)) => match serde_json::from_str::<ServerMessage>(&text) {
                Ok(message) => {
                    if let Some(step) =
                        on_server(server, token, socket, &mut voice, message, events)
                    {
                        return step;
                    }
                }
                Err(_) if !warned => {
                    warned = true;
                    let _ = events.send(Event::Toast("не понял сообщение сервера".into()));
                }
                Err(_) => {}
            },
            Ok(Message::Close(_)) => return Step::Dropped,
            Ok(_) => {}
            Err(tungstenite::Error::Io(err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return Step::Dropped,
        }
    }
}

fn on_command(
    server: &str,
    token: &str,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    voice: &mut Option<Link>,
    command: Command,
    events: &Sender<Event>,
) -> Step {
    match command {
        Command::Logout => {
            stop_voice(socket, voice, true);
            Step::Logout
        }
        Command::Login { .. } => Step::Continue,
        Command::Send(message) => send_or_drop(socket, &message),
        Command::OpenText(channel_id) => match http::history(server, token, channel_id) {
            Ok(messages) => {
                let _ = events.send(Event::History {
                    channel_id,
                    messages,
                });
                Step::Continue
            }
            Err(message) => {
                let _ = events.send(Event::Toast(message));
                Step::Continue
            }
        },
        Command::JoinVoice {
            channel_id,
            mic,
            user_id,
        } => {
            stop_voice(socket, voice, true);
            if send_or_drop(socket, &ClientMessage::JoinVoice { channel_id }) == Step::Dropped {
                return Step::Dropped;
            }
            let (commands, voice_events) = voice_pair(server, mic, user_id);
            *voice = Some(Link {
                commands,
                events: voice_events,
            });
            Step::Continue
        }
        Command::LeaveVoice => {
            stop_voice(socket, voice, true);
            Step::Continue
        }
        Command::StopVoice => {
            stop_voice(socket, voice, false);
            Step::Continue
        }
        Command::SetMute(muted) => {
            if let Some(link) = voice {
                let _ = link.commands.send(VoiceCommand::Mute(muted));
            }
            send_or_drop(socket, &ClientMessage::SetMute { muted })
        }
        Command::SetDeafen(deaf) => {
            if let Some(link) = voice {
                let _ = link.commands.send(VoiceCommand::Deafen(deaf));
            }
            Step::Continue
        }
    }
}

fn on_server(
    server: &str,
    token: &str,
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    voice: &mut Option<Link>,
    message: ServerMessage,
    events: &Sender<Event>,
) -> Option<Step> {
    if let ServerMessage::Error { code, .. } = &message {
        if code == "unauthenticated" {
            stop_voice(socket, voice, false);
            return Some(Step::Logout);
        }
    }
    if let ServerMessage::Ready {
        protocol, channels, ..
    } = &message
    {
        if *protocol != PROTOCOL_VERSION {
            let _ = events.send(Event::Toast(
                "сервер говорит на другой версии протокола".into(),
            ));
            return None;
        }
        let text_id = channels
            .iter()
            .find(|channel| channel.kind == ChannelKind::Text)
            .map(|channel| channel.id);
        let _ = events.send(Event::Server(message));
        if let Some(channel_id) = text_id {
            if let Ok(messages) = http::history(server, token, channel_id) {
                let _ = events.send(Event::History {
                    channel_id,
                    messages,
                });
            }
        }
        return None;
    }
    match &message {
        ServerMessage::RtcAnswer { sdp } => {
            if let Some(link) = voice {
                let _ = link.commands.send(VoiceCommand::Answer(sdp.clone()));
            }
        }
        ServerMessage::RtcOffer { sdp } => {
            if let Some(link) = voice {
                let _ = link.commands.send(VoiceCommand::Offer(sdp.clone()));
            }
        }
        ServerMessage::RtcIce {
            candidate: Some(candidate),
            ..
        } if !candidate.is_empty() => {
            if let Some(link) = voice {
                let _ = link.commands.send(VoiceCommand::Ice(candidate.clone()));
            }
        }
        _ => {}
    }
    let _ = events.send(Event::Server(message));
    None
}

fn poll_voice(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    voice: &mut Option<Link>,
    events: &Sender<Event>,
) -> Option<Step> {
    let mut failed = None;
    if let Some(link) = voice.as_mut() {
        while let Ok(event) = link.events.try_recv() {
            match event {
                VoiceEvent::Signal(message) => {
                    if ws_send(socket, &message).is_err() {
                        return Some(Step::Dropped);
                    }
                }
                VoiceEvent::Status(label) => {
                    let _ = events.send(Event::VoiceStatus(label));
                }
                VoiceEvent::Toast(text) => {
                    let _ = events.send(Event::Toast(text));
                }
                VoiceEvent::Failed(text) => failed = Some(text),
                VoiceEvent::RemoteAudio => {
                    let _ = events.send(Event::RemoteAudio);
                }
                VoiceEvent::Level { user_id, milli } => {
                    let _ = events.send(Event::Level { user_id, milli });
                }
            }
        }
    }
    if let Some(text) = failed {
        stop_voice(socket, voice, true);
        let _ = events.send(Event::VoiceFailed(text));
    }
    None
}

fn voice_pair(
    server: &str,
    mic: MicMode,
    user_id: i64,
) -> (Sender<VoiceCommand>, Receiver<VoiceEvent>) {
    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let commands = voice::start(
        http::is_loopback(server),
        http::server_ip(server),
        mic,
        user_id,
        events_tx,
    );
    (commands, events_rx)
}

fn stop_voice(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    voice: &mut Option<Link>,
    tell_server: bool,
) {
    if let Some(link) = voice.take() {
        let _ = link.commands.send(VoiceCommand::Stop);
    }
    if tell_server {
        let _ = ws_send(socket, &ClientMessage::LeaveVoice);
    }
}

fn send_or_drop(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    message: &ClientMessage,
) -> Step {
    if ws_send(socket, message).is_err() {
        Step::Dropped
    } else {
        Step::Continue
    }
}

fn sign_out(end: End, server: &str, token: &str, persist: bool) -> End {
    if matches!(end, End::Logout) {
        if persist {
            store::forget_token(server);
        }
        http::logout(server, token);
    }
    end
}

fn wait(commands: &Receiver<Command>, pause: Duration) -> Option<End> {
    let deadline = Instant::now() + pause;
    while Instant::now() < deadline {
        match commands.recv_timeout(Duration::from_millis(50)) {
            Ok(Command::Logout) => return Some(End::Logout),
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Some(End::Closed),
        }
    }
    None
}

fn ws_send(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    message: &ClientMessage,
) -> Result<(), ()> {
    let text = serde_json::to_string(message).map_err(|_| ())?;
    socket.send(Message::text(text)).map_err(|_| ())
}

fn set_timeout(socket: &mut WebSocket<MaybeTlsStream<TcpStream>>, timeout: Option<Duration>) {
    let apply = |stream: &TcpStream| {
        let _ = stream.set_read_timeout(timeout);
    };
    match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => apply(stream),
        MaybeTlsStream::Rustls(stream) => apply(stream.get_ref()),
        _ => {}
    }
}
