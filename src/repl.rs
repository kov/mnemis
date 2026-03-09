use anyhow::Result;
use rustyline::DefaultEditor;

use crate::agent::Agent;
use crate::imap_client::ImapClient;
use crate::memory::MemoryStore;
use crate::state::StateStore;

pub async fn run_repl(
    agent: &mut Agent,
    imap: &mut ImapClient,
    memory: &MemoryStore,
    state: &mut StateStore,
) -> Result<()> {
    let history_dir = crate::config::expand_tilde("~/.local/share/mnemis");
    std::fs::create_dir_all(&history_dir)?;
    let history_path = history_dir.join("history.txt");

    let mut rl = DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    loop {
        match rl.readline("mnemis> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if line == "quit" || line == "exit" {
                    break;
                }

                let _ = rl.add_history_entry(line);

                match agent.run(Some(line), imap, memory, state).await {
                    Ok(response) => {
                        if !response.is_empty() {
                            println!("{response}");
                        }
                    }
                    Err(err) => {
                        eprintln!("Error: {err:#}");
                    }
                }
            }
            Err(
                rustyline::error::ReadlineError::Interrupted | rustyline::error::ReadlineError::Eof,
            ) => {
                break;
            }
            Err(err) => {
                eprintln!("Error: {err:#}");
                break;
            }
        }
    }

    let _ = rl.save_history(&history_path);
    Ok(())
}
