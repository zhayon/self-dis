//! HTTP к серверу: вход, история, адрес.

use std::net::IpAddr;
use std::time::Duration;

use self_dis_protocol::{ChatMessage, ErrorBody, LoginRequest, LoginResponse, User};
use url::Url;

pub fn normalize_server(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("укажите адрес сервера".into());
    }
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let url = Url::parse(&with_scheme).map_err(|_| "некорректный адрес сервера".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err("адрес должен начинаться с http:// или https://".into());
    }
    Ok(format!("{}://{}", url.scheme(), authority(&url)))
}

pub fn ws_url(server: &str) -> Result<String, String> {
    let base = normalize_server(server)?;
    let url = Url::parse(&base).map_err(|_| "некорректный адрес сервера".to_string())?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    Ok(format!("{scheme}://{}/api/ws", authority(&url)))
}

pub fn is_loopback(server: &str) -> bool {
    let Ok(base) = normalize_server(server) else {
        return false;
    };
    let Ok(url) = Url::parse(&base) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub fn server_ip(server: &str) -> Option<IpAddr> {
    let base = normalize_server(server).ok()?;
    let url = Url::parse(&base).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default().unwrap_or(80);
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .ok()?
        .next()
        .map(|addr| addr.ip())
}

pub fn login(server: &str, username: &str, password: &str) -> Result<LoginResponse, String> {
    let response = client()
        .post(format!("{}/api/login", normalize_server(server)?))
        .json(&LoginRequest {
            username: username.to_string(),
            password: password.to_string(),
        })
        .send()
        .map_err(|_| "не удалось связаться с сервером".to_string())?;
    if !response.status().is_success() {
        return Err(error_message(response));
    }
    response
        .json()
        .map_err(|_| "не удалось связаться с сервером".into())
}

pub fn me(server: &str, token: &str) -> Result<User, String> {
    let response = client()
        .get(format!("{}/api/me", normalize_server(server)?))
        .bearer_auth(token)
        .send()
        .map_err(|_| "не удалось связаться с сервером".to_string())?;
    if !response.status().is_success() {
        return Err(error_message(response));
    }
    response
        .json()
        .map_err(|_| "не удалось связаться с сервером".into())
}

pub fn logout(server: &str, token: &str) {
    let Ok(base) = normalize_server(server) else {
        return;
    };
    let _ = client()
        .post(format!("{base}/api/logout"))
        .bearer_auth(token)
        .send();
}

pub fn history(server: &str, token: &str, channel_id: i64) -> Result<Vec<ChatMessage>, String> {
    let response = client()
        .get(format!(
            "{}/api/channels/{channel_id}/messages?limit=50",
            normalize_server(server)?
        ))
        .bearer_auth(token)
        .send()
        .map_err(|_| "не удалось связаться с сервером".to_string())?;
    if !response.status().is_success() {
        return Err(error_message(response));
    }
    response
        .json()
        .map_err(|_| "не удалось связаться с сервером".into())
}

fn authority(url: &Url) -> String {
    let host = match url.host() {
        Some(url::Host::Ipv6(ip)) => format!("[{ip}]"),
        Some(host) => host.to_string(),
        None => String::new(),
    };
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    }
}

fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("http client")
}

fn error_message(response: reqwest::blocking::Response) -> String {
    let status = response.status();
    match response.json::<ErrorBody>() {
        Ok(body) if !body.error.message.is_empty() => body.error.message,
        _ if status == reqwest::StatusCode::UNAUTHORIZED => "неверный логин или пароль".into(),
        _ => "не удалось связаться с сервером".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_address() {
        assert_eq!(
            normalize_server("127.0.0.1:8080").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_server("http://127.0.0.1:8080/").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            ws_url("https://example.com").unwrap(),
            "wss://example.com/api/ws"
        );
        assert!(is_loopback("http://localhost:8080"));
        assert!(is_loopback("127.0.0.1:8080"));
        assert!(!is_loopback("http://203.0.113.10:8080"));
    }
}
