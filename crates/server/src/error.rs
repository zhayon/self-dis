use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use self_dis_protocol::{ErrorBody, ErrorDetail};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("неверный логин или пароль")]
    InvalidCredentials,
    #[error("нужна авторизация")]
    Unauthenticated,
    #[error("недостаточно прав")]
    Forbidden,
    #[error("не найдено")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    #[error("голос пока не включён")]
    VoiceNotReady,
    #[error("{0}")]
    Internal(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidCredentials => "invalid_credentials",
            Error::Unauthenticated => "unauthenticated",
            Error::Forbidden => "forbidden",
            Error::NotFound => "not_found",
            Error::Invalid(_) => "invalid",
            Error::VoiceNotReady => "voice_not_ready",
            Error::Internal(_) => "internal",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Error::InvalidCredentials | Error::Unauthenticated => StatusCode::UNAUTHORIZED,
            Error::Forbidden => StatusCode::FORBIDDEN,
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Invalid(_) => StatusCode::BAD_REQUEST,
            Error::VoiceNotReady => StatusCode::NOT_IMPLEMENTED,
            Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    pub fn client_message(&self) -> String {
        match self {
            Error::Internal(_) => "внутренняя ошибка".to_owned(),
            other => other.to_string(),
        }
    }

    pub fn to_server_message(&self) -> self_dis_protocol::ServerMessage {
        if let Error::Internal(detail) = self {
            tracing::error!(error = %detail, "внутренняя ошибка");
        }
        self_dis_protocol::ServerMessage::Error {
            code: self.code().to_owned(),
            message: self.client_message(),
        }
    }
}

pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Error::Internal(ref detail) = self.0 {
            tracing::error!(error = %detail, "запрос завершился внутренней ошибкой");
        }
        let status = self.0.status();
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.0.code().to_owned(),
                message: self.0.client_message(),
            },
        };
        (status, Json(body)).into_response()
    }
}
