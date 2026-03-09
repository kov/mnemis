mod agent;
mod config;
mod imap_client;
mod llm;
mod memory;
mod prompt;
mod repl;
mod tools;

use anyhow::Result;
use clap::{Parser, Subcommand};

use config::expand_tilde;

#[derive(Parser)]
#[command(name = "mnemis", about = "Email agent powered by a local LLM")]
struct Cli {
    /// Config file path
    #[arg(long, default_value = "~/.config/mnemis/config.toml")]
    config: String,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run with a specific prompt
    Ask {
        /// The prompt to send
        #[arg(trailing_var_arg = true, required = true)]
        prompt: Vec<String>,
    },
    /// Interactive chat mode
    Chat,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config_path = expand_tilde(&cli.config);
    let cfg = config::Config::load(&config_path).await?;

    let instructions = prompt::build_instructions(&cfg.guidance_file()).await;
    let llm_client = llm::LlmClient::new(&cfg.llm);
    let memory_store = memory::MemoryStore::new(cfg.memory_dir()).await?;
    let mut imap = imap_client::ImapClient::connect(&cfg.imap).await?;
    let mut agent = agent::Agent::new(llm_client, instructions, cfg.llm.max_tool_calls);

    let result: Result<Option<String>> = match cli.command {
        None => {
            // Default autonomous mode
            agent.run(None, &mut imap, &memory_store).await.map(Some)
        }
        Some(Command::Ask { prompt }) => {
            let prompt_str = prompt.join(" ");
            agent
                .run(Some(&prompt_str), &mut imap, &memory_store)
                .await
                .map(Some)
        }
        Some(Command::Chat) => {
            repl::run_repl(&mut agent, &mut imap, &memory_store).await?;
            Ok(None)
        }
    };

    // Logout from IMAP regardless of result
    if let Err(err) = imap.logout().await {
        eprintln!("Warning: IMAP logout failed: {err:#}");
    }

    match result {
        Ok(Some(output)) if !output.is_empty() => {
            println!("{output}");
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(err) => Err(err),
    }
}
