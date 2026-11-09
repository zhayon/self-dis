use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use self_dis_protocol::{ChannelKind, Role};
use self_dis_server::{AppState, Error};

#[derive(Parser)]
#[command(name = "self-dis", about = "Закрытый сервер чата и голосовых комнат")]
struct Cli {
    /// Каталог с файлом базы SQLite
    #[arg(long, global = true, default_value = "data")]
    data: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Слушать HTTP и WebSocket
    Serve {
        /// Адрес, например 0.0.0.0:8080
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: SocketAddr,
    },
    /// Пользователи. Открытой регистрации нет
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Каналы в базе. Подключённые клиенты увидят изменение после переподключения
    Channel {
        #[command(subcommand)]
        command: ChannelCommand,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    Add {
        #[arg(long)]
        username: String,
        /// Если не задан, пароль спрашивается в терминале
        #[arg(long)]
        password: Option<String>,
        #[arg(long, value_enum, default_value = "member")]
        role: RoleArg,
    },
    List,
}

#[derive(Subcommand)]
enum ChannelCommand {
    List,
    AddVoice {
        #[arg(long)]
        name: String,
    },
    Rename {
        id: i64,
        #[arg(long)]
        name: String,
    },
    Delete {
        id: i64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RoleArg {
    Admin,
    Member,
}

impl From<RoleArg> for Role {
    fn from(role: RoleArg) -> Self {
        match role {
            RoleArg::Admin => Role::Admin,
            RoleArg::Member => Role::Member,
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "self_dis_server=info,tower_http=info".into()),
        )
        .init();

    if let Err(err) = run(Cli::parse()).await {
        eprintln!("ошибка: {err}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Error> {
    let state = AppState::open(&cli.data).await?;
    match cli.command {
        Command::Serve { listen } => serve(state, listen).await,
        Command::User { command } => user_command(&state, command).await,
        Command::Channel { command } => channel_command(&state, command).await,
    }
}

async fn serve(state: AppState, listen: SocketAddr) -> Result<(), Error> {
    let app = self_dis_server::router(state);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|err| Error::Internal(format!("не удалось занять {listen}: {err}")))?;
    tracing::info!(%listen, "сервер слушает");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("остановка");
        })
        .await
        .map_err(|err| Error::Internal(err.to_string()))
}

async fn user_command(state: &AppState, command: UserCommand) -> Result<(), Error> {
    match command {
        UserCommand::Add {
            username,
            password,
            role,
        } => {
            let password = match password {
                Some(password) => password,
                None => rpassword::prompt_password("Пароль: ").map_err(|err| {
                    Error::Internal(format!("не удалось прочитать пароль: {err}"))
                })?,
            };
            let user = state.create_user(&username, &password, role.into()).await?;
            println!("{} {} {}", user.id, role_label(user.role), user.username);
        }
        UserCommand::List => {
            for user in state.list_users().await? {
                println!("{} {} {}", user.id, role_label(user.role), user.username);
            }
        }
    }
    Ok(())
}

async fn channel_command(state: &AppState, command: ChannelCommand) -> Result<(), Error> {
    match command {
        ChannelCommand::List => {
            for channel in state.list_channels().await? {
                println!(
                    "{} {} {} {}",
                    channel.id,
                    kind_label(channel.kind),
                    channel.position,
                    channel.name
                );
            }
        }
        ChannelCommand::AddVoice { name } => {
            let channel = state.create_voice_channel(&name).await?;
            println!("{} {}", channel.id, channel.name);
        }
        ChannelCommand::Rename { id, name } => {
            let channel = state.rename_channel(id, &name).await?;
            println!("{} {}", channel.id, channel.name);
        }
        ChannelCommand::Delete { id } => {
            state.delete_channel(id).await?;
            println!("удалена {id}");
        }
    }
    Ok(())
}

fn role_label(role: Role) -> &'static str {
    match role {
        Role::Admin => "admin",
        Role::Member => "member",
    }
}

fn kind_label(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::Text => "text",
        ChannelKind::Voice => "voice",
    }
}
