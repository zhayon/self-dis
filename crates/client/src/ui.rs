//! Окно клиента. Состояние рисуется из [`crate::model::Model`], сеть — в сессии.

use std::collections::HashMap;
use std::sync::mpsc::Sender;

use iced::futures::{SinkExt, StreamExt};
use iced::keyboard::{key, Key};
use iced::widget::{
    button, column, container, row, rule, scrollable, text, text_editor, text_input, tooltip,
    Column,
};
use iced::{
    color, stream, Alignment, Background, Border, Color, Element, Font, Length, Shadow, Size,
    Subscription, Task, Theme,
};
use self_dis_protocol::{Channel, ChannelKind, ClientMessage, Role, ServerMessage};

use crate::icons::{self, Glyph};
use crate::model::{role_label, Effect, Model};
use crate::session::{self, Command, Event};
use crate::voice::MicMode;

const TEXT: Color = color!(0xf3eee4);
const MUTED: Color = color!(0xa39888);
const ACCENT: Color = color!(0xe0b15a);
const ACCENT_INK: Color = color!(0x2a2112);
const OK: Color = color!(0x8fbf8a);
const DANGER: Color = color!(0xe07a68);
const LINE: Color = color!(0x3c352c);
const PANEL_2: Color = color!(0x27231d);

const FONT: Font = Font::with_name("Noto Sans");

pub fn run() -> iced::Result {
    iced::application(App::boot, App::update, App::view)
        .title(|_: &App| "self-dis".to_string())
        .subscription(App::subscription)
        .theme(theme)
        .window_size(Size::new(1080.0, 720.0))
        .default_font(FONT)
        .font(include_bytes!("../fonts/NotoSans-Regular.ttf"))
        .run()
}

struct App {
    commands: Option<Sender<Command>>,
    server: String,
    username: String,
    password: String,
    login_error: String,
    toast: String,
    logged_in: bool,
    model: Model,
    draft: text_editor::Content,
    new_room: String,
    rename: Option<(i64, String)>,
    confirm_delete: Option<i64>,
    levels: HashMap<i64, u16>,
}

#[derive(Debug, Clone)]
enum Message {
    Boot(Sender<Command>),
    Net(Event),
    ServerChanged(String),
    UsernameChanged(String),
    PasswordChanged(String),
    Login,
    Logout,
    Draft(text_editor::Action),
    Send,
    NewRoomChanged(String),
    CreateRoom,
    SelectText(i64),
    JoinVoice(i64),
    LeaveVoice,
    ToggleMute,
    ToggleDeafen,
    StartRename(i64, String),
    RenameChanged(String),
    SubmitRename,
    CancelRename,
    AskDelete(i64),
    ConfirmDelete,
    CancelDelete,
}

impl App {
    fn boot() -> Self {
        Self {
            commands: None,
            server: "http://127.0.0.1:8080".into(),
            username: String::new(),
            password: String::new(),
            login_error: String::new(),
            toast: String::new(),
            logged_in: false,
            model: Model::default(),
            draft: text_editor::Content::new(),
            new_room: String::new(),
            rename: None,
            confirm_delete: None,
            levels: HashMap::new(),
        }
    }

    fn subscription(_app: &Self) -> Subscription<Message> {
        Subscription::run(listen)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Boot(commands) => self.commands = Some(commands),
            Message::Net(event) => self.on_event(event),
            Message::ServerChanged(value) => self.server = value,
            Message::UsernameChanged(value) => self.username = value,
            Message::PasswordChanged(value) => self.password = value,
            Message::Login => {
                self.login_error.clear();
                self.send(Command::Login {
                    server: self.server.clone(),
                    username: self.username.trim().to_string(),
                    password: self.password.clone(),
                });
            }
            Message::Logout => self.send(Command::Logout),
            Message::Draft(action) => self.draft.perform(action),
            Message::Send => self.send_draft(),
            Message::NewRoomChanged(value) => self.new_room = value,
            Message::CreateRoom => {
                let name = self.new_room.trim().to_string();
                if !name.is_empty() {
                    self.send(Command::Send(ClientMessage::CreateVoiceChannel { name }));
                    self.new_room.clear();
                }
            }
            Message::SelectText(id) => {
                if self.model.text_id != Some(id) {
                    self.model.text_id = Some(id);
                    self.model.messages.clear();
                    self.model.seen.clear();
                    self.send(Command::OpenText(id));
                }
            }
            Message::JoinVoice(id) => {
                if self.model.voice_channel_id != Some(id) {
                    if let Some(user_id) = self.model.me.as_ref().map(|user| user.id) {
                        self.model.voice_channel_id = Some(id);
                        self.model.voice_up = false;
                        self.model.muted = false;
                        self.model.deaf = false;
                        self.model.voice_label = "запрашиваю микрофон".into();
                        self.levels.clear();
                        self.send(Command::JoinVoice {
                            channel_id: id,
                            mic: MicMode::Device,
                            user_id,
                        });
                    }
                }
            }
            Message::LeaveVoice => {
                self.send(Command::LeaveVoice);
                self.model.clear_voice();
                self.levels.clear();
            }
            Message::ToggleMute => {
                self.model.muted = !self.model.muted;
                self.send(Command::SetMute(self.model.muted));
            }
            Message::ToggleDeafen => {
                self.model.deaf = !self.model.deaf;
                self.send(Command::SetDeafen(self.model.deaf));
            }
            Message::StartRename(id, name) => self.rename = Some((id, name)),
            Message::RenameChanged(value) => {
                if let Some((_, name)) = &mut self.rename {
                    *name = value;
                }
            }
            Message::SubmitRename => {
                if let Some((id, name)) = self.rename.take() {
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        self.send(Command::Send(ClientMessage::RenameChannel {
                            channel_id: id,
                            name,
                        }));
                    }
                }
            }
            Message::CancelRename => self.rename = None,
            Message::AskDelete(id) => self.confirm_delete = Some(id),
            Message::ConfirmDelete => {
                if let Some(id) = self.confirm_delete.take() {
                    self.send(Command::Send(ClientMessage::DeleteChannel {
                        channel_id: id,
                    }));
                }
            }
            Message::CancelDelete => self.confirm_delete = None,
        }
        Task::none()
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Prefill { server } => {
                if !self.logged_in {
                    self.server = server;
                }
            }
            Event::LoggedIn { server, .. } => {
                self.logged_in = true;
                self.server = server;
                self.password.clear();
                self.login_error.clear();
            }
            Event::LoggedOut => {
                self.logged_in = false;
                self.model = Model::default();
                self.password.clear();
                self.toast.clear();
                self.rename = None;
                self.confirm_delete = None;
                self.levels.clear();
            }
            Event::Server(message) => {
                if let ServerMessage::Error { message, .. } = &message {
                    self.toast = message.clone();
                }
                match self.model.apply(&message) {
                    Effect::LeaveVoice => self.send(Command::LeaveVoice),
                    Effect::DropVoice => self.send(Command::StopVoice),
                    Effect::None => {}
                }
            }
            Event::History {
                channel_id,
                messages,
            } => {
                if self.model.text_id == Some(channel_id) {
                    self.model.load_history(messages);
                }
            }
            Event::Toast(text) => {
                if self.logged_in {
                    self.toast = text;
                } else {
                    self.login_error = text;
                }
            }
            Event::VoiceStatus(label) => {
                self.model.voice_up = label == "в эфире";
                self.model.voice_label = label;
            }
            Event::VoiceFailed(text) => {
                self.toast = text;
                self.model.clear_voice();
                self.levels.clear();
            }
            Event::VoiceDropped => {
                self.model.clear_voice();
                self.levels.clear();
            }
            Event::Level { user_id, milli } => {
                self.levels.insert(user_id, milli);
            }
            Event::RemoteAudio => {}
        }
    }

    fn send_draft(&mut self) {
        let body = self.draft.text();
        let body = body.trim();
        let Some(channel_id) = self.model.text_id else {
            return;
        };
        if body.is_empty() {
            return;
        }
        self.send(Command::Send(ClientMessage::SendMessage {
            channel_id,
            body: body.to_string(),
        }));
        self.draft = text_editor::Content::new();
    }

    fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let body = if self.logged_in {
            self.shell()
        } else {
            self.login()
        };
        container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style::default().background(color!(0x14120f)))
            .into()
    }

    fn login(&self) -> Element<'_, Message> {
        let form = column![
            text("self-dis").size(40),
            text("Закрытая комната. Логин выдаёт администратор.").color(color!(0xa39888)),
            text("Сервер").color(color!(0xa39888)).size(14),
            text_input("http://127.0.0.1:8080", &self.server).on_input(Message::ServerChanged),
            text("Имя").color(color!(0xa39888)).size(14),
            text_input("имя", &self.username).on_input(Message::UsernameChanged),
            text("Пароль").color(color!(0xa39888)).size(14),
            text_input("пароль", &self.password)
                .secure(true)
                .on_input(Message::PasswordChanged)
                .on_submit(Message::Login),
            button("Войти")
                .on_press(Message::Login)
                .style(button::primary),
            text(&self.login_error).color(color!(0xe07a68)),
        ]
        .spacing(10)
        .padding(28)
        .width(380);
        container(container(form).style(|_| {
            container::Style::default()
                .background(color!(0x1e1b16))
                .border(iced::Border {
                    color: color!(0x3c352c),
                    width: 1.0,
                    radius: 18.0.into(),
                })
        }))
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .into()
    }

    fn shell(&self) -> Element<'_, Message> {
        row![
            container(self.sidebar())
                .width(300)
                .height(Length::Fill)
                .padding(16)
                .style(|_| container::Style::default().background(color!(0x1e1b16))),
            container(self.chat())
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(16),
        ]
        .into()
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let who = self
            .model
            .me
            .as_ref()
            .map(|user| format!("{} · {}", user.username, role_label(user.role)));
        let mut side = column![
            text("self-dis").size(40),
            text(who.unwrap_or_default()).color(MUTED).size(14),
            rule::horizontal(1),
            text("КТО ЗДЕСЬ").color(MUTED).size(12),
            self.online(),
            rule::horizontal(1),
            text("КАНАЛЫ").color(MUTED).size(12),
        ]
        .spacing(8);
        side = side.push(scrollable(self.channels()).height(Length::Fill));
        if self
            .model
            .me
            .as_ref()
            .is_some_and(|user| user.role == Role::Admin)
        {
            side = side.push(
                row![
                    text_input("Новая голосовая", &self.new_room)
                        .on_input(Message::NewRoomChanged)
                        .on_submit(Message::CreateRoom),
                    button(text("Создать").color(ACCENT_INK))
                        .on_press(Message::CreateRoom)
                        .style(button::primary),
                ]
                .spacing(8),
            );
        }
        side = side.push(self.voice_bar());
        if !self.toast.is_empty() {
            side = side.push(text(&self.toast).color(DANGER).size(13));
        }
        side.push(
            button("Выйти")
                .on_press(Message::Logout)
                .width(Length::Fill)
                .style(|_theme, _status| bordered(LINE, TEXT)),
        )
        .into()
    }

    fn online(&self) -> Element<'_, Message> {
        let mut people = self.model.online.clone();
        people.sort_by(|left, right| {
            left.username
                .to_lowercase()
                .cmp(&right.username.to_lowercase())
        });
        let rows = people
            .into_iter()
            .map(|user| text(user.username).size(14).into())
            .collect::<Vec<Element<_>>>();
        Column::with_children(rows).spacing(2).into()
    }

    fn channels(&self) -> Element<'_, Message> {
        let mut list = column![text("ТЕКСТ").color(MUTED).size(12)].spacing(4);
        for channel in self
            .model
            .channels
            .iter()
            .filter(|channel| channel.kind == ChannelKind::Text)
        {
            list = list.push(self.channel_row(channel));
        }
        list = list.push(text("ГОЛОС").color(MUTED).size(12));
        for channel in self
            .model
            .channels
            .iter()
            .filter(|channel| channel.kind == ChannelKind::Voice)
        {
            list = list.push(self.channel_row(channel));
            list = list.push(self.people(channel.id));
        }
        list.into()
    }

    fn channel_row(&self, channel: &Channel) -> Element<'_, Message> {
        let active = match channel.kind {
            ChannelKind::Text => self.model.text_id == Some(channel.id),
            ChannelKind::Voice => self.model.voice_channel_id == Some(channel.id),
        };
        let open = match channel.kind {
            ChannelKind::Text => Message::SelectText(channel.id),
            ChannelKind::Voice => Message::JoinVoice(channel.id),
        };
        let mark = if channel.kind == ChannelKind::Voice {
            ACCENT
        } else {
            OK
        };
        let glyph = if channel.kind == ChannelKind::Voice {
            Glyph::Speaker
        } else {
            Glyph::Hash
        };
        let name = text(channel.name.clone()).size(15);
        let name = if active { name.color(ACCENT) } else { name };
        let label = row![icons::draw(glyph, mark, 18.0), name]
            .spacing(8)
            .align_y(Alignment::Center);
        let mut row = row![button(label)
            .on_press(open)
            .style(button::text)
            .width(Length::Fill)]
        .spacing(6)
        .align_y(Alignment::Center);
        if self
            .model
            .me
            .as_ref()
            .is_some_and(|user| user.role == Role::Admin)
            && channel.kind == ChannelKind::Voice
        {
            row = row.push(
                button("имя")
                    .on_press(Message::StartRename(channel.id, channel.name.clone()))
                    .style(button::text),
            );
            row = row.push(
                button("убрать")
                    .on_press(Message::AskDelete(channel.id))
                    .style(button::text),
            );
        }
        let mut block = column![row].spacing(4);
        if self
            .rename
            .as_ref()
            .is_some_and(|(id, _)| *id == channel.id)
        {
            let value = self
                .rename
                .as_ref()
                .map(|(_, name)| name.as_str())
                .unwrap_or("");
            block = block.push(
                row![
                    text_input("Название", value)
                        .on_input(Message::RenameChanged)
                        .on_submit(Message::SubmitRename),
                    button("ок")
                        .on_press(Message::SubmitRename)
                        .style(button::primary),
                    button("отмена")
                        .on_press(Message::CancelRename)
                        .style(button::text),
                ]
                .spacing(6),
            );
        }
        if self.confirm_delete == Some(channel.id) {
            block = block.push(
                column![
                    text(format!("Убрать комнату «{}»?", channel.name)).size(13),
                    row![
                        button("убрать")
                            .on_press(Message::ConfirmDelete)
                            .style(button::danger),
                        button("отмена")
                            .on_press(Message::CancelDelete)
                            .style(button::text),
                    ]
                    .spacing(6),
                ]
                .spacing(4),
            );
        }
        block.into()
    }

    fn people(&self, channel_id: i64) -> Element<'_, Message> {
        let me = self.model.me.as_ref().map(|user| user.id);
        let rows = self
            .model
            .participants(channel_id)
            .iter()
            .map(|person| {
                let speaking =
                    self.levels.get(&person.user_id).copied().unwrap_or(0) > 45 && !person.muted;
                let (glyph, tint) = if person.muted {
                    (Glyph::MicOff, MUTED)
                } else if speaking {
                    (Glyph::Speaker, OK)
                } else {
                    (Glyph::Speaker, LINE)
                };
                let mut name = text(person.username.clone()).size(14);
                if person.muted {
                    name = name.color(MUTED);
                } else if Some(person.user_id) == me {
                    name = name.color(ACCENT);
                }
                let mut line = row![icons::draw(glyph, tint, 18.0), name]
                    .spacing(8)
                    .align_y(Alignment::Center);
                if person.muted {
                    line = line.push(text("выкл").size(12).color(MUTED));
                }
                line.into()
            })
            .collect::<Vec<Element<_>>>();
        Column::with_children(rows)
            .spacing(2)
            .padding(iced::Padding {
                left: 28.0,
                ..Default::default()
            })
            .into()
    }

    fn voice_bar(&self) -> Element<'_, Message> {
        let Some(channel_id) = self.model.voice_channel_id else {
            return text("Голос не подключён").color(MUTED).size(14).into();
        };
        let name = self
            .model
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .map(|channel| channel.name.as_str())
            .unwrap_or("Голос");
        let me = self.model.me.as_ref().map(|user| user.id);
        let milli = me.and_then(|id| self.levels.get(&id).copied()).unwrap_or(0);
        let body = column![
            text(name),
            text(&self.model.voice_label).color(MUTED).size(14),
            meter(milli, self.model.muted),
            row![
                voice_button(
                    if self.model.muted {
                        Glyph::MicOff
                    } else {
                        Glyph::Mic
                    },
                    if self.model.muted {
                        "Включить микрофон"
                    } else {
                        "Выключить микрофон"
                    },
                    Message::ToggleMute,
                    self.model.muted,
                    false,
                ),
                voice_button(
                    if self.model.deaf {
                        Glyph::HeadphonesOff
                    } else {
                        Glyph::Headphones
                    },
                    if self.model.deaf {
                        "Снова слышать комнату"
                    } else {
                        "Не слышать никого"
                    },
                    Message::ToggleDeafen,
                    self.model.deaf,
                    false,
                ),
                voice_button(
                    Glyph::Leave,
                    "Покинуть комнату",
                    Message::LeaveVoice,
                    false,
                    true,
                ),
            ]
            .spacing(8),
        ]
        .spacing(8);
        container(body)
            .padding(12)
            .width(Length::Fill)
            .style(|_| container::Style {
                text_color: None,
                background: Some(Background::Color(PANEL_2)),
                border: Border {
                    color: PANEL_2,
                    width: 0.0,
                    radius: 12.0.into(),
                },
                shadow: Shadow::default(),
                snap: true,
            })
            .into()
    }

    fn chat(&self) -> Element<'_, Message> {
        let title = self
            .model
            .channels
            .iter()
            .find(|channel| Some(channel.id) == self.model.text_id)
            .map(|channel| channel.name.clone())
            .unwrap_or_else(|| "Чат".into());
        let messages = self
            .model
            .messages
            .iter()
            .map(|message| {
                column![
                    row![
                        text(message.username.clone()),
                        text(format_time(&message.created_at))
                            .color(color!(0xa39888))
                            .size(13),
                    ]
                    .spacing(8),
                    text(message.body.clone()),
                ]
                .spacing(2)
                .into()
            })
            .collect::<Vec<Element<_>>>();
        column![
            text(title).size(22),
            scrollable(Column::with_children(messages).spacing(12))
                .height(Length::Fill)
                .anchor_bottom(),
            text_editor(&self.draft)
                .placeholder("Сообщение")
                .on_action(Message::Draft)
                .key_binding(|press| {
                    let submit =
                        press.key == Key::Named(key::Named::Enter) && !press.modifiers.shift();
                    if submit {
                        Some(text_editor::Binding::Custom(Message::Send))
                    } else {
                        text_editor::Binding::from_key_press(press)
                    }
                })
                .height(Length::Fixed(80.0))
                .padding(8),
            button("Отправить")
                .on_press(Message::Send)
                .style(button::primary),
            text("Enter отправляет, Shift+Enter переносит строку")
                .color(color!(0xa39888))
                .size(12),
        ]
        .spacing(10)
        .into()
    }
}

fn voice_button<'a>(
    glyph: Glyph,
    label: &'a str,
    message: Message,
    active: bool,
    leave: bool,
) -> Element<'a, Message> {
    let tint = if active { DANGER } else { TEXT };
    let button = button(icons::draw(glyph, tint, 18.0))
        .width(40)
        .height(40)
        .padding(0)
        .on_press(message)
        .style(move |_theme, status| {
            let hot = matches!(status, button::Status::Hovered | button::Status::Pressed);
            let border = if active || (leave && hot) {
                DANGER
            } else {
                LINE
            };
            bordered(border, tint)
        });
    tooltip(button, text(label).size(13), tooltip::Position::Top)
        .gap(6)
        .into()
}

fn meter(milli: u16, muted: bool) -> Element<'static, Message> {
    let amount = if muted {
        0.0
    } else {
        (milli as f32 / 1000.0 * 8.0).min(1.0)
    };
    let mut bars = row![].spacing(3).align_y(Alignment::End).height(14);
    for index in 0..4 {
        let gate = (index + 1) as f32 / 4.0;
        let scale = if amount >= gate * 0.45 {
            0.35 + amount * 0.65
        } else {
            0.16
        };
        bars = bars.push(
            container("")
                .width(4)
                .height(Length::Fixed(14.0 * scale))
                .style(|_| container::Style {
                    text_color: None,
                    background: Some(Background::Color(OK)),
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                }),
        );
    }
    bars.into()
}

fn bordered(border: Color, text_color: Color) -> button::Style {
    button::Style {
        background: Some(Background::Color(Color::TRANSPARENT)),
        text_color,
        border: Border {
            color: border,
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

fn theme(_app: &App) -> Theme {
    Theme::custom(
        "self-dis",
        iced::theme::Palette {
            background: color!(0x14120f),
            text: color!(0xf3eee4),
            primary: color!(0xe0b15a),
            success: color!(0x8fbf8a),
            warning: color!(0xe0b15a),
            danger: color!(0xe07a68),
        },
    )
}

fn listen() -> impl iced::futures::Stream<Item = Message> {
    stream::channel(32, async |mut output| {
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let (evt_tx, evt_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || session::run(cmd_rx, evt_tx, true));
        let _ = output.send(Message::Boot(cmd_tx)).await;
        let (async_tx, mut async_rx) = iced::futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            while let Ok(event) = evt_rx.recv() {
                if async_tx.unbounded_send(event).is_err() {
                    break;
                }
            }
        });
        while let Some(event) = async_rx.next().await {
            if output.send(Message::Net(event)).await.is_err() {
                break;
            }
        }
    })
}

fn format_time(value: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
