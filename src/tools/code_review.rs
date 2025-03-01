use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use rig::{
    completion::ToolDefinition,
    tool::Tool,
};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use std::error::Error;
use std::fmt;
use std::collections::HashSet;
use url::Url;
use chrono::{DateTime, Utc};
use uuid::Uuid;
use sha2::{Sha256, Digest};
use hex;

#[derive(Debug)]
pub enum CodeReviewError {
    WebSocketError(String),
    ConnectionError(String),
    DeserializationError(String),
}

impl fmt::Display for CodeReviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodeReviewError::WebSocketError(msg) => write!(f, "WebSocket error: {}", msg),
            CodeReviewError::ConnectionError(msg) => write!(f, "Connection error: {}", msg),
            CodeReviewError::DeserializationError(msg) => write!(f, "Deserialization error: {}", msg),
        }
    }
}

impl Error for CodeReviewError {}

#[derive(Debug, Deserialize, Serialize)]
pub struct CodeReviewArgs {
    user_input: String,
    code: String,
}

// MAGI Gateway message types
#[derive(Deserialize, Debug)]
struct ConnectionEstablished {
    #[serde(rename = "type")]
    message_type: String,
    session_id: String,
}

#[derive(Deserialize, Debug)]
struct MessageReceived {
    #[serde(rename = "type")]
    message_type: String,
    session_id: String,
    status: String,
    request_id: String,
    agent_id: String,
    #[serde(default)]
    content: String,
    timestamp: String,
}

#[derive(Deserialize, Debug)]
struct AgentErrorResponse {
    #[serde(rename = "type")]
    message_type: String,
    session_id: String,
    status: String,
    request_id: String,
    agent_id: String,
    error: String,
    timestamp: String,
}

#[derive(Deserialize, Debug)]
struct AgentResponse {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    message_type: String,
    agent_id: String,
    request_id: String,
    content: String,
    status: String,
    #[allow(dead_code)]
    timestamp: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MAGIMessage {
    pub request_id: String,
    pub content: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum MAGIDecision {
    POSITIVE,
    NEGATIVE,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MAGIAgentState {
    pub messages: Vec<MAGIMessage>,
    pub decision: Option<MAGIDecision>,
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MAGISystemState {
    pub melchior: MAGIAgentState,
    pub balthasar: MAGIAgentState,
    pub casper: MAGIAgentState,
}

impl Default for MAGISystemState {
    fn default() -> Self {
        Self {
            melchior: MAGIAgentState { messages: vec![], decision: None, content: String::new() },
            balthasar: MAGIAgentState { messages: vec![], decision: None, content: String::new() },
            casper: MAGIAgentState { messages: vec![], decision: None, content: String::new() },
        }
    }
}

impl MAGISystemState {
    pub fn get_final_decision(&self) -> Option<MAGIDecision> {
        let positive_count = [&self.melchior, &self.balthasar, &self.casper]
            .iter()
            .filter(|state| matches!(state.decision, Some(MAGIDecision::POSITIVE)))
            .count();
        
        if positive_count >= 2 {
            Some(MAGIDecision::POSITIVE)
        } else if [&self.melchior, &self.balthasar, &self.casper]
            .iter()
            .all(|state| state.decision.is_some()) {
            Some(MAGIDecision::NEGATIVE)
        } else {
            None
        }
    }
}

// Constants for MAGI Gateway
const APP_ID: &str = "b75fce6f-e8af-4207-9c32-f8166afb4520";
const APP_SECRET: &str = "magi-gateway-development-secret";
const AGENT_IDS: [(&str, &str); 3] = [
    ("melchior", "d37c1cc8-bcc4-4b73-9f49-a93a30971f2c"),
    ("balthasar", "6634d0ec-d700-4a92-9066-4960a0f11927"),
    ("casper", "89cbe912-25d0-47b0-97da-b25622bfac0d"),
];

#[derive(Serialize, Debug)]
struct AgentJudgementRequest {
    #[serde(rename = "type")]
    message_type: String,
    request_id: String,
    request: String,
    timestamp: f64,
    agents: Vec<AgentInfo>,
}

#[derive(Serialize, Debug)]
struct AgentInfo {
    agent_id: String,
}

#[derive(Serialize)]
pub struct CodeReviewOutput {
    reviews: Vec<String>,
    result: String,
    passed: bool,
    magi_state: MAGISystemState,
    code: String,
}

pub struct CodeReviewTool;

impl CodeReviewTool {
    pub fn new() -> Self {
        Self {}
    }
}

impl Default for CodeReviewTool {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for CodeReviewTool {
    const NAME: &'static str = "code_review";
    type Error = CodeReviewError;
    type Args = CodeReviewArgs;
    type Output = CodeReviewOutput;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        // println!("[DEBUG] CodeReviewTool::definition called");
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Review generated code through a panel of expert reviewers".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "user_input": {
                        "type": "string",
                        "description": "The user input to the code review tool"
                    },
                    "code": {
                        "type": "string",
                        "description": "The code to be reviewed"
                    }
                },
                "required": ["code"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        // println!("[DEBUG] CodeReviewTool::call called with args: {:?}", args);
        // Get WebSocket URL from environment variable
        let review_server_url = std::env::var("CODE_REVIEW_SERVER_URL")
            .unwrap_or_else(|_| "ws://localhost:8080/review".to_string());

        // Parse WebSocket URL
        let mut url = Url::parse(&review_server_url).map_err(|e| {
            CodeReviewError::ConnectionError(format!("Invalid WebSocket URL: {}", e))
        })?;
        
        // Generate authentication token
        let current_minute = chrono::Utc::now().timestamp() / 60;
        let raw_str = format!("{}{}{}", APP_ID, APP_SECRET, current_minute);
        let mut hasher = Sha256::new();
        hasher.update(raw_str.as_bytes());
        let token = hex::encode(&hasher.finalize())[..10].to_string();
        
        // Add query parameters for authentication
        url.query_pairs_mut()
            .append_pair("appid", APP_ID)
            .append_pair("token", &token);
            
        // println!("[DEBUG] Connecting to WebSocket with URL: {}", url);

        // Connect to WebSocket server
        let (ws_stream, _) = connect_async(url).await.map_err(|e| {
            CodeReviewError::ConnectionError(format!("Failed to connect to WebSocket server: {}", e))
        })?;
        
        let (mut write, mut read) = ws_stream.split();
        
        // Generate a unique request ID
        let request_id = Uuid::new_v4().to_string();
        
        // Create agent judgement request
        let agent_request = AgentJudgementRequest {
            message_type: "agent_judgement".to_string(),
            request_id: request_id.clone(),
            request: format!("<user_input>\n{}\n</user_input>\n<response>\n{}\n</response>", args.user_input, args.code),
            timestamp: chrono::Utc::now().timestamp() as f64,
            agents: AGENT_IDS.iter().map(|(_, id)| AgentInfo {
                agent_id: id.to_string(),
            }).collect(),
        };
        
        // Send the request
        write.send(Message::Text(serde_json::to_string(&agent_request).map_err(|e| {
            CodeReviewError::DeserializationError(format!("Failed to serialize request: {}", e))
        })?)).await.map_err(|e| {
            CodeReviewError::WebSocketError(format!("Failed to send review request: {}", e))
        })?;
        
        // Process streaming responses
        let mut reviews = Vec::new();
        let mut final_result = String::new();
        let mut passed = false;
        let mut magi_state = MAGISystemState::default();
        let mut completed_agents = HashSet::new();
        let mut error_messages = Vec::new();
        
        // Wait for responses from all three agents
        while let Some(msg) = read.next().await {
            let msg = msg.map_err(|e| {
                CodeReviewError::WebSocketError(format!("Error receiving message: {}", e))
            })?;
            
            if let Message::Text(text) = msg {
                // println!("[DEBUG] Received message: {}", text);
                
                // Try to parse as different message types
                if let Ok(response) = serde_json::from_str::<AgentResponse>(&text) {
                    // Only process messages for our request
                    if response.request_id != request_id {
                        continue;
                    }
                    
                    // Find which agent this is
                    let agent_name = AGENT_IDS.iter()
                        .find(|(_, id)| *id == response.agent_id)
                        .map(|(name, _)| name)
                        .unwrap_or(&"unknown");
                    
                    // Add to reviews
                    let review_msg = format!("Reviewer {}: {}", agent_name, response.content);
                    reviews.push(review_msg.clone());
                    
                    // Update MAGI state
                    let agent_state = match *agent_name {
                        "melchior" => &mut magi_state.melchior,
                        "balthasar" => &mut magi_state.balthasar,
                        "casper" => &mut magi_state.casper,
                        _ => continue,
                    };
                    
                    agent_state.messages.push(MAGIMessage {
                        request_id: response.request_id.clone(),
                        content: response.content.clone(),
                        timestamp: Utc::now(),
                    });
                    
                    // Append content to agent state
                    agent_state.content.push_str(&response.content);
                    
                    // Check if this is a completion message
                    if response.status == "completed" {
                        // Extract decision from content
                        if response.content.contains("POSITIVE") {
                            agent_state.decision = Some(MAGIDecision::POSITIVE);
                        } else {
                            agent_state.decision = Some(MAGIDecision::NEGATIVE);
                        }
                        
                        completed_agents.insert(agent_name.to_string());
                        
                        // If all agents have completed, determine final result
                        if completed_agents.len() >= 3 {
                            // Get final decision
                            if let Some(decision) = magi_state.get_final_decision() {
                                match decision {
                                    MAGIDecision::POSITIVE => {
                                        final_result = "POSITIVE".to_string();
                                        passed = true;
                                        let output = CodeReviewOutput {
                                            reviews,
                                            result: final_result,
                                            passed,
                                            magi_state,
                                            code: args.code,
                                        };
                                        return Ok(output);
                                    },
                                    MAGIDecision::NEGATIVE => {
                                        final_result = "NEGATIVE".to_string();
                                        passed = false;
                                        let output = CodeReviewOutput {
                                            reviews,
                                            result: final_result,
                                            passed,
                                            magi_state,
                                            code: args.code,
                                        };
                                        return Ok(output);
                                    },
                                }
                                break; // Exit loop once we have a final decision
                            }
                        }
                    }
                } else if let Ok(message) = serde_json::from_str::<MessageReceived>(&text) {
                    // Process agent_response messages
                    if message.message_type == "agent_response" {
                        // Only process messages for our request
                        if message.request_id != request_id {
                            continue;
                        }
                        
                        // Find which agent this is
                        let agent_name = AGENT_IDS.iter()
                            .find(|(_, id)| *id == message.agent_id)
                            .map(|(name, _)| name)
                            .unwrap_or(&"unknown");
                        
                        // Update MAGI state
                        let agent_state = match *agent_name {
                            "melchior" => &mut magi_state.melchior,
                            "balthasar" => &mut magi_state.balthasar,
                            "casper" => &mut magi_state.casper,
                            _ => continue,
                        };
                        
                        // Handle streaming or completed status
                        if message.status == "streaming" {
                            // Append streaming message to agent content
                            agent_state.content.push_str(&message.content);
                            
                            // Add to messages
                            agent_state.messages.push(MAGIMessage {
                                request_id: message.request_id.clone(),
                                content: message.content.clone(),
                                timestamp: Utc::now(),
                            });
                        } else if message.status == "completed" {
                            // Mark agent as completed
                            completed_agents.insert(agent_name.to_string());
                            
                            // Extract decision from content
                            if agent_state.content.contains("POSITIVE") {
                                agent_state.decision = Some(MAGIDecision::POSITIVE);
                            } else {
                                agent_state.decision = Some(MAGIDecision::NEGATIVE);
                            }
                            
                            // If all agents have completed, determine final result
                            if completed_agents.len() >= 3 {
                                // Get final decision using majority rule
                                if let Some(decision) = magi_state.get_final_decision() {
                                    match decision {
                                        MAGIDecision::POSITIVE => {
                                            final_result = "POSITIVE".to_string();
                                            passed = true;
                                        },
                                        MAGIDecision::NEGATIVE => {
                                            final_result = "NEGATIVE".to_string();
                                            passed = false;
                                        },
                                    }
                                    break; // Exit loop once we have a final decision
                                }
                            }
                        }
                    }
                } else if let Ok(error_response) = serde_json::from_str::<AgentErrorResponse>(&text) {
                    // Handle error responses
                    if error_response.request_id == request_id {
                        let agent_name = AGENT_IDS.iter()
                            .find(|(_, id)| *id == error_response.agent_id)
                            .map(|(name, _)| name)
                            .unwrap_or(&"unknown");
                        
                        let error_msg = format!("Reviewer {} error: {}", agent_name, error_response.error);
                        error_messages.push(error_msg.clone());
                        
                        // Mark this agent as completed with a NEGATIVE decision
                        let agent_state = match *agent_name {
                            "melchior" => &mut magi_state.melchior,
                            "balthasar" => &mut magi_state.balthasar,
                            "casper" => &mut magi_state.casper,
                            _ => continue,
                        };
                        
                        agent_state.messages.push(MAGIMessage {
                            request_id: error_response.request_id.clone(),
                            content: format!("ERROR: {}", error_response.error),
                            timestamp: Utc::now(),
                        });
                        
                        agent_state.decision = Some(MAGIDecision::NEGATIVE);
                        completed_agents.insert(agent_name.to_string());
                        
                        // If all agents have completed or errored, determine final result
                        if completed_agents.len() >= 3 {
                            final_result = "NEGATIVE".to_string();
                            passed = false;
                            break;
                        }
                    }
                } else {
                    // Just log other message types
                    // println!("[DEBUG] Received other message type: {}", text);
                }
            }
        }

        // If we have error messages, add them to the reviews
        if !error_messages.is_empty() {
            reviews.extend(error_messages);
        }

        // Add accumulated content from each agent to reviews
        reviews.push(format!("Melchior: {}", magi_state.melchior.content));
        reviews.push(format!("Balthasar: {}", magi_state.balthasar.content));
        reviews.push(format!("Casper: {}", magi_state.casper.content));

        Ok(CodeReviewOutput {
            reviews,
            result: final_result,
            passed,
            magi_state,
            code: args.code,
        })
    }
}
