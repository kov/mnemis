use anyhow::{Result, bail};

use crate::imap_client::ImapClient;
use crate::llm::{
    ContentItem, InputItem, LlmClient, OutputItem, ReasoningContentItem, Role, ToolDef,
};
use crate::memory::MemoryStore;
use crate::tools;

pub struct Agent {
    llm: LlmClient,
    instructions: String,
    tool_defs: Vec<ToolDef>,
    max_tool_calls: usize,
    last_response_id: Option<String>,
    verbose: bool,
}

impl Agent {
    pub fn new(llm: LlmClient, instructions: String, max_tool_calls: usize, verbose: bool) -> Self {
        Self {
            llm,
            instructions,
            tool_defs: tools::tool_definitions(),
            max_tool_calls,
            last_response_id: None,
            verbose,
        }
    }

    /// Run one agent turn. Returns the final text output (from message or write_report).
    pub async fn run(
        &mut self,
        user_message: Option<&str>,
        imap: &mut ImapClient,
        memory: &MemoryStore,
    ) -> Result<String> {
        let mut input: Vec<InputItem> = Vec::new();
        if let Some(msg) = user_message {
            input.push(InputItem::Message {
                role: Role::User,
                content: msg.to_string(),
            });
        }

        let mut tool_call_count: usize = 0;
        let mut last_report: Option<String> = None;

        loop {
            let response = self
                .llm
                .send(
                    &self.instructions,
                    std::mem::take(&mut input),
                    &self.tool_defs,
                    self.last_response_id.as_deref(),
                )
                .await?;

            self.last_response_id = Some(response.id);

            // Separate function calls from messages
            let mut function_calls = Vec::new();
            let mut text_parts = Vec::new();

            for item in &response.output {
                match item {
                    OutputItem::FunctionCall {
                        call_id,
                        name,
                        arguments,
                    } => {
                        if self.verbose {
                            eprintln!("  tool: {name}({arguments})");
                        }
                        function_calls.push((call_id.clone(), name.clone(), arguments.clone()));
                    }
                    OutputItem::Message { content } => {
                        for c in content {
                            if let ContentItem::OutputText { text } = c {
                                text_parts.push(text.clone());
                            }
                        }
                    }
                    OutputItem::Reasoning { content } => {
                        if self.verbose {
                            for c in content {
                                if let ReasoningContentItem::ReasoningText { text } = c {
                                    for line in text.lines() {
                                        eprintln!("  thinking: {line}");
                                    }
                                }
                            }
                        }
                    }
                    OutputItem::Unknown => {}
                }
            }

            // If no function calls, we're done
            if function_calls.is_empty() {
                // Prefer the last report if we got one, otherwise use the message text
                return Ok(last_report.unwrap_or_else(|| text_parts.join("")));
            }

            // Execute each tool call
            input = Vec::new();
            for (call_id, name, arguments) in &function_calls {
                tool_call_count += 1;
                if tool_call_count > self.max_tool_calls {
                    bail!(
                        "tool call limit exceeded ({} calls). Stopping agent loop.",
                        self.max_tool_calls
                    );
                }

                eprintln!("  [{tool_call_count}] {name}");

                let result = tools::dispatch(name, arguments, imap, memory).await;
                if let Some(report) = result.report {
                    last_report = Some(report);
                }
                input.push(InputItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: result.output,
                });
            }
        }
    }
}
