use self_dis_protocol::{
    CHANNEL_NAME_MAX_CHARS, MESSAGE_MAX_CHARS, PASSWORD_MAX_CHARS, PASSWORD_MIN_CHARS,
    USERNAME_MAX_CHARS, USERNAME_MIN_CHARS,
};

use crate::error::Error;

pub fn username(raw: &str) -> Result<String, Error> {
    let name = raw.trim();
    let len = name.chars().count();
    if !(USERNAME_MIN_CHARS..=USERNAME_MAX_CHARS).contains(&len) {
        return Err(Error::Invalid(format!(
            "имя должно быть от {USERNAME_MIN_CHARS} до {USERNAME_MAX_CHARS} символов"
        )));
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(Error::Invalid("пустое имя".into()));
    };
    if !first.is_alphanumeric() {
        return Err(Error::Invalid(
            "имя должно начинаться с буквы или цифры".into(),
        ));
    }
    if !chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        return Err(Error::Invalid(
            "в имени допустимы буквы, цифры, точка, _ и -".into(),
        ));
    }
    Ok(name.to_owned())
}

pub fn password(raw: &str) -> Result<&str, Error> {
    let len = raw.chars().count();
    if !(PASSWORD_MIN_CHARS..=PASSWORD_MAX_CHARS).contains(&len) {
        return Err(Error::Invalid(format!(
            "пароль должен быть от {PASSWORD_MIN_CHARS} до {PASSWORD_MAX_CHARS} символов"
        )));
    }
    Ok(raw)
}

pub fn channel_name(raw: &str) -> Result<String, Error> {
    let name = raw.trim();
    let len = name.chars().count();
    if len == 0 || len > CHANNEL_NAME_MAX_CHARS {
        return Err(Error::Invalid(format!(
            "название комнаты должно быть от 1 до {CHANNEL_NAME_MAX_CHARS} символов"
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "в названии комнаты нельзя использовать управляющие символы".into(),
        ));
    }
    Ok(name.to_owned())
}

pub fn message_body(raw: &str) -> Result<String, Error> {
    let body = raw.trim();
    let len = body.chars().count();
    if len == 0 || len > MESSAGE_MAX_CHARS {
        return Err(Error::Invalid(format!(
            "сообщение должно быть от 1 до {MESSAGE_MAX_CHARS} символов"
        )));
    }
    if body
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(Error::Invalid(
            "в сообщении нельзя использовать управляющие символы".into(),
        ));
    }
    Ok(body.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unicode_username() {
        assert_eq!(username("  Ада  ").unwrap(), "Ада");
    }

    #[test]
    fn rejects_short_password() {
        assert!(password("short").is_err());
    }
}
