//! Нативный клиент self-dis для macOS и Windows.
//!
//! Окно в `ui`. Сеть, модель и голос от окна не зависят: их же гоняет тест.
//! Сервер по-прежнему только пересылает Opus. Клиент сам кодирует микрофон
//! и смешивает чужие дорожки.

mod audio;
mod http;
mod icons;
mod model;
mod permit;
mod session;
mod store;
mod stun;
mod ui;
mod voice;

pub use model::{Effect, Model};
pub use session::{run as run_session, Command, Event};
pub use voice::MicMode;

pub fn run() -> iced::Result {
    ui::run()
}
