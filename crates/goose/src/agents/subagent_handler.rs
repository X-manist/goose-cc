use crate::permission::permission_confirmation::PrincipalType;
use crate::permission::{Permission, PermissionConfirmation};
use crate::{
    agents::{
        subagent_task_config::TaskConfig,
        tool_confirmation_router::{
            delegated_tool_confirmation_id, register_delegated_tool_confirmation,
            schedule_unregister_delegated_tool_confirmations_for_subagent,
            unregister_delegated_tool_confirmations_for_subagent,
        },
        Agent, AgentConfig, AgentEvent, SessionConfig,
    },
    conversation::{
        message::{ActionRequiredData, Message, MessageContent},
        Conversation,
    },
    prompt_template::render_template,
    recipe::Recipe,
};
use anyhow::{anyhow, Result};
use futures::StreamExt;
use rmcp::model::{
    ErrorCode, ErrorData, LoggingLevel, LoggingMessageNotificationParam, Notification,
    ServerNotification,
};
use serde::Serialize;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub type OnMessageCallback = Arc<dyn Fn(&Message) + Send + Sync>;

#[derive(Serialize)]
pub struct SubagentPromptContext {
    pub max_turns: usize,
    pub subagent_id: String,
    pub task_instructions: String,
    pub tool_count: usize,
    pub available_tools: String,
}

type AgentMessagesFuture =
    Pin<Box<dyn Future<Output = Result<(Conversation, Option<String>)>> + Send>>;

struct DelegatedConfirmationCleanup {
    subagent_id: String,
    active: bool,
}

impl DelegatedConfirmationCleanup {
    fn new(subagent_id: String) -> Self {
        Self {
            subagent_id,
            active: true,
        }
    }

    async fn finish(mut self) {
        unregister_delegated_tool_confirmations_for_subagent(&self.subagent_id).await;
        self.active = false;
    }
}

impl Drop for DelegatedConfirmationCleanup {
    fn drop(&mut self) {
        if self.active {
            schedule_unregister_delegated_tool_confirmations_for_subagent(self.subagent_id.clone());
        }
    }
}

pub struct SubagentRunParams {
    pub config: AgentConfig,
    pub recipe: Recipe,
    pub task_config: TaskConfig,
    pub return_last_only: bool,
    pub parent_session_id: String,
    pub parent_tool_request_id: Option<String>,
    pub session_id: String,
    pub cancellation_token: Option<CancellationToken>,
    pub on_message: Option<OnMessageCallback>,
    pub notification_tx: Option<tokio::sync::mpsc::UnboundedSender<ServerNotification>>,
    pub approval_forwarding: bool,
}

pub async fn run_subagent_task(params: SubagentRunParams) -> Result<String, anyhow::Error> {
    let return_last_only = params.return_last_only;
    let (messages, final_output) = get_agent_messages(params).await.map_err(|e| {
        ErrorData::new(
            ErrorCode::INTERNAL_ERROR,
            format!("Failed to execute task: {}", e),
            None,
        )
    })?;

    if let Some(output) = final_output {
        return Ok(output);
    }

    Ok(extract_response_text(&messages, return_last_only))
}

fn extract_response_text(messages: &Conversation, return_last_only: bool) -> String {
    if return_last_only {
        messages
            .messages()
            .last()
            .and_then(|message| {
                message.content.iter().find_map(|content| match content {
                    crate::conversation::message::MessageContent::Text(text_content) => {
                        Some(text_content.text.clone())
                    }
                    _ => None,
                })
            })
            .unwrap_or_else(|| String::from("No text content in last message"))
    } else {
        let all_text_content: Vec<String> = messages
            .iter()
            .flat_map(|message| {
                message.content.iter().filter_map(|content| match content {
                    crate::conversation::message::MessageContent::Text(text_content) => {
                        Some(text_content.text.clone())
                    }
                    crate::conversation::message::MessageContent::ToolResponse(tool_response) => {
                        if let Ok(result) = &tool_response.tool_result {
                            let texts: Vec<String> = result
                                .content
                                .iter()
                                .filter_map(|content| {
                                    if let rmcp::model::RawContent::Text(raw_text_content) =
                                        &content.raw
                                    {
                                        Some(raw_text_content.text.clone())
                                    } else {
                                        None
                                    }
                                })
                                .collect();
                            if !texts.is_empty() {
                                Some(format!("Tool result: {}", texts.join("\n")))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
            })
            .collect();

        all_text_content.join("\n")
    }
}

pub const SUBAGENT_TOOL_REQUEST_TYPE: &str = "subagent_tool_request";
pub const SUBAGENT_TOOL_CONFIRMATION_TYPE: &str = "subagent_tool_confirmation";

fn get_agent_messages(params: SubagentRunParams) -> AgentMessagesFuture {
    Box::pin(async move {
        let SubagentRunParams {
            config,
            recipe,
            task_config,
            parent_session_id,
            parent_tool_request_id,
            session_id,
            cancellation_token,
            on_message,
            notification_tx,
            approval_forwarding,
            ..
        } = params;

        let system_instructions = recipe.instructions.clone().unwrap_or_default();
        let user_task = recipe
            .prompt
            .clone()
            .unwrap_or_else(|| "Begin.".to_string());

        let agent = Arc::new(Agent::with_config(config));

        agent
            .update_provider(task_config.provider.clone(), &session_id)
            .await
            .map_err(|e| anyhow!("Failed to set provider on sub agent: {}", e))?;

        for extension in &task_config.extensions {
            if let Err(e) = agent.add_extension(extension.clone(), &session_id).await {
                debug!(
                    "Failed to add extension '{}' to subagent: {}",
                    extension.name(),
                    e
                );
            }
        }

        let has_response_schema = recipe.response.is_some();
        agent
            .apply_recipe_components(recipe.response.clone(), true)
            .await;

        let subagent_prompt =
            build_subagent_prompt(&agent, &task_config, &session_id, system_instructions).await?;
        agent.override_system_prompt(subagent_prompt).await;

        let user_message = Message::user().with_text(user_task);
        let mut conversation = Conversation::new_unvalidated(vec![user_message.clone()]);

        if let Some(activities) = recipe.activities {
            for activity in activities {
                info!("Recipe activity: {}", activity);
            }
        }
        let session_config = SessionConfig {
            id: session_id.clone(),
            schedule_id: None,
            max_turns: task_config.max_turns.map(|v| v as u32),
            retry_config: recipe.retry,
        };
        let confirmation_cleanup = DelegatedConfirmationCleanup::new(session_id.clone());

        let mut stream =
            crate::session_context::with_session_id(Some(session_id.to_string()), async {
                agent
                    .reply(user_message, session_config, cancellation_token)
                    .await
            })
            .await
            .map_err(|e| anyhow!("Failed to get reply from agent: {}", e))?;

        while let Some(message_result) = stream.next().await {
            match message_result {
                Ok(AgentEvent::Message(mut msg)) => {
                    rewrite_tool_confirmations_for_parent(
                        &mut msg,
                        &parent_session_id,
                        &session_id,
                        agent.tool_confirmation_router.clone(),
                        approval_forwarding,
                    )
                    .await;
                    if let Some(ref callback) = on_message {
                        callback(&msg);
                    }
                    if let Some(ref tx) = notification_tx {
                        for content in &msg.content {
                            if let Some(notif) = create_tool_confirmation_notification(
                                content,
                                &session_id,
                                parent_tool_request_id.as_deref(),
                                approval_forwarding,
                            ) {
                                if tx.send(notif).is_err() {
                                    debug!(
                                        "Notification receiver dropped for subagent {}",
                                        session_id
                                    );
                                }
                            }
                            if let Some(notif) = create_tool_notification(content, &session_id) {
                                if tx.send(notif).is_err() {
                                    debug!(
                                        "Notification receiver dropped for subagent {}",
                                        session_id
                                    );
                                }
                            }
                        }
                    }
                    conversation.push(msg);
                }
                Ok(AgentEvent::McpNotification(_)) => {}
                Ok(AgentEvent::HistoryReplaced(updated_conversation)) => {
                    conversation = updated_conversation;
                }
                Err(e) => {
                    tracing::error!("Error receiving message from subagent: {}", e);
                    break;
                }
            }
        }

        confirmation_cleanup.finish().await;

        let final_output = get_final_output(&agent, has_response_schema).await;

        Ok((conversation, final_output))
    })
}

async fn rewrite_tool_confirmations_for_parent(
    msg: &mut Message,
    parent_session_id: &str,
    subagent_id: &str,
    child_router: crate::agents::tool_confirmation_router::ToolConfirmationRouter,
    approval_forwarding: bool,
) {
    for content in &mut msg.content {
        if let MessageContent::ActionRequired(action_required) = content {
            if let ActionRequiredData::ToolConfirmation { id, .. } = &mut action_required.data {
                let child_request_id = id.clone();
                if !approval_forwarding {
                    let _ = child_router
                        .deliver(
                            child_request_id,
                            PermissionConfirmation {
                                principal_type: PrincipalType::Tool,
                                permission: Permission::DenyOnce,
                            },
                        )
                        .await;
                    continue;
                }
                let delegated_id = delegated_tool_confirmation_id(subagent_id, &child_request_id);
                register_delegated_tool_confirmation(
                    parent_session_id.to_string(),
                    subagent_id.to_string(),
                    delegated_id.clone(),
                    child_request_id,
                    child_router.clone(),
                )
                .await;
                *id = delegated_id;
            }
        }
    }
}

async fn build_subagent_prompt(
    agent: &Agent,
    task_config: &TaskConfig,
    session_id: &str,
    system_instructions: String,
) -> Result<String> {
    let tools: Vec<_> = agent
        .list_tools(session_id, None)
        .await
        .into_iter()
        .filter(super::reply_parts::is_tool_visible_to_model)
        .collect();
    render_template(
        "subagent_system.md",
        &SubagentPromptContext {
            max_turns: task_config
                .max_turns
                .expect("TaskConfig always sets max_turns"),
            subagent_id: session_id.to_string(),
            task_instructions: system_instructions,
            tool_count: tools.len(),
            available_tools: tools
                .iter()
                .map(|t| t.name.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        },
    )
    .map_err(|e| anyhow!("Failed to render subagent system prompt: {}", e))
}

async fn get_final_output(agent: &Agent, has_response_schema: bool) -> Option<String> {
    if has_response_schema {
        agent
            .final_output_tool
            .lock()
            .await
            .as_ref()
            .and_then(|tool| tool.final_output.clone())
    } else {
        None
    }
}

pub fn create_tool_notification(
    content: &MessageContent,
    subagent_id: &str,
) -> Option<ServerNotification> {
    if let MessageContent::ToolRequest(req) = content {
        let tool_call = req.tool_call.as_ref().ok()?;

        Some(ServerNotification::LoggingMessageNotification(
            Notification::new(
                LoggingMessageNotificationParam::new(
                    LoggingLevel::Info,
                    serde_json::json!({
                        "type": SUBAGENT_TOOL_REQUEST_TYPE,
                        "subagent_id": subagent_id,
                        "tool_call": {
                            "name": tool_call.name,
                            "arguments": tool_call.arguments
                        }
                    }),
                )
                .with_logger(format!("subagent:{}", subagent_id)),
            ),
        ))
    } else {
        None
    }
}

pub fn create_tool_confirmation_notification(
    content: &MessageContent,
    subagent_id: &str,
    parent_tool_request_id: Option<&str>,
    approval_forwarding: bool,
) -> Option<ServerNotification> {
    if !approval_forwarding {
        return None;
    }
    if let MessageContent::ActionRequired(action_required) = content {
        if let ActionRequiredData::ToolConfirmation {
            id,
            tool_name,
            arguments,
            prompt,
        } = &action_required.data
        {
            return Some(ServerNotification::LoggingMessageNotification(
                Notification::new(
                    LoggingMessageNotificationParam::new(
                        LoggingLevel::Info,
                        serde_json::json!({
                        "type": SUBAGENT_TOOL_CONFIRMATION_TYPE,
                        "subagent_id": subagent_id,
                        "parent_tool_request_id": parent_tool_request_id,
                        "id": id,
                        "tool_name": tool_name,
                            "arguments": arguments,
                            "prompt": prompt
                        }),
                    )
                    .with_logger(format!("subagent:{}", subagent_id)),
                ),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        create_tool_confirmation_notification, create_tool_notification,
        rewrite_tool_confirmations_for_parent, DelegatedConfirmationCleanup,
        SUBAGENT_TOOL_CONFIRMATION_TYPE, SUBAGENT_TOOL_REQUEST_TYPE,
    };
    use crate::conversation::message::{Message, MessageContent};
    use crate::permission::permission_confirmation::PrincipalType;
    use crate::permission::{Permission, PermissionConfirmation};
    use rmcp::model::{CallToolRequestParams, ServerNotification};
    use serde_json::json;

    #[test]
    fn create_tool_notification_for_tool_request() {
        let tool_call = CallToolRequestParams::new("developer__shell".to_string())
            .with_arguments(json!({"command": "ls"}).as_object().unwrap().clone());
        let content = MessageContent::tool_request("req1", Ok(tool_call));
        let notification =
            create_tool_notification(&content, "session_1").expect("expected notification");

        let ServerNotification::LoggingMessageNotification(log_notif) = notification else {
            panic!("expected logging notification");
        };
        let data = log_notif
            .params
            .data
            .as_object()
            .expect("expected object data");
        assert_eq!(
            data.get("type").and_then(|v| v.as_str()),
            Some(SUBAGENT_TOOL_REQUEST_TYPE)
        );
        assert_eq!(
            data.get("subagent_id").and_then(|v| v.as_str()),
            Some("session_1")
        );
        let tool_call = data
            .get("tool_call")
            .and_then(|v| v.as_object())
            .expect("expected tool_call object");
        assert_eq!(
            tool_call.get("name").and_then(|v| v.as_str()),
            Some("developer__shell")
        );
    }

    #[test]
    fn create_tool_notification_ignores_non_tool_request() {
        let content = MessageContent::text("hello");
        assert!(create_tool_notification(&content, "session_1").is_none());
    }

    #[test]
    fn create_tool_confirmation_notification_for_subagent_action_required() {
        let content = MessageContent::action_required(
            "subagent:session_1:req1",
            "developer__shell".to_string(),
            serde_json::json!({"command": "touch x"})
                .as_object()
                .unwrap()
                .clone(),
            Some("confirm shell".to_string()),
        );
        let notification =
            create_tool_confirmation_notification(&content, "session_1", Some("parent_req"), true)
                .expect("expected notification");

        let ServerNotification::LoggingMessageNotification(log_notif) = notification else {
            panic!("expected logging notification");
        };
        let data = log_notif
            .params
            .data
            .as_object()
            .expect("expected object data");
        assert_eq!(
            data.get("type").and_then(|v| v.as_str()),
            Some(SUBAGENT_TOOL_CONFIRMATION_TYPE)
        );
        assert_eq!(
            data.get("id").and_then(|v| v.as_str()),
            Some("subagent:session_1:req1")
        );
        assert_eq!(
            data.get("parent_tool_request_id").and_then(|v| v.as_str()),
            Some("parent_req")
        );
        assert_eq!(
            data.get("tool_name").and_then(|v| v.as_str()),
            Some("developer__shell")
        );
    }

    #[test]
    fn create_tool_confirmation_notification_skips_when_forwarding_disabled() {
        let content = MessageContent::action_required(
            "req1",
            "developer__shell".to_string(),
            serde_json::Map::new(),
            None,
        );
        assert!(create_tool_confirmation_notification(
            &content,
            "session_1",
            Some("parent_req"),
            false
        )
        .is_none());
    }

    #[tokio::test]
    async fn rewrite_tool_confirmations_for_parent_registers_forwarded_id() {
        let router = crate::agents::tool_confirmation_router::ToolConfirmationRouter::new();
        let child_rx = router.register("req1".to_string()).await;
        let mut message = Message::assistant().with_action_required(
            "req1",
            "developer__shell".to_string(),
            serde_json::Map::new(),
            None,
        );

        rewrite_tool_confirmations_for_parent(&mut message, "parent1", "sub1", router, true).await;

        let action = message.content[0].as_action_required().unwrap();
        let crate::conversation::message::ActionRequiredData::ToolConfirmation { id, .. } =
            &action.data
        else {
            panic!("expected tool confirmation");
        };
        assert_eq!(id, "subagent:sub1:req1");
        assert!(
            crate::agents::tool_confirmation_router::deliver_delegated_tool_confirmation(
                "parent1",
                id.as_str(),
                PermissionConfirmation {
                    principal_type: PrincipalType::Tool,
                    permission: Permission::AllowOnce,
                },
            )
            .await
        );
        assert_eq!(child_rx.await.unwrap().permission, Permission::AllowOnce);
    }

    #[tokio::test]
    async fn delegated_confirmation_cleanup_removes_pending_routes_on_drop() {
        let router = crate::agents::tool_confirmation_router::ToolConfirmationRouter::new();
        let _child_rx = router.register("req_cleanup".to_string()).await;
        let mut message = Message::assistant().with_action_required(
            "req_cleanup",
            "developer__shell".to_string(),
            serde_json::Map::new(),
            None,
        );

        rewrite_tool_confirmations_for_parent(&mut message, "parent1", "sub_cleanup", router, true)
            .await;
        let action = message.content[0].as_action_required().unwrap();
        let crate::conversation::message::ActionRequiredData::ToolConfirmation { id, .. } =
            &action.data
        else {
            panic!("expected tool confirmation");
        };
        assert_eq!(id, "subagent:sub_cleanup:req_cleanup");

        let cleanup = DelegatedConfirmationCleanup::new("sub_cleanup".to_string());
        drop(cleanup);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        assert!(
            !crate::agents::tool_confirmation_router::deliver_delegated_tool_confirmation(
                "parent1",
                id.as_str(),
                PermissionConfirmation {
                    principal_type: PrincipalType::Tool,
                    permission: Permission::AllowOnce,
                },
            )
            .await
        );
    }

    #[tokio::test]
    async fn rewrite_tool_confirmations_for_parent_denies_when_forwarding_disabled() {
        let router = crate::agents::tool_confirmation_router::ToolConfirmationRouter::new();
        let child_rx = router.register("req1".to_string()).await;
        let mut message = Message::assistant().with_action_required(
            "req1",
            "developer__shell".to_string(),
            serde_json::Map::new(),
            None,
        );

        rewrite_tool_confirmations_for_parent(&mut message, "parent1", "sub1", router, false).await;

        let action = message.content[0].as_action_required().unwrap();
        let crate::conversation::message::ActionRequiredData::ToolConfirmation { id, .. } =
            &action.data
        else {
            panic!("expected tool confirmation");
        };
        assert_eq!(id, "req1");
        assert_eq!(child_rx.await.unwrap().permission, Permission::DenyOnce);
    }
}
