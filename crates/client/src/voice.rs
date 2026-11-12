//! Одно WebRTC-соединение с сервером. Клиент не ICE-lite: первый SDP — наш offer.
//! После каждой правки `Rtc` вывод вычитывается до `Timeout`.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use self_dis_protocol::ClientMessage;
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};

use crate::audio::{self, Capture, Codec as Opus, Playback, FRAME};
use crate::stun;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicMode {
    Device,
    Silence,
    Tone,
}

pub enum VoiceCommand {
    Answer(String),
    Offer(String),
    Ice(String),
    Mute(bool),
    Deafen(bool),
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceEvent {
    Signal(ClientMessage),
    Status(String),
    Toast(String),
    Failed(String),
    RemoteAudio,
    /// Уровень 0…1000. Порог «говорит» — 45, как на странице.
    Level {
        user_id: i64,
        milli: u16,
    },
}

pub fn start(
    loopback: bool,
    toward: Option<IpAddr>,
    mic: MicMode,
    user_id: i64,
    events: Sender<VoiceEvent>,
) -> Sender<VoiceCommand> {
    let (commands_tx, commands_rx) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("self-dis-voice".into())
        .spawn(move || {
            if let Err(message) = run(commands_rx, &events, loopback, toward, mic, user_id) {
                let _ = events.send(VoiceEvent::Failed(message));
            }
        });
    commands_tx
}

fn run(
    commands: Receiver<VoiceCommand>,
    events: &Sender<VoiceEvent>,
    loopback: bool,
    toward: Option<IpAddr>,
    mic: MicMode,
    user_id: i64,
) -> Result<(), String> {
    static CRYPTO: std::sync::Once = std::sync::Once::new();
    CRYPTO.call_once(|| {
        str0m::crypto::from_feature_flags().install_process_default();
    });
    send_status(events, "запрашиваю микрофон");
    let capture = match mic {
        MicMode::Device => Capture::device()?,
        MicMode::Silence => Capture::synth(false),
        MicMode::Tone => Capture::synth(true),
    };
    let playback = if mic == MicMode::Device {
        match Playback::open() {
            Ok(playback) => Some(playback),
            Err(message) => {
                let _ = events.send(VoiceEvent::Toast(message));
                None
            }
        }
    } else {
        None
    };
    let mut call = Call::open(loopback, toward, capture, playback, user_id, events)?;
    let mut buf = vec![0u8; 2000];
    loop {
        let timeout = call.drain(events)?;
        match commands.try_recv() {
            Ok(VoiceCommand::Stop) => return Ok(()),
            Ok(command) => {
                call.apply(command, events)?;
                continue;
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
        }
        if call.answered {
            if let Some(line) = call.remote_ice.pop_front() {
                call.add_ice(&line);
                continue;
            }
        } else if call.offer_at.elapsed() > Duration::from_secs(8) {
            return Err("сервер не ответил на голос".into());
        }
        if let Some(mut frame) = call.capture.next_frame() {
            let level = if call.muted {
                0.0
            } else {
                audio::level(&frame)
            };
            call.note_level(call.self_id, level, events);
            if call.connected && !call.muted {
                if call.capture.is_device() {
                    audio::clean_mic(&mut frame);
                }
                call.write_frame(&frame);
                continue;
            }
        }
        let wait = timeout
            .saturating_duration_since(Instant::now())
            .clamp(Duration::from_millis(1), Duration::from_millis(20));
        let _ = call.socket.set_read_timeout(Some(wait));
        if let Some(input) = read_input(&call.socket, call.host, &mut buf) {
            call.rtc.handle_input(input).map_err(voice_failure)?;
        } else {
            call.rtc
                .handle_input(Input::Timeout(Instant::now()))
                .map_err(voice_failure)?;
        }
    }
}

struct Call {
    socket: UdpSocket,
    host: SocketAddr,
    rtc: Rtc,
    local_mid: Mid,
    pending: Option<SdpPendingOffer>,
    remote_ice: VecDeque<String>,
    answered: bool,
    connected: bool,
    muted: bool,
    offer_at: Instant,
    capture: Capture,
    playback: Option<Playback>,
    codec: Opus,
    opus_pt: Option<str0m::media::Pt>,
    timestamp: u64,
    queues: HashMap<Mid, std::sync::Arc<Mutex<VecDeque<i16>>>>,
    heard: bool,
    label: String,
    self_id: i64,
    reported: HashMap<i64, (bool, u16)>,
}

impl Call {
    fn open(
        loopback: bool,
        toward: Option<IpAddr>,
        capture: Capture,
        playback: Option<Playback>,
        user_id: i64,
        events: &Sender<VoiceEvent>,
    ) -> Result<Self, String> {
        let bind = if loopback { "127.0.0.1:0" } else { "0.0.0.0:0" };
        let socket = UdpSocket::bind(bind).map_err(|_| "не удалось открыть голос".to_string())?;
        let port = socket
            .local_addr()
            .map_err(|_| "не удалось открыть голос".to_string())?
            .port();
        let host_ip = if loopback {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        } else {
            toward
                .and_then(route_ip)
                .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
        };
        let host = SocketAddr::new(host_ip, port);
        let mut rtc = Rtc::builder()
            .enable_opus(true, false)
            .build(Instant::now());
        let host_candidate = Candidate::host(host, "udp").map_err(voice_failure)?;
        let mut lines = vec![host_candidate.to_sdp_string()];
        rtc.add_local_candidate(host_candidate);
        drain_rtc(&mut rtc, &socket)?;
        if !loopback {
            if let Some(mapped) = stun::lookup(&socket) {
                if let Ok(candidate) = Candidate::server_reflexive(mapped, host, "udp") {
                    lines.push(candidate.to_sdp_string());
                    rtc.add_local_candidate(candidate);
                    drain_rtc(&mut rtc, &socket)?;
                }
            }
        }
        let mut change = rtc.sdp_api();
        let local_mid = change.add_media(MediaKind::Audio, Direction::SendRecv, None, None, None);
        let (offer, pending) = change
            .apply()
            .ok_or_else(|| "не удалось подключить голос".to_string())?;
        let mut call = Self {
            socket,
            host,
            rtc,
            local_mid,
            pending: Some(pending),
            remote_ice: VecDeque::new(),
            answered: false,
            connected: false,
            muted: false,
            offer_at: Instant::now(),
            capture,
            playback,
            codec: Opus::new()?,
            opus_pt: None,
            timestamp: 0,
            queues: HashMap::new(),
            heard: false,
            label: String::new(),
            self_id: user_id,
            reported: HashMap::new(),
        };
        call.drain(events)?;
        let _ = events.send(VoiceEvent::Signal(ClientMessage::RtcOffer {
            sdp: offer.to_sdp_string(),
        }));
        for line in lines {
            let _ = events.send(VoiceEvent::Signal(ClientMessage::RtcIce {
                candidate: Some(line),
                sdp_mid: None,
                sdp_mline_index: None,
            }));
        }
        send_status(events, "соединяюсь");
        call.label = "соединяюсь".into();
        Ok(call)
    }

    fn drain(&mut self, events: &Sender<VoiceEvent>) -> Result<Instant, String> {
        loop {
            if !self.rtc.is_alive() {
                return Err("не удалось подключить голос".into());
            }
            match self.rtc.poll_output().map_err(voice_failure)? {
                Output::Timeout(timeout) => return Ok(timeout),
                Output::Transmit(transmit) => {
                    let _ = self
                        .socket
                        .send_to(&transmit.contents, transmit.destination);
                }
                Output::Event(event) => self.on_event(event, events),
            }
        }
    }

    fn on_event(&mut self, event: Event, events: &Sender<VoiceEvent>) {
        match event {
            Event::Connected => self.mark_up(events),
            Event::IceConnectionStateChange(state) => match state {
                IceConnectionState::Connected | IceConnectionState::Completed => {
                    self.mark_up(events)
                }
                IceConnectionState::New | IceConnectionState::Checking => {
                    self.set_label(events, "соединяюсь", false)
                }
                IceConnectionState::Disconnected => self.set_label(events, "нет связи", false),
            },
            Event::MediaData(data)
                if data.mid != self.local_mid && data.params.spec().codec == Codec::Opus =>
            {
                match self.codec.decode(&data.data) {
                    Ok(pcm) => self.play(data.mid, pcm, events),
                    Err(err) => tracing::debug!(error = %err, "opus"),
                }
            }
            _ => {}
        }
    }

    fn mark_up(&mut self, events: &Sender<VoiceEvent>) {
        self.set_label(events, "в эфире", true);
    }

    fn set_label(&mut self, events: &Sender<VoiceEvent>, label: &str, up: bool) {
        self.connected = up;
        if self.label == label {
            return;
        }
        self.label = label.to_string();
        send_status(events, label);
    }

    fn note_level(&mut self, user_id: i64, level: f32, events: &Sender<VoiceEvent>) {
        let milli = (level.clamp(0.0, 1.0) * 1000.0) as u16;
        let speaking = milli > 45;
        let bucket = milli / 125;
        if self.reported.get(&user_id) == Some(&(speaking, bucket)) {
            return;
        }
        self.reported.insert(user_id, (speaking, bucket));
        let _ = events.send(VoiceEvent::Level { user_id, milli });
    }

    fn user_of(&self, mid: Mid) -> Option<i64> {
        let stream = self.rtc.media(mid)?.stream_id();
        stream.strip_prefix("user-")?.parse().ok()
    }

    fn play(&mut self, mid: Mid, pcm: Vec<i16>, events: &Sender<VoiceEvent>) {
        if !self.heard && pcm.iter().any(|sample| sample.abs() > 1500) {
            self.heard = true;
            let _ = events.send(VoiceEvent::RemoteAudio);
        }
        if let Some(user_id) = self.user_of(mid) {
            self.note_level(user_id, audio::level(&pcm), events);
        }
        let queue = self.queues.entry(mid).or_insert_with(|| {
            self.playback
                .as_ref()
                .map(Playback::add_queue)
                .unwrap_or_else(|| std::sync::Arc::new(Mutex::new(VecDeque::new())))
        });
        audio::push_playback(queue, pcm);
    }

    fn apply(&mut self, command: VoiceCommand, events: &Sender<VoiceEvent>) -> Result<(), String> {
        match command {
            VoiceCommand::Stop => Ok(()),
            VoiceCommand::Mute(muted) => {
                self.muted = muted;
                if muted {
                    self.note_level(self.self_id, 0.0, events);
                }
                Ok(())
            }
            VoiceCommand::Deafen(deaf) => {
                if let Some(playback) = &self.playback {
                    playback.set_deaf(deaf);
                }
                Ok(())
            }
            VoiceCommand::Answer(sdp) => self.accept_answer(&sdp),
            VoiceCommand::Offer(sdp) => self.accept_offer(&sdp, events),
            VoiceCommand::Ice(line) => {
                if self.answered {
                    self.add_ice(&line);
                } else if !line.is_empty() {
                    self.remote_ice.push_back(line);
                }
                Ok(())
            }
        }
    }

    fn accept_answer(&mut self, sdp: &str) -> Result<(), String> {
        let pending = self
            .pending
            .take()
            .ok_or_else(|| "не удалось подключить голос".to_string())?;
        let answer = SdpAnswer::from_sdp_string(sdp).map_err(|err| {
            tracing::info!(error = %err, "голос");
            "не удалось подключить голос".to_string()
        })?;
        self.rtc
            .sdp_api()
            .accept_answer(pending, answer)
            .map_err(voice_failure)?;
        self.answered = true;
        Ok(())
    }

    fn accept_offer(&mut self, sdp: &str, events: &Sender<VoiceEvent>) -> Result<(), String> {
        if self.pending.is_some() {
            return Err("не удалось подключить голос".into());
        }
        let offer = SdpOffer::from_sdp_string(sdp).map_err(voice_failure)?;
        let answer = self
            .rtc
            .sdp_api()
            .accept_offer(offer)
            .map_err(voice_failure)?;
        let _ = events.send(VoiceEvent::Signal(ClientMessage::RtcAnswer {
            sdp: answer.to_sdp_string(),
        }));
        Ok(())
    }

    fn add_ice(&mut self, line: &str) {
        match Candidate::from_sdp_string(line.trim()) {
            Ok(candidate) => {
                self.rtc.add_remote_candidate(candidate);
            }
            Err(err) => tracing::debug!(error = %err, "пропущен ice-кандидат"),
        }
    }

    fn write_frame(&mut self, frame: &[i16; FRAME]) {
        if self.opus_pt.is_none() {
            self.opus_pt = self.rtc.writer(self.local_mid).and_then(|writer| {
                writer
                    .payload_params()
                    .find(|params| params.spec().codec == Codec::Opus)
                    .map(|params| params.pt())
            });
        }
        let Some(pt) = self.opus_pt else {
            return;
        };
        let packet = match self.codec.encode(frame) {
            Ok(packet) => packet,
            Err(err) => {
                tracing::debug!(error = %err, "opus");
                return;
            }
        };
        let time = MediaTime::new(self.timestamp, Frequency::FORTY_EIGHT_KHZ);
        let Some(writer) = self.rtc.writer(self.local_mid) else {
            return;
        };
        if writer.write(pt, Instant::now(), time, packet).is_ok() {
            self.timestamp += FRAME as u64;
        }
    }
}

fn read_input<'a>(socket: &UdpSocket, host: SocketAddr, buf: &'a mut Vec<u8>) -> Option<Input<'a>> {
    buf.resize(2000, 0);
    let (size, source) = match socket.recv_from(buf) {
        Ok(packet) => packet,
        Err(err)
            if err.kind() == std::io::ErrorKind::WouldBlock
                || err.kind() == std::io::ErrorKind::TimedOut =>
        {
            return None;
        }
        Err(err) => {
            tracing::debug!(error = %err, "udp");
            return None;
        }
    };
    buf.truncate(size);
    let Ok(contents) = buf.as_slice().try_into() else {
        return None;
    };
    let mut destination = socket.local_addr().ok()?;
    if destination.ip().is_unspecified() {
        destination.set_ip(host.ip());
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

fn route_ip(toward: IpAddr) -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((toward, 9)).ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_multicast()).then_some(ip)
}

fn drain_rtc(rtc: &mut Rtc, socket: &UdpSocket) -> Result<(), String> {
    loop {
        if !rtc.is_alive() {
            return Err("не удалось подключить голос".into());
        }
        match rtc.poll_output().map_err(voice_failure)? {
            Output::Timeout(_) => return Ok(()),
            Output::Transmit(transmit) => {
                let _ = socket.send_to(&transmit.contents, transmit.destination);
            }
            Output::Event(_) => {}
        }
    }
}

fn send_status(events: &Sender<VoiceEvent>, label: &str) {
    let _ = events.send(VoiceEvent::Status(label.to_string()));
}

fn voice_failure(err: impl std::fmt::Display) -> String {
    tracing::info!(error = %err, "голос");
    "не удалось подключить голос".into()
}
