mod agent;
mod config;
mod imap_client;
mod llm;
mod memory;
mod prompt;
mod repl;
mod state;
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

    /// Print LLM reasoning and tool calls to stderr
    #[arg(long)]
    thinking: bool,

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
    let mut state = state::StateStore::load(cfg.state_file()).await?;
    let mut imap = imap_client::ImapClient::connect(&cfg.imap).await?;
    let mut agent = agent::Agent::new(
        llm_client,
        instructions,
        cfg.llm.max_tool_calls,
        cli.thinking,
    );

    let result: Result<Option<String>> = match cli.command {
        None => {
            // Default autonomous mode: one fresh conversation per mailbox
            if cfg.mailboxes.is_empty() {
                anyhow::bail!(
                    "no mailboxes configured. Add a `mailboxes` list to your config file."
                );
            }
            let mut printed_any = false;
            for mailbox in &cfg.mailboxes {
                eprintln!("=== Scanning {mailbox} ===");
                agent.reset();
                let prompt = format!(
                    "Check the mailbox \"{}\" for new unread emails and report per the guidance. \
                     Only look at this single mailbox, do not check other mailboxes.",
                    mailbox
                );
                match agent
                    .run(Some(&prompt), &mut imap, &memory_store, &mut state)
                    .await
                {
                    Ok(report) if !report.is_empty() => {
                        if printed_any {
                            println!("\n---\n");
                        }
                        println!("{report}");
                        printed_any = true;
                    }
                    Ok(_) => {}
                    Err(err) => eprintln!("Warning: error scanning {mailbox}: {err:#}"),
                }
            }
            // Commit watermarks after all mailboxes succeed
            if let Err(err) = state.commit().await {
                eprintln!("Warning: failed to commit state: {err:#}");
            }
            Ok(None)
        }
        Some(Command::Ask { prompt }) => {
            let prompt_str = prompt.join(" ");
            agent
                .run(Some(&prompt_str), &mut imap, &memory_store, &mut state)
                .await
                .map(Some)
        }
        Some(Command::Chat) => {
            repl::run_repl(&mut agent, &mut imap, &memory_store, &mut state).await?;
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
