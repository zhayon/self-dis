mod db;
mod error;
mod http;
mod state;
mod validate;
mod voice;
mod ws;

pub use error::Error;
pub use state::AppState;

pub fn router(state: AppState) -> axum::Router {
    http::router(state)
}
