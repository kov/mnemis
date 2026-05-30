use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

use mnemis_engine::config;

mod commands;

#[derive(Parser)]
#[command(name = "mnemis", about = "mnemis CLI", version)]
struct Cli {
    /// Path to config file (default: ~/.config/mnemis/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the database and run migrations.
    Init {
        /// Display name for the user profile.
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Add a source.
    AddSource {
        #[command(subcommand)]
        kind: AddSourceKind,
    },
    /// Run one polling + extraction cycle for all enabled sources.
    Sync,
    /// List pending and recently-claimed actions.
    ListActions {
        /// Filter by status (comma-separated). Defaults to pending,auto_claimed,claimed.
        #[arg(long)]
        status: Option<String>,
        /// Output as JSON instead of pretty text.
        #[arg(long)]
        json: bool,
    },
    /// Print the extraction prompt that would be sent for a channel, without calling the LLM.
    DumpPrompt {
        /// Channel id (from `sqlite3 ... 'SELECT id, name FROM channels'`).
        channel_id: i64,
    },
    /// Run extraction for one channel against the configured LLM. For debugging.
    Extract {
        /// Channel id.
        channel_id: i64,
    },
    /// Send one message to the chat agent against the configured LLM, printing
    /// streamed events. For exercising the chat loop end-to-end. With no
    /// --chat-id, starts a new chat (optionally seeded from an action/message).
    Chat {
        /// The message to send.
        text: String,
        /// Continue an existing chat instead of starting a new one.
        #[arg(long)]
        chat_id: Option<i64>,
        /// Seed a NEW chat from an action (A-N or bare N).
        #[arg(long)]
        seed_action: Option<String>,
        /// Seed a NEW chat from an internal message id.
        #[arg(long)]
        seed_message: Option<i64>,
    },
    /// Drain the embed queue once against the configured embedding model. For debugging.
    EmbedOnce,
    /// Zero out user data (messages, actions, embeddings, etc.) while keeping
    /// sources, channels, contacts, settings, and user_profile. Source/channel
    /// cursors are rewound so the next sync re-bootstraps.
    ResetData {
        /// Actually perform the wipe. Without this flag, prints counts only.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum AddSourceKind {
    /// Add an IMAP source. Password is read from stdin (prompted) and stored in the OS keychain.
    Imap {
        #[arg(long)]
        server: String,
        #[arg(long, default_value = "993")]
        port: u16,
        #[arg(long)]
        username: String,
        /// Friendly label for this source.
        #[arg(long)]
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::load(cli.config.as_deref()).context("loading config")?;

    match cli.command {
        Command::Init { display_name } => commands::init(&cfg, display_name).await,
        Command::AddSource { kind } => match kind {
            AddSourceKind::Imap {
                server,
                port,
                username,
                name,
            } => commands::add_source_imap(&cfg, &name, &server, port, &username).await,
        },
        Command::Sync => commands::sync(&cfg).await,
        Command::ListActions { status, json } => {
            commands::list_actions(&cfg, status.as_deref(), json).await
        }
        Command::DumpPrompt { channel_id } => commands::dump_prompt(&cfg, channel_id).await,
        Command::Extract { channel_id } => commands::extract(&cfg, channel_id).await,
        Command::Chat {
            text,
            chat_id,
            seed_action,
            seed_message,
        } => commands::chat(&cfg, &text, chat_id, seed_action.as_deref(), seed_message).await,
        Command::EmbedOnce => commands::embed_once(&cfg).await,
        Command::ResetData { yes } => commands::reset_data(&cfg, yes).await,
    }
}
