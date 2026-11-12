//! Снимок общины. Кадры сервера применяются здесь, окно только рисует.

use std::collections::HashSet;

use self_dis_protocol::{
    Channel, ChannelKind, ChatMessage, Role, ServerMessage, User, VoiceParticipant, VoiceRoom,
    PROTOCOL_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    None,
    /// Оборвать звонок и сказать серверу.
    LeaveVoice,
    /// Сервер уже выкинул из комнаты: локально положить трубку.
    DropVoice,
}

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub me: Option<User>,
    pub channels: Vec<Channel>,
    pub online: Vec<User>,
    pub voice: Vec<VoiceRoom>,
    pub messages: Vec<ChatMessage>,
    pub seen: HashSet<i64>,
    pub text_id: Option<i64>,
    pub voice_channel_id: Option<i64>,
    pub voice_up: bool,
    pub muted: bool,
    pub deaf: bool,
    pub voice_label: String,
}

impl Model {
    pub fn apply(&mut self, message: &ServerMessage) -> Effect {
        match message {
            ServerMessage::Ready {
                protocol,
                user,
                channels,
                online,
                voice,
            } => {
                if *protocol != PROTOCOL_VERSION {
                    return Effect::None;
                }
                self.me = Some(user.clone());
                self.channels = channels.clone();
                self.sort_channels();
                self.online = online.clone();
                self.voice = voice.clone();
                let still_here = self.channels.iter().any(|channel| {
                    Some(channel.id) == self.text_id && channel.kind == ChannelKind::Text
                });
                if !still_here {
                    self.text_id = self
                        .channels
                        .iter()
                        .find(|channel| channel.kind == ChannelKind::Text)
                        .map(|channel| channel.id);
                }
                Effect::None
            }
            ServerMessage::Error { code, .. } => {
                // Ошибка SDP при входе второго человека не должна выкидывать из комнаты.
                // Выходим сразу только если сервер нас туда и не пустил.
                if self.voice_channel_id.is_some()
                    && !self.voice_up
                    && matches!(code.as_str(), "voice_not_ready" | "forbidden" | "not_found")
                {
                    self.clear_voice();
                    Effect::LeaveVoice
                } else {
                    Effect::None
                }
            }
            ServerMessage::MessageCreated { message } => {
                if Some(message.channel_id) == self.text_id {
                    self.push_message(message.clone());
                }
                Effect::None
            }
            ServerMessage::ChannelCreated { channel }
            | ServerMessage::ChannelUpdated { channel } => {
                self.upsert_channel(channel.clone());
                Effect::None
            }
            ServerMessage::ChannelDeleted { channel_id } => {
                self.channels.retain(|channel| channel.id != *channel_id);
                if self.voice_channel_id == Some(*channel_id) {
                    self.clear_voice();
                    return Effect::DropVoice;
                }
                Effect::None
            }
            ServerMessage::UserOnline { user } => {
                if !self.online.iter().any(|item| item.id == user.id) {
                    self.online.push(user.clone());
                }
                Effect::None
            }
            ServerMessage::UserOffline { user_id } => {
                self.online.retain(|user| user.id != *user_id);
                Effect::None
            }
            ServerMessage::VoiceState {
                channel_id,
                participants,
            } => self.apply_voice(*channel_id, participants),
            ServerMessage::RtcOffer { .. }
            | ServerMessage::RtcAnswer { .. }
            | ServerMessage::RtcIce { .. } => Effect::None,
        }
    }

    pub fn load_history(&mut self, incoming: Vec<ChatMessage>) {
        let newest = incoming.last().map(|message| message.id).unwrap_or(0);
        let tail: Vec<ChatMessage> = self
            .messages
            .iter()
            .filter(|message| {
                incoming.iter().all(|item| item.id != message.id) && message.id > newest
            })
            .cloned()
            .collect();
        self.messages.clear();
        self.seen.clear();
        for message in incoming {
            self.push_message(message);
        }
        for message in tail {
            self.push_message(message);
        }
    }

    pub fn push_message(&mut self, message: ChatMessage) -> bool {
        if !self.seen.insert(message.id) {
            return false;
        }
        self.messages.push(message);
        true
    }

    pub fn clear_voice(&mut self) {
        self.voice_channel_id = None;
        self.voice_up = false;
        self.muted = false;
        self.deaf = false;
        self.voice_label.clear();
    }

    pub fn participants(&self, channel_id: i64) -> &[VoiceParticipant] {
        self.voice
            .iter()
            .find(|room| room.channel_id == channel_id)
            .map(|room| room.participants.as_slice())
            .unwrap_or(&[])
    }

    fn apply_voice(&mut self, channel_id: i64, participants: &[VoiceParticipant]) -> Effect {
        self.voice.retain(|room| room.channel_id != channel_id);
        if !participants.is_empty() {
            self.voice.push(VoiceRoom {
                channel_id,
                participants: participants.to_vec(),
            });
        }
        if self.voice_channel_id != Some(channel_id) {
            return Effect::None;
        }
        let Some(me) = self.me.as_ref().map(|user| user.id) else {
            return Effect::None;
        };
        let Some(myself) = participants.iter().find(|person| person.user_id == me) else {
            self.clear_voice();
            return Effect::DropVoice;
        };
        self.muted = myself.muted;
        Effect::None
    }

    fn upsert_channel(&mut self, channel: Channel) {
        if let Some(slot) = self.channels.iter_mut().find(|item| item.id == channel.id) {
            *slot = channel;
        } else {
            self.channels.push(channel);
        }
        self.sort_channels();
    }

    fn sort_channels(&mut self) {
        self.channels.sort_by(|left, right| {
            left.position
                .cmp(&right.position)
                .then(left.id.cmp(&right.id))
        });
    }
}

pub fn role_label(role: Role) -> &'static str {
    match role {
        Role::Admin => "админ",
        Role::Member => "участник",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: i64, body: &str) -> ChatMessage {
        ChatMessage {
            id,
            channel_id: 1,
            user_id: 7,
            username: "ada".into(),
            body: body.into(),
            created_at: "2026-11-12T10:00:00Z".into(),
        }
    }

    #[test]
    fn skips_duplicate_messages_and_keeps_newer_tail() {
        let mut model = Model {
            text_id: Some(1),
            ..Model::default()
        };
        assert!(model.push_message(message(2, "живое")));
        assert!(!model.push_message(message(2, "живое")));
        model.load_history(vec![message(1, "старое")]);
        let bodies: Vec<_> = model
            .messages
            .iter()
            .map(|item| item.body.as_str())
            .collect();
        assert_eq!(bodies, ["старое", "живое"]);
    }

    #[test]
    fn voice_snapshot_replaces_room_and_drops_self() {
        let mut model = Model {
            me: Some(User {
                id: 7,
                username: "ada".into(),
                role: Role::Member,
            }),
            voice_channel_id: Some(3),
            voice_up: true,
            ..Model::default()
        };
        let stay = model.apply(&ServerMessage::VoiceState {
            channel_id: 3,
            participants: vec![VoiceParticipant {
                user_id: 7,
                username: "ada".into(),
                muted: true,
            }],
        });
        assert_eq!(stay, Effect::None);
        assert!(model.muted);
        let gone = model.apply(&ServerMessage::VoiceState {
            channel_id: 3,
            participants: Vec::new(),
        });
        assert_eq!(gone, Effect::DropVoice);
        assert!(model.voice_channel_id.is_none());
    }
}
