//! Mock stdio MCP server used by `tests/proxy_full_mcp.rs`.
//!
//! This binary implements every MCP method with hardcoded responses so
//! that the proxy's full forwarding surface — `list_resources`,
//! `list_prompts`, `read_resource`, `complete`, sampling, elicitation,
//! and every notification — can be exercised end-to-end. It is built
//! as a Cargo `[[bin]]` so the integration tests can `spawn_proxy_process`
//! it just like a real stdio MCP server.
//!
//! NOT shipped to end users; only built when test targets compile.

// Sampling, roots, and logging are deprecated by SEP-2577 (advisory only, no
// replacement API), but the mock must still advertise and exercise them so the
// proxy's forwarding of these messages can be tested end-to-end.
#![allow(deprecated)]

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, CompleteRequestParams,
        CompleteResult, CompletionInfo, ContentBlock, CreateMessageRequestParams,
        ElicitRequestParams, ElicitationSchema, ErrorCode, GetPromptRequestParams, GetPromptResult,
        Implementation, InitializeRequestParams, InitializeResult, JsonObject, ListPromptsResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, LoggingLevel,
        LoggingMessageNotificationParam, NumberOrString, PaginatedRequestParams,
        ProgressNotificationParam, ProgressToken, Prompt, PromptMessage, ReadResourceRequestParams,
        ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
        ResourceUpdatedNotificationParam, Role, ServerCapabilities, SetLevelRequestParams,
        SubscribeRequestParams, Tool, UnsubscribeRequestParams,
    },
    serde_json,
    service::{RequestContext, RoleServer},
};

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MockServer;

impl ServerHandler for MockServer {
    // Logging is deprecated by SEP-2577 (advisory only, no replacement API), but
    // the mock must still advertise it so the proxy's forwarding can be tested.
    #[allow(deprecated)]
    async fn initialize(
        &self,
        _request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_resources_subscribe()
            .enable_prompts()
            .enable_completions()
            .enable_logging()
            .build();

        let mut result = InitializeResult::new(caps);
        result.server_info = Implementation::new("mock-mcp-child", "0.0.0");
        Ok(result)
    }

    // -- Tools --------------------------------------------------------------

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema: Arc<JsonObject> = Arc::new(JsonObject::default());
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("echo", "Echo a fixed string", schema.clone()),
            Tool::new(
                "trigger_notifications",
                "Fire every supported child->client notification",
                schema.clone(),
            ),
            Tool::new(
                "trigger_server_requests",
                "Issue every supported child->client request",
                schema,
            ),
        ]))
    }

    // Sampling, roots, and logging are deprecated by SEP-2577 (advisory only, no
    // replacement API), but the mock must still exercise them so the proxy's
    // forwarding of these messages can be tested end-to-end.
    #[allow(deprecated)]
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "echo" => Ok(CallToolResult::success(vec![ContentBlock::text(
                "echo-response",
            )])),

            "trigger_notifications" => {
                let peer = context.peer.clone();

                // Progress
                let _ = peer
                    .notify_progress(
                        ProgressNotificationParam::new(
                            ProgressToken(NumberOrString::String("p-1".into())),
                            0.5,
                        )
                        .with_message("halfway"),
                    )
                    .await;

                // Logging
                let _ = peer
                    .notify_logging_message(
                        LoggingMessageNotificationParam::new(
                            LoggingLevel::Info,
                            serde_json::Value::String("hello from mock".into()),
                        )
                        .with_logger("mock-mcp-child"),
                    )
                    .await;

                // Resource updated
                let _ = peer
                    .notify_resource_updated(ResourceUpdatedNotificationParam::new(
                        "mock://resources/0",
                    ))
                    .await;

                // Resource / tool / prompt list changes
                let _ = peer.notify_resource_list_changed().await;
                let _ = peer.notify_tool_list_changed().await;
                let _ = peer.notify_prompt_list_changed().await;

                // Cancellation
                let _ = peer
                    .notify_cancelled(CancelledNotificationParam::new(
                        Some(NumberOrString::String("req-1".into())),
                        Some("mock cancel".into()),
                    ))
                    .await;

                Ok(CallToolResult::success(vec![ContentBlock::text(
                    "notifications-sent",
                )]))
            }

            "trigger_server_requests" => {
                let peer = context.peer.clone();

                // Ask the client for sampling
                let _ = peer
                    .create_message(CreateMessageRequestParams::new(vec![], 16))
                    .await;

                // Ask the client for roots
                let _ = peer.list_roots().await;

                // Ask the client for elicitation
                let _ = peer
                    .create_elicitation(ElicitRequestParams::FormElicitationParams {
                        meta: None,
                        message: "mock elicitation".into(),
                        requested_schema: ElicitationSchema::new(BTreeMap::new()),
                    })
                    .await;

                Ok(CallToolResult::success(vec![ContentBlock::text(
                    "server-requests-done",
                )]))
            }

            other => Err(ErrorData::new(
                ErrorCode::INVALID_PARAMS,
                format!("unknown tool: {other}"),
                None,
            )),
        }
    }

    // -- Resources ----------------------------------------------------------

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let resource = Resource::new("mock://resources/0", "mock-resource")
            .with_title("Mock Resource")
            .with_description("A fake resource for proxy tests")
            .with_mime_type("text/plain");
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let template = ResourceTemplate::new("mock://resources/{id}", "mock-template")
            .with_title("Mock Resource Template")
            .with_description("Parameterized mock resource")
            .with_mime_type("text/plain");
        Ok(ListResourceTemplatesResult::with_all_items(vec![template]))
    }

    async fn read_resource(
        &self,
        _request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        Ok(ReadResourceResult::new(vec![
            ResourceContents::TextResourceContents {
                uri: "mock://resources/0".into(),
                mime_type: Some("text/plain".into()),
                text: "mock content".into(),
                meta: None,
            },
        ]))
    }

    async fn subscribe(
        &self,
        _request: SubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }

    async fn unsubscribe(
        &self,
        _request: UnsubscribeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }

    // -- Prompts ------------------------------------------------------------

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult::with_all_items(vec![Prompt::new(
            "mock-prompt",
            Some("A fake prompt for proxy tests"),
            None,
        )]))
    }

    async fn get_prompt(
        &self,
        _request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        let mut result =
            GetPromptResult::new(vec![PromptMessage::new_text(Role::User, "hello from mock")]);
        result.description = Some("rendered mock prompt".into());
        Ok(result)
    }

    // -- Completion ---------------------------------------------------------

    async fn complete(
        &self,
        _request: CompleteRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CompleteResult, ErrorData> {
        let info = CompletionInfo::new(vec!["mock-completion".into()]).map_err(|e| {
            ErrorData::new(ErrorCode::INTERNAL_ERROR, format!("completion: {e}"), None)
        })?;
        Ok(CompleteResult::new(info))
    }

    // -- Logging ------------------------------------------------------------

    async fn set_level(
        &self,
        _request: SetLevelRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = MockServer
        .serve((tokio::io::stdin(), tokio::io::stdout()))
        .await?;
    service.waiting().await?;
    Ok(())
}
