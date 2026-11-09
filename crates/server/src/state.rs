use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use self_dis_protocol::{Channel, Role, ServerMessage, User, VoiceRoom};
use sqlx::SqlitePool;
use tokio::sync::broadcast;

use crate::db;
use crate::error::Error;
use crate::voice::Voice;

struct Online {
    user: User,
    connections: usize,
}

#[derive(Clone)]
pub struct AppState {
    pub(crate) db: SqlitePool,
    events: broadcast::Sender<Arc<str>>,
    presence: Arc<Mutex<HashMap<i64, Online>>>,
    pub(crate) voice: Voice,
}

impl AppState {
    pub async fn open(data_dir: &Path) -> Result<Self, Error> {
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            db: db::open(data_dir).await?,
            events,
            presence: Arc::new(Mutex::new(HashMap::new())),
            voice: Voice::new(),
        })
    }

    pub fn start_voice(&self, bind: SocketAddr, advertise_ip: IpAddr) -> Result<SocketAddr, Error> {
        self.voice.start(bind, advertise_ip, self.clone())
    }

    pub(crate) fn voice_snapshot(&self) -> Vec<VoiceRoom> {
        self.voice.snapshot()
    }

    pub fn publish(&self, message: &ServerMessage) {
        match serde_json::to_string(message) {
            Ok(json) => {
                let _ = self.events.send(Arc::from(json));
            }
            Err(err) => tracing::error!(%err, "не удалось сериализовать событие"),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.events.subscribe()
    }

    /// Возвращает `true`, если это первое живое соединение пользователя.
    pub(crate) fn join(&self, user: &User) -> bool {
        let mut presence = self.presence.lock().expect("presence");
        let entry = presence.entry(user.id).or_insert_with(|| Online {
            user: user.clone(),
            connections: 0,
        });
        entry.user = user.clone();
        entry.connections += 1;
        entry.connections == 1
    }

    /// Возвращает `true`, если живых соединений пользователя не осталось.
    pub(crate) fn leave(&self, user_id: i64) -> bool {
        let mut presence = self.presence.lock().expect("presence");
        let Some(entry) = presence.get_mut(&user_id) else {
            return false;
        };
        entry.connections = entry.connections.saturating_sub(1);
        if entry.connections == 0 {
            presence.remove(&user_id);
            true
        } else {
            false
        }
    }

    pub(crate) fn online_users(&self) -> Vec<User> {
        let presence = self.presence.lock().expect("presence");
        let mut users: Vec<_> = presence.values().map(|entry| entry.user.clone()).collect();
        users.sort_by_key(|user| user.id);
        users
    }

    pub async fn create_user(
        &self,
        username: &str,
        password: &str,
        role: Role,
    ) -> Result<User, Error> {
        db::create_user(&self.db, username, password, role).await
    }

    pub async fn list_users(&self) -> Result<Vec<User>, Error> {
        db::list_users(&self.db).await
    }

    pub async fn list_channels(&self) -> Result<Vec<Channel>, Error> {
        db::list_channels(&self.db).await
    }

    pub async fn create_voice_channel(&self, name: &str) -> Result<Channel, Error> {
        db::create_voice_channel(&self.db, name).await
    }

    pub async fn rename_channel(&self, id: i64, name: &str) -> Result<Channel, Error> {
        db::rename_channel(&self.db, id, name).await
    }

    pub async fn delete_channel(&self, id: i64) -> Result<(), Error> {
        db::delete_channel(&self.db, id).await
    }
}

pub(crate) struct PresenceGuard {
    pub(crate) state: AppState,
    pub(crate) user_id: i64,
    pub(crate) signal_id: u64,
    pub(crate) joined_voice: Arc<AtomicBool>,
}

impl Drop for PresenceGuard {
    fn drop(&mut self) {
        self.state.voice.unregister_socket(self.signal_id);
        if self.joined_voice.swap(false, Ordering::Relaxed) {
            self.state.voice.leave(self.user_id);
        }
        if self.state.leave(self.user_id) {
            self.state.publish(&ServerMessage::UserOffline {
                user_id: self.user_id,
            });
        }
    }
}

pub(crate) fn mark_voice_join(flag: &AtomicBool) {
    flag.store(true, Ordering::Relaxed);
}

pub(crate) fn mark_voice_leave(flag: &AtomicBool) {
    flag.store(false, Ordering::Relaxed);
}
