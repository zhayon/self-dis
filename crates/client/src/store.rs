//! Адрес сервера в файле настроек, токен в связке ключей ОС.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const SERVICE: &str = "self-dis";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct File {
    server: String,
}

pub fn load_server() -> String {
    read()
        .map(|file| file.server)
        .unwrap_or_else(|| "http://127.0.0.1:8080".into())
}

pub fn save_server(server: &str) {
    let file = File {
        server: server.to_string(),
    };
    let Some(path) = path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(body) = serde_json::to_string_pretty(&file) {
        let _ = fs::write(path, body);
    }
}

pub fn load_token(server: &str) -> Option<String> {
    let entry = keyring::Entry::new(SERVICE, server).ok()?;
    entry.get_password().ok()
}

pub fn save_token(server: &str, token: &str) -> bool {
    let Ok(entry) = keyring::Entry::new(SERVICE, server) else {
        return false;
    };
    entry.set_password(token).is_ok()
}

pub fn forget_token(server: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, server) {
        let _ = entry.delete_credential();
    }
}

fn read() -> Option<File> {
    let body = fs::read_to_string(path()?).ok()?;
    serde_json::from_str(&body).ok()
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("self-dis").join("config.json"))
}
