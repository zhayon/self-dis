//! Голосовой узел: один UDP-сокет и по одному WebRTC-соединению на участника.
//! Звук не декодируется, пакет Opus пересылается остальным в той же комнате.
//!
//! Поток в отдельном потоке, потому что `str0m` сам не ждёт сокет. После каждой
//! правки соединения цикл вычитывает его вывод до таймаута и только потом
//! трогает его снова.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use self_dis_protocol::{ServerMessage, VoiceParticipant, VoiceRoom};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::media::{Direction, MediaData, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::sync::mpsc::UnboundedSender;

use crate::error::Error;
use crate::state::AppState;

const READ_SLICE: Duration = Duration::from_millis(20);
/// Краткий ICE `Disconnected` бывает при добавлении второго участника.
/// Рвать комнату сразу нельзя: соединение часто само возвращается.
const ICE_GRACE: Duration = Duration::from_secs(15);

enum Cmd {
    Join {
        signal_id: u64,
        user_id: i64,
        username: String,
        channel_id: i64,
    },
    Leave {
        user_id: i64,
    },
    Mute {
        user_id: i64,
        muted: bool,
    },
    Offer {
        user_id: i64,
        signal_id: u64,
        sdp: String,
    },
    Answer {
        user_id: i64,
        sdp: String,
    },
    Ice {
        user_id: i64,
        candidate: String,
    },
    DropChannel {
        channel_id: i64,
    },
}

enum VoiceOut {
    Broadcast(ServerMessage),
    Signal {
        signal_id: u64,
        message: ServerMessage,
    },
}

struct Signal {
    tx: UnboundedSender<ServerMessage>,
}

struct VoiceInner {
    cmd: Mutex<Option<Sender<Cmd>>>,
    signals: Mutex<HashMap<u64, Signal>>,
    roster: Mutex<Vec<VoiceRoom>>,
}

#[derive(Clone)]
pub(crate) struct Voice {
    inner: Arc<VoiceInner>,
}

pub(crate) struct SocketSignal {
    pub id: u64,
    pub rx: tokio::sync::mpsc::UnboundedReceiver<ServerMessage>,
}

impl Voice {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(VoiceInner {
                cmd: Mutex::new(None),
                signals: Mutex::new(HashMap::new()),
                roster: Mutex::new(Vec::new()),
            }),
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<VoiceRoom> {
        self.inner.roster.lock().expect("roster").clone()
    }

    pub(crate) fn register_socket(&self) -> SocketSignal {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.inner
            .signals
            .lock()
            .expect("signals")
            .insert(id, Signal { tx });
        SocketSignal { id, rx }
    }

    pub(crate) fn unregister_socket(&self, signal_id: u64) {
        self.inner
            .signals
            .lock()
            .expect("signals")
            .remove(&signal_id);
    }

    pub fn start(
        &self,
        bind: SocketAddr,
        advertise_ip: IpAddr,
        app: AppState,
    ) -> Result<SocketAddr, Error> {
        let mut cmd_slot = self.inner.cmd.lock().expect("voice cmd");
        if cmd_slot.is_some() {
            return Err(Error::Internal("голос уже запущен".into()));
        }
        str0m::crypto::from_feature_flags().install_process_default();
        let socket = UdpSocket::bind(bind)
            .map_err(|err| Error::Internal(format!("не удалось занять udp {bind}: {err}")))?;
        let bound = socket
            .local_addr()
            .map_err(|err| Error::Internal(err.to_string()))?;
        let advertise = SocketAddr::new(advertise_ip, bound.port());
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let signals = Arc::clone(&self.inner);
        let thread_roster = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name("voice".into())
            .spawn(move || {
                if let Err(err) = run(socket, advertise, cmd_rx, event_tx, thread_roster) {
                    tracing::error!(error = %err, "голосовой поток остановился");
                }
            })
            .map_err(|err| Error::Internal(format!("не удалось запустить голос: {err}")))?;
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                deliver(event, &signals, &app);
            }
        });
        *cmd_slot = Some(cmd_tx);
        Ok(advertise)
    }

    pub(crate) fn join(
        &self,
        signal_id: u64,
        user_id: i64,
        username: &str,
        channel_id: i64,
    ) -> Result<(), Error> {
        self.send(Cmd::Join {
            signal_id,
            user_id,
            username: username.to_owned(),
            channel_id,
        })
    }

    pub(crate) fn leave(&self, user_id: i64) {
        let _ = self.send(Cmd::Leave { user_id });
    }

    pub(crate) fn mute(&self, user_id: i64, muted: bool) -> Result<(), Error> {
        self.send(Cmd::Mute { user_id, muted })
    }

    pub(crate) fn offer(&self, user_id: i64, signal_id: u64, sdp: String) -> Result<(), Error> {
        self.send(Cmd::Offer {
            user_id,
            signal_id,
            sdp,
        })
    }

    pub(crate) fn answer(&self, user_id: i64, sdp: String) -> Result<(), Error> {
        self.send(Cmd::Answer { user_id, sdp })
    }

    pub(crate) fn ice(&self, user_id: i64, candidate: String) -> Result<(), Error> {
        self.send(Cmd::Ice { user_id, candidate })
    }

    pub(crate) fn drop_channel(&self, channel_id: i64) {
        let _ = self.send(Cmd::DropChannel { channel_id });
    }

    fn send(&self, cmd: Cmd) -> Result<(), Error> {
        let slot = self.inner.cmd.lock().expect("voice cmd");
        let Some(tx) = slot.as_ref() else {
            return Err(Error::VoiceNotReady);
        };
        tx.send(cmd)
            .map_err(|_| Error::Internal("голосовой поток недоступен".into()))
    }
}

fn deliver(event: VoiceOut, voice: &VoiceInner, app: &AppState) {
    match event {
        VoiceOut::Broadcast(message) => app.publish(&message),
        VoiceOut::Signal { signal_id, message } => {
            let signals = voice.signals.lock().expect("signals");
            if let Some(signal) = signals.get(&signal_id) {
                let _ = signal.tx.send(message);
            }
        }
    }
}

fn run(
    socket: UdpSocket,
    advertise: SocketAddr,
    cmd_rx: Receiver<Cmd>,
    event_tx: UnboundedSender<VoiceOut>,
    voice: Arc<VoiceInner>,
) -> Result<(), String> {
    let mut clients: Vec<Client> = Vec::new();
    let mut queue: VecDeque<Propagated> = VecDeque::new();
    let mut announced: HashSet<i64> = HashSet::new();
    let mut buf = vec![0; 2000];

    loop {
        let before = clients.len();
        clients.retain(|client| client.rtc.as_ref().is_none_or(|rtc| rtc.is_alive()));
        if clients.len() != before {
            sync_roster(&clients, &voice, &event_tx, &mut announced);
        }
        expire_ice(&mut clients);

        let mut timeout = Instant::now() + READ_SLICE;
        for client in &mut clients {
            let next = poll_until_timeout(client, &mut queue, &socket, &event_tx);
            timeout = timeout.min(next);
        }

        if let Some(propagated) = queue.pop_front() {
            propagate(&propagated, &mut clients);
            continue;
        }

        if apply_one_ice(&mut clients) {
            continue;
        }
        match cmd_rx.try_recv() {
            Ok(cmd) => {
                apply_cmd(
                    cmd,
                    &mut clients,
                    advertise,
                    &event_tx,
                    &voice,
                    &mut announced,
                );
                continue;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return Ok(()),
        }

        let wait = timeout
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(1), READ_SLICE);
        if let Err(err) = socket.set_read_timeout(Some(wait)) {
            return Err(err.to_string());
        }
        if let Some(input) = read_socket_input(&socket, advertise, &mut buf) {
            if let Some(client) = clients.iter_mut().find(|client| client.accepts(&input)) {
                client.handle_input(input);
            }
        }
        let now = Instant::now();
        for client in &mut clients {
            client.handle_input(Input::Timeout(now));
        }
    }
}

fn apply_one_ice(clients: &mut [Client]) -> bool {
    for client in clients {
        if client.rtc.is_none() || client.pending_ice.is_empty() {
            continue;
        }
        let line = client.pending_ice.remove(0);
        let candidate = match Candidate::from_sdp_string(line.trim()) {
            Ok(candidate) => candidate,
            Err(err) => {
                tracing::debug!(error = %err, "пропущен ice-кандидат");
                continue;
            }
        };
        if let Some(rtc) = client.rtc.as_mut() {
            rtc.add_remote_candidate(candidate);
            return true;
        }
    }
    false
}

fn apply_cmd(
    cmd: Cmd,
    clients: &mut Vec<Client>,
    advertise: SocketAddr,
    event_tx: &UnboundedSender<VoiceOut>,
    voice: &VoiceInner,
    announced: &mut HashSet<i64>,
) {
    match cmd {
        Cmd::Join {
            signal_id,
            user_id,
            username,
            channel_id,
        } => {
            if let Some(client) = clients.iter_mut().find(|client| client.user_id == user_id) {
                client.channel_id = channel_id;
                client.username = username;
                client.signal_id = signal_id;
                client.muted = false;
                client.rtc = None;
                client.pending = None;
                client.connected = false;
                client.tracks_in.clear();
                client.tracks_out.clear();
                client.pending_ice.clear();
                client.disconnected_at = None;
            } else {
                clients.push(Client::new(signal_id, user_id, username, channel_id));
            }
            sync_roster(clients, voice, event_tx, announced);
        }
        Cmd::Leave { user_id } => {
            clients.retain(|client| client.user_id != user_id);
            sync_roster(clients, voice, event_tx, announced);
        }
        Cmd::DropChannel { channel_id } => {
            clients.retain(|client| client.channel_id != channel_id);
            sync_roster(clients, voice, event_tx, announced);
        }
        Cmd::Mute { user_id, muted } => {
            if let Some(client) = clients.iter_mut().find(|client| client.user_id == user_id) {
                client.muted = muted;
            }
            sync_roster(clients, voice, event_tx, announced);
        }
        Cmd::Offer {
            user_id,
            signal_id,
            sdp,
        } => {
            let answer = clients
                .iter_mut()
                .find(|client| client.user_id == user_id)
                .map(|client| {
                    client.signal_id = signal_id;
                    client.accept_offer(&sdp, advertise)
                });
            match answer {
                Some(Ok(sdp)) => {
                    let _ = event_tx.send(VoiceOut::Signal {
                        signal_id,
                        message: ServerMessage::RtcAnswer { sdp },
                    });
                    adopt_existing_tracks(clients, user_id);
                }
                Some(Err(err)) => signal_error(event_tx, signal_id, &err),
                None => signal_error(event_tx, signal_id, "сначала войдите в голосовую комнату"),
            }
        }
        Cmd::Answer { user_id, sdp } => {
            let Some(client) = clients.iter_mut().find(|client| client.user_id == user_id) else {
                return;
            };
            if let Err(err) = client.accept_answer(&sdp) {
                signal_error(event_tx, client.signal_id, &err);
            }
        }
        Cmd::Ice { user_id, candidate } => {
            if let Some(client) = clients.iter_mut().find(|client| client.user_id == user_id) {
                client.pending_ice.push(candidate);
            }
        }
    }
}

fn adopt_existing_tracks(clients: &mut [Client], user_id: i64) {
    let Some(channel_id) = clients
        .iter()
        .find(|client| client.user_id == user_id)
        .map(|client| client.channel_id)
    else {
        return;
    };
    let existing: Vec<Weak<TrackIn>> = clients
        .iter()
        .filter(|client| client.user_id != user_id && client.channel_id == channel_id)
        .flat_map(|client| {
            client
                .tracks_in
                .iter()
                .map(|track| Arc::downgrade(&track.id))
        })
        .collect();
    if let Some(client) = clients.iter_mut().find(|client| client.user_id == user_id) {
        for track in existing {
            client.tracks_out.push(TrackOut {
                track_in: track,
                state: TrackOutState::ToOpen,
            });
        }
    }
}

fn signal_error(event_tx: &UnboundedSender<VoiceOut>, signal_id: u64, message: &str) {
    let _ = event_tx.send(VoiceOut::Signal {
        signal_id,
        message: ServerMessage::Error {
            code: "invalid".into(),
            message: message.to_owned(),
        },
    });
}

fn sync_roster(
    clients: &[Client],
    voice: &VoiceInner,
    event_tx: &UnboundedSender<VoiceOut>,
    announced: &mut HashSet<i64>,
) {
    let mut grouped: HashMap<i64, Vec<VoiceParticipant>> = HashMap::new();
    for client in clients {
        grouped
            .entry(client.channel_id)
            .or_default()
            .push(VoiceParticipant {
                user_id: client.user_id,
                username: client.username.clone(),
                muted: client.muted,
            });
    }
    let mut rooms: Vec<VoiceRoom> = grouped
        .into_iter()
        .map(|(channel_id, mut participants)| {
            participants.sort_by_key(|participant| participant.user_id);
            VoiceRoom {
                channel_id,
                participants,
            }
        })
        .collect();
    rooms.sort_by_key(|room| room.channel_id);
    let current: HashSet<i64> = rooms.iter().map(|room| room.channel_id).collect();
    *voice.roster.lock().expect("roster") = rooms.clone();
    for room in &rooms {
        let _ = event_tx.send(VoiceOut::Broadcast(ServerMessage::VoiceState {
            channel_id: room.channel_id,
            participants: room.participants.clone(),
        }));
    }
    for channel_id in announced.difference(&current) {
        let _ = event_tx.send(VoiceOut::Broadcast(ServerMessage::VoiceState {
            channel_id: *channel_id,
            participants: Vec::new(),
        }));
    }
    *announced = current;
}

fn expire_ice(clients: &mut [Client]) {
    let now = Instant::now();
    for client in clients {
        let Some(since) = client.disconnected_at else {
            continue;
        };
        if now.duration_since(since) < ICE_GRACE {
            continue;
        }
        if let Some(rtc) = client.rtc.as_mut() {
            if rtc.is_alive() {
                rtc.disconnect();
            }
        }
        client.disconnected_at = None;
        return;
    }
}

fn poll_until_timeout(
    client: &mut Client,
    queue: &mut VecDeque<Propagated>,
    socket: &UdpSocket,
    event_tx: &UnboundedSender<VoiceOut>,
) -> Instant {
    loop {
        let propagated = client.poll_output(socket, event_tx);
        if let Propagated::Timeout(timeout) = propagated {
            return timeout;
        }
        queue.push_back(propagated);
    }
}

fn propagate(propagated: &Propagated, clients: &mut [Client]) {
    let Some(origin) = propagated.origin() else {
        return;
    };
    let Some(origin_client) = clients.iter().find(|client| client.user_id == origin) else {
        return;
    };
    if origin_client.muted && matches!(propagated, Propagated::Media { .. }) {
        return;
    }
    let channel_id = origin_client.channel_id;
    for client in clients.iter_mut() {
        if client.user_id == origin || client.channel_id != channel_id {
            continue;
        }
        match propagated {
            Propagated::TrackOpen(_, track) => client.tracks_out.push(TrackOut {
                track_in: track.clone(),
                state: TrackOutState::ToOpen,
            }),
            Propagated::Media { data, .. } => client.write_media(origin, data),
            Propagated::Noop | Propagated::Timeout(_) => {}
        }
    }
}

fn read_socket_input<'a>(
    socket: &UdpSocket,
    advertise: SocketAddr,
    buf: &'a mut Vec<u8>,
) -> Option<Input<'a>> {
    buf.resize(2000, 0);
    match socket.recv_from(buf) {
        Ok((size, source)) => {
            buf.truncate(size);
            let Ok(contents) = buf.as_slice().try_into() else {
                return None;
            };
            // Сокет на 0.0.0.0 сообщает local_addr как 0.0.0.0. ICE сверяет адрес
            // назначения с объявленным host-кандидатом и иначе молча выбрасывает STUN.
            let mut destination = socket.local_addr().ok()?;
            if destination.ip().is_unspecified() {
                destination.set_ip(advertise.ip());
            }
            Some(Input::Receive(
                Instant::now(),
                Receive {
                    proto: Protocol::Udp,
                    source,
                    destination,
                    contents,
                },
            ))
        }
        Err(err) => match err.kind() {
            ErrorKind::WouldBlock | ErrorKind::TimedOut => None,
            _ => {
                tracing::warn!(error = %err, "ошибка чтения udp");
                None
            }
        },
    }
}

struct Client {
    signal_id: u64,
    user_id: i64,
    username: String,
    channel_id: i64,
    muted: bool,
    connected: bool,
    rtc: Option<Rtc>,
    pending: Option<SdpPendingOffer>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    pending_ice: Vec<String>,
    disconnected_at: Option<Instant>,
}

struct TrackIn {
    origin: i64,
    mid: Mid,
    kind: MediaKind,
}

struct TrackInEntry {
    id: Arc<TrackIn>,
}

struct TrackOut {
    track_in: Weak<TrackIn>,
    state: TrackOutState,
}

#[derive(Clone, Copy)]
enum TrackOutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
    ToStop(Mid),
    NegotiatingStop(Mid),
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            TrackOutState::ToOpen => None,
            TrackOutState::Negotiating(mid)
            | TrackOutState::Open(mid)
            | TrackOutState::ToStop(mid)
            | TrackOutState::NegotiatingStop(mid) => Some(mid),
        }
    }
}

enum Propagated {
    Noop,
    Timeout(Instant),
    TrackOpen(i64, Weak<TrackIn>),
    Media { origin: i64, data: Box<MediaData> },
}

impl Propagated {
    fn origin(&self) -> Option<i64> {
        match self {
            Propagated::TrackOpen(origin, _) | Propagated::Media { origin, .. } => Some(*origin),
            Propagated::Noop | Propagated::Timeout(_) => None,
        }
    }
}

impl Client {
    fn new(signal_id: u64, user_id: i64, username: String, channel_id: i64) -> Self {
        Self {
            signal_id,
            user_id,
            username,
            channel_id,
            muted: false,
            connected: false,
            rtc: None,
            pending: None,
            tracks_in: Vec::new(),
            tracks_out: Vec::new(),
            pending_ice: Vec::new(),
            disconnected_at: None,
        }
    }

    fn accept_offer(&mut self, sdp: &str, advertise: SocketAddr) -> Result<String, String> {
        let offer = SdpOffer::from_sdp_string(sdp).map_err(|err| err.to_string())?;
        let mut rtc = Rtc::builder().set_ice_lite(true).build(Instant::now());
        let candidate = Candidate::host(advertise, "udp").map_err(|err| err.to_string())?;
        rtc.add_local_candidate(candidate);
        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(|err| err.to_string())?;
        self.rtc = Some(rtc);
        self.pending = None;
        self.connected = false;
        self.disconnected_at = None;
        self.tracks_in.clear();
        self.tracks_out.clear();
        Ok(answer.to_sdp_string())
    }

    fn accept_answer(&mut self, sdp: &str) -> Result<(), String> {
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|err| err.to_string())?;
        let Some(pending) = self.pending.take() else {
            return Err("нет ожидаемого ответа sdp".into());
        };
        let rtc = self.rtc.as_mut().ok_or("нет голосового соединения")?;
        rtc.sdp_api()
            .accept_answer(pending, answer)
            .map_err(|err| err.to_string())?;
        for track in &mut self.tracks_out {
            if let TrackOutState::Negotiating(mid) = track.state {
                track.state = TrackOutState::Open(mid);
            }
        }
        self.tracks_out
            .retain(|track| !matches!(track.state, TrackOutState::NegotiatingStop(_)));
        Ok(())
    }

    fn accepts(&self, input: &Input) -> bool {
        self.rtc.as_ref().is_some_and(|rtc| rtc.accepts(input))
    }

    fn handle_input(&mut self, input: Input) {
        let Some(rtc) = self.rtc.as_mut() else {
            return;
        };
        if !rtc.is_alive() {
            return;
        }
        if let Err(err) = rtc.handle_input(input) {
            tracing::info!(user_id = self.user_id, error = %err, "голос отключён");
            rtc.disconnect();
        }
    }

    fn poll_output(
        &mut self,
        socket: &UdpSocket,
        event_tx: &UnboundedSender<VoiceOut>,
    ) -> Propagated {
        if self.rtc.as_ref().is_none_or(|rtc| !rtc.is_alive()) {
            return Propagated::Timeout(Instant::now() + READ_SLICE);
        }
        if self.negotiate(event_tx) {
            return Propagated::Noop;
        }
        match self.rtc.as_mut().unwrap().poll_output() {
            Ok(output) => self.handle_output(output, socket),
            Err(err) => {
                tracing::info!(user_id = self.user_id, error = %err, "ошибка голоса");
                if let Some(rtc) = self.rtc.as_mut() {
                    rtc.disconnect();
                }
                Propagated::Noop
            }
        }
    }

    fn handle_output(&mut self, output: Output, socket: &UdpSocket) -> Propagated {
        match output {
            Output::Transmit(transmit) => {
                if let Err(err) = socket.send_to(&transmit.contents, transmit.destination) {
                    tracing::debug!(error = %err, "не удалось отправить udp");
                }
                Propagated::Noop
            }
            Output::Timeout(timeout) => Propagated::Timeout(timeout),
            Output::Event(event) => match event {
                Event::Connected => {
                    self.connected = true;
                    Propagated::Noop
                }
                Event::IceConnectionStateChange(
                    IceConnectionState::Connected | IceConnectionState::Completed,
                ) => {
                    self.connected = true;
                    self.disconnected_at = None;
                    Propagated::Noop
                }
                Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                    if self.disconnected_at.is_none() {
                        self.disconnected_at = Some(Instant::now());
                    }
                    Propagated::Noop
                }
                Event::MediaAdded(added) if added.kind == MediaKind::Audio => {
                    let track = TrackInEntry {
                        id: Arc::new(TrackIn {
                            origin: self.user_id,
                            mid: added.mid,
                            kind: added.kind,
                        }),
                    };
                    let weak = Arc::downgrade(&track.id);
                    self.tracks_in.push(track);
                    Propagated::TrackOpen(self.user_id, weak)
                }
                Event::MediaData(data) if !self.muted => Propagated::Media {
                    origin: self.user_id,
                    data: Box::new(data),
                },
                _ => Propagated::Noop,
            },
        }
    }

    fn negotiate(&mut self, event_tx: &UnboundedSender<VoiceOut>) -> bool {
        if !self.connected || self.pending.is_some() || self.rtc.is_none() {
            return false;
        }
        for track in &mut self.tracks_out {
            if let TrackOutState::Open(mid) = track.state {
                if track.track_in.upgrade().is_none() {
                    track.state = TrackOutState::ToStop(mid);
                }
            }
        }
        let mut change = self.rtc.as_mut().unwrap().sdp_api();
        for track in &mut self.tracks_out {
            match track.state {
                TrackOutState::ToOpen => {
                    if let Some(track_in) = track.track_in.upgrade() {
                        let mid = change.add_media(
                            track_in.kind,
                            Direction::SendOnly,
                            Some(format!("user-{}", track_in.origin)),
                            None,
                            None,
                        );
                        track.state = TrackOutState::Negotiating(mid);
                    }
                }
                TrackOutState::ToStop(mid) => {
                    change.stop_media(mid);
                    track.state = TrackOutState::NegotiatingStop(mid);
                }
                _ => {}
            }
        }
        if !change.has_changes() {
            return false;
        }
        let Some((offer, pending)) = change.apply() else {
            for track in &mut self.tracks_out {
                match track.state {
                    TrackOutState::Negotiating(_) => track.state = TrackOutState::ToOpen,
                    TrackOutState::NegotiatingStop(mid) => track.state = TrackOutState::ToStop(mid),
                    _ => {}
                }
            }
            return false;
        };
        self.pending = Some(pending);
        let _ = event_tx.send(VoiceOut::Signal {
            signal_id: self.signal_id,
            message: ServerMessage::RtcOffer {
                sdp: offer.to_sdp_string(),
            },
        });
        true
    }

    fn write_media(&mut self, origin: i64, data: &MediaData) {
        let Some(mid) = self.tracks_out.iter().find_map(|track| {
            let incoming = track.track_in.upgrade()?;
            if incoming.origin == origin && incoming.mid == data.mid {
                track.mid()
            } else {
                None
            }
        }) else {
            return;
        };
        let Some(rtc) = self.rtc.as_mut() else {
            return;
        };
        let writer = rtc.writer(mid);
        let Some(writer) = writer else {
            return;
        };
        let Some(pt) = writer.match_params(data.params) else {
            return;
        };
        if let Err(err) = writer.write(pt, data.network_time, data.time, Arc::clone(&data.data)) {
            tracing::debug!(user_id = self.user_id, error = %err, "не удалось переслать звук");
        }
    }
}
