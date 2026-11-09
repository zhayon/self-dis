use std::path::PathBuf;

use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::header::{AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::request::Parts;
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use include_dir::{include_dir, Dir};
use self_dis_protocol::{
    Channel, ChatMessage, HistoryQuery, LoginRequest, LoginResponse, User, HISTORY_DEFAULT_LIMIT,
    HISTORY_MAX_LIMIT,
};
use tower_http::trace::TraceLayer;

use crate::db;
use crate::error::{ApiError, Error};
use crate::state::AppState;

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/channels", get(channels))
        .route("/api/channels/{id}/messages", get(messages))
        .route("/api/ws", get(crate::ws::upgrade))
        .fallback(frontend)
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn frontend(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(body) = read_asset(path) {
        return asset_response(path, body);
    }
    match read_asset("index.html") {
        Some(body) => asset_response("index.html", body),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn asset_response(path: &str, body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (CONTENT_TYPE, content_type(path)),
            (CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

fn read_asset(path: &str) -> Option<Vec<u8>> {
    if !safe_asset_path(path) {
        return None;
    }
    let dev = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");
    if dev.is_dir() {
        let file = dev.join(path);
        if file.is_file() {
            return std::fs::read(file).ok();
        }
    }
    ASSETS.get_file(path).map(|file| file.contents().to_vec())
}

fn safe_asset_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn content_type(path: &str) -> &'static str {
    match std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
    {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        _ => "application/octet-stream",
    }
}

async fn login(
    State(state): State<AppState>,
    body: Result<Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<LoginResponse>, ApiError> {
    let Json(body) = body.map_err(|_| Error::Invalid("некорректный json".into()))?;
    let (token, user) = db::login(&state.db, &body.username, &body.password).await?;
    Ok(Json(LoginResponse { token, user }))
}

async fn logout(
    State(state): State<AppState>,
    AuthUser { token, .. }: AuthUser,
) -> Result<StatusNoContent, ApiError> {
    db::logout(&state.db, &token).await?;
    Ok(StatusNoContent)
}

async fn me(AuthUser { user, .. }: AuthUser) -> Json<User> {
    Json(user)
}

async fn channels(
    State(state): State<AppState>,
    _: AuthUser,
) -> Result<Json<Vec<Channel>>, ApiError> {
    Ok(Json(db::list_channels(&state.db).await?))
}

async fn messages(
    State(state): State<AppState>,
    _: AuthUser,
    Path(id): Path<i64>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Vec<ChatMessage>>, ApiError> {
    let limit = match query.limit {
        None | Some(0) => HISTORY_DEFAULT_LIMIT,
        Some(limit) => limit.min(HISTORY_MAX_LIMIT),
    };
    Ok(Json(
        db::list_messages(&state.db, id, limit, query.before_id).await?,
    ))
}

struct StatusNoContent;

impl axum::response::IntoResponse for StatusNoContent {
    fn into_response(self) -> axum::response::Response {
        axum::http::StatusCode::NO_CONTENT.into_response()
    }
}

pub struct AuthUser {
    pub user: User,
    pub token: String,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or(Error::Unauthenticated)?;
        let (scheme, token) = header.split_once(' ').ok_or(Error::Unauthenticated)?;
        if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
            return Err(Error::Unauthenticated.into());
        }
        let user = db::user_for_token(&state.db, token).await?;
        Ok(AuthUser {
            user,
            token: token.to_owned(),
        })
    }
}
