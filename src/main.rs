use dotenv::dotenv;
use rig::{
    cli_chatbot::cli_chatbot,
    agent::Agent,
    completion::{self, Prompt, Completion, PromptError, ToolDefinition},
    message::{self, AssistantContent, Message, ToolCall, ToolFunction, ToolResultContent, UserContent},
    providers::{openai, anthropic},
    OneOrMany,
};
use std::{env, error::Error, thread::current, io::Write};
use serde_json::json;
mod tools;
use tools::code_review::CodeReviewTool;

struct MultiTurnAgent<M: rig::completion::CompletionModel> {
    agent: Agent<M>,
    chat_history: Vec<completion::Message>,
}

impl<M: rig::completion::CompletionModel> MultiTurnAgent<M> {
    async fn multi_turn_prompt(
        &mut self,
        prompt: impl Into<Message> + Send,
    ) -> Result<String, PromptError> {
        // Initial prompt
        let initial_prompt = prompt.into();
        let mut current_prompt = initial_prompt.clone();
        
        // Save initial prompt to history
        self.chat_history.push(current_prompt.clone());
        
        // Code generation and review loop
        loop {
            tracing::info!(target: "rig-magi",
                            "Generating codes"
                        );
            
            // Send prompt to AI
            let resp = self
                .agent
                .completion(current_prompt.clone(), self.chat_history.clone())
                .await?
                .send()
                .await?;

            let mut final_text = None;
            let mut code_approved = false;

            for content in resp.choice.into_iter() {
                match content {
                    AssistantContent::Text(text) => {
                        // AI directly returns text (usually code that has passed review)
                        println!("AI响应: {}", text.text);
                        final_text = Some(text.text.clone());
                        
                        // Save to history
                        let response_message = Message::Assistant {
                            content: OneOrMany::one(AssistantContent::Text(message::Text {
                                text: text.text.clone(),
                            })),
                        };
                        self.chat_history.push(response_message);
                        code_approved = true;
                    }
                    AssistantContent::ToolCall(content) => {
                        
                        tracing::info!(target: "rig-magi",
                            "AI call tool: {}",
                            content.function.name
                        );
                        
                        // Save AI's tool call to history
                        let tool_call_msg = AssistantContent::ToolCall(content.clone());
                        self.chat_history.push(Message::Assistant {
                            content: OneOrMany::one(tool_call_msg),
                        });

                        // Extract tool call information
                        let ToolCall {
                            id,
                            function: ToolFunction { name, arguments },
                        } = content;

                        // Call tool (code review)
                        tracing::info!(target: "rig-magi",
                            "Executing code review"
                        );
                        let tool_result = self.agent.tools.call(&name, arguments.to_string()).await?;

                        // Parse review result
                        if let Ok(review_result) = serde_json::from_str::<serde_json::Value>(&tool_result) {
                            // Check if code passed review
                            if let Some(passed) = review_result.get("passed").and_then(|v| v.as_bool()) {
                                if passed {
                                    tracing::info!(target: "rig-magi",
                                        "Code review passed"
                                    );
                                    
                                    // Extract code
                                    if let Some(code) = review_result.get("code").and_then(|v| v.as_str()) {
                                        final_text = Some(code.to_string());
                                        code_approved = true;
                                        
                                        // Create tool result message and add to history
                                        let tool_result_message =  Message::User {
                                            content: OneOrMany::one(UserContent::ToolResult(message::ToolResult {
                                                id: id.clone(),
                                                content: OneOrMany::one(ToolResultContent::Text(message::Text {
                                                    text: tool_result.clone(),
                                                })),
                                            })),
                                        };

                                        self.chat_history.push(tool_result_message);
                                        
                                        // Add final result message
                                        let final_message = Message::Assistant {
                                            content: OneOrMany::one(AssistantContent::Text(message::Text {
                                                text: code.to_string(),
                                            })),
                                        };
                                        self.chat_history.push(final_message);
                                        
                                        // Return result directly after code passes review
                                        return Ok(code.to_string());
                                    }
                                } else {
                                    println!("Code review failed, continuing improvements...");
                                    tracing::info!(target: "rig-magi",
                                        "Code review failed"
                                    );

                                    tracing::debug!(target: "rig-magi",
                                        "Review result: {}",
                                        tool_result
                                    );
                                    
                                    // Create tool result message
                                    let tool_result_message =  Message::User {
                                        content: OneOrMany::one(UserContent::ToolResult(message::ToolResult {
                                            id: id.clone(),
                                            content: OneOrMany::one(ToolResultContent::Text(message::Text {
                                                text: tool_result.clone(),
                                            })),
                                        })),
                                    };

                                    self.chat_history.push(tool_result_message.clone());
                                    
                                    // Next round prompt uses original request plus review feedback
                                    current_prompt = Message::User {
                                        content: OneOrMany::one(UserContent::Text(message::Text {
                                            text: format!("Please improve the code based on the last review feedback",),
                                        })),
                                    };

                                    break;
                                }
                            }
                        }
                        
                        // If unable to parse review result, use original tool result
                        let tool_result_message = Message::User {
                            content: OneOrMany::one(UserContent::ToolResult(message::ToolResult {
                                id: id.clone(),
                                content: OneOrMany::one(ToolResultContent::Text(message::Text {
                                    text: tool_result.clone(),
                                })),
                            })),
                        };
                        self.chat_history.push(tool_result_message.clone());
                        current_prompt = tool_result_message;
                        
                        break;
                    }
                }
            }

            if code_approved || final_text.is_some() {
                return Ok(final_text.unwrap_or_else(|| "Unable to get final code".to_string()));
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(true)
        .init();

    dotenv().ok();
    
    let openai_client = match env::var("OPENAI_BASE_URL") {
        Ok(base_url) => {
            // println!("Custom OpenAI base URL: {}", base_url);
            tracing::debug!(target: "rig-magi",
                "Custom OpenAI base URL: {base_url}"
            );

            openai::Client::from_url(
                &env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY unset"),
                &base_url
            )
        },
        Err(_) => openai::Client::from_env()
    };

    let code_agent = openai_client
        .agent(openai::GPT_4O)
        .preamble(
            "You are a code generation assistant with access to the code_review tool.\
            \
            IMPORTANT: You MUST follow this EXACT workflow:\
            1. First, generate the requested code.\
            2. Then, IMMEDIATELY call the code_review tool with these parameters:\
               - user_input: user's first message\
               - code: your generated code\
            3. Wait for the review results.\
            4. If approved, output the code.\
            5. If rejected, improve and try again.\
            \
            DO NOT output any explanations or comments.\
            DO NOT skip the code review step.\
            ALWAYS use the code_review tool after generating ANY code.\
            \
            Example tool usage:\
            {\"name\": \"code_review\",\
             \"arguments\": {\
                \"user_input\": \"hello world program in python\",\
                \"code\": \"def add(a, b): return a + b\"\
             }\
            }\
            \
            Type 'exit' to quit."
        )
        .tool(CodeReviewTool::new())
        .build();

    let mut agent = MultiTurnAgent {
        agent: code_agent,
        chat_history: Vec::new(),
    };

    println!("🤖 MAGI System Interactive Mode");
    println!("Type 'exit' to quit");
    println!("-------------------");

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    loop {
        print!("> ");
        stdout.flush().unwrap();

        let mut input = String::new();
        match stdin.read_line(&mut input) {
            Ok(_) => {
                let input = input.trim();
                if input == "exit" {
                    break;
                }

                match agent.multi_turn_prompt(input).await {
                    Ok(result) => {
                        println!("🤖 Result:");
                        println!("{}", result);
                        println!("-------------------");
                        agent.chat_history.clear();

                    }
                    Err(e) => {
                        println!("Error: {}", e);
                    }
                }
            }
            Err(error) => println!("Error reading input: {}", error),
        }
    }

    Ok(())
}
