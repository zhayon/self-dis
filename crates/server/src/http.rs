use axum::extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::routing::{get, post};
use axum::{Json, Router};
use self_dis_protocol::{
    Channel, ChatMessage, HistoryQuery, LoginRequest, LoginResponse, User, HISTORY_DEFAULT_LIMIT,
    HISTORY_MAX_LIMIT,
};
use tower_http::trace::TraceLayer;

use crate::db;
use crate::error::{ApiError, Error};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/channels", get(channels))
        .route("/api/channels/{id}/messages", get(messages))
        .route("/api/ws", get(crate::ws::upgrade))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
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
