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

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    model::{
        AnnotateAble, CallToolRequestParams, CallToolResult, CancelledNotificationParam,
        CompleteRequestParams, CompleteResult, CompletionInfo, Content,
        CreateElicitationRequestParams, CreateMessageRequestParams, ElicitationSchema, ErrorCode,
        GetPromptRequestParams, GetPromptResult, Implementation, InitializeRequestParams,
        InitializeResult, JsonObject, ListPromptsResult, ListResourceTemplatesResult,
        ListResourcesResult, ListToolsResult, LoggingLevel, LoggingMessageNotificationParam,
        NumberOrString, PaginatedRequestParams, ProgressNotificationParam, ProgressToken, Prompt,
        PromptMessage, PromptMessageRole, RawResource, RawResourceTemplate,
        ReadResourceRequestParams, ReadResourceResult, ResourceContents,
        ResourceUpdatedNotificationParam, ServerCapabilities, SetLevelRequestParams,
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

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            "echo" => Ok(CallToolResult::success(vec![Content::text(
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
                    .notify_logging_message(LoggingMessageNotificationParam {
                        level: LoggingLevel::Info,
                        logger: Some("mock-mcp-child".into()),
                        data: serde_json::Value::String("hello from mock".into()),
                    })
                    .await;

                // Resource updated
                let _ = peer
                    .notify_resource_updated(ResourceUpdatedNotificationParam {
                        uri: "mock://resources/0".into(),
                    })
                    .await;

                // Resource / tool / prompt list changes
                let _ = peer.notify_resource_list_changed().await;
                let _ = peer.notify_tool_list_changed().await;
                let _ = peer.notify_prompt_list_changed().await;

                // Cancellation
                let _ = peer
                    .notify_cancelled(CancelledNotificationParam {
                        request_id: NumberOrString::String("req-1".into()),
                        reason: Some("mock cancel".into()),
                    })
                    .await;

                Ok(CallToolResult::success(vec![Content::text(
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
                    .create_elicitation(CreateElicitationRequestParams::FormElicitationParams {
                        meta: None,
                        message: "mock elicitation".into(),
                        requested_schema: ElicitationSchema::new(BTreeMap::new()),
                    })
                    .await;

                Ok(CallToolResult::success(vec![Content::text(
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
        let resource = RawResource {
            uri: "mock://resources/0".into(),
            name: "mock-resource".into(),
            title: Some("Mock Resource".into()),
            description: Some("A fake resource for proxy tests".into()),
            mime_type: Some("text/plain".into()),
            size: None,
            icons: None,
            meta: None,
        }
        .no_annotation();
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        let template = RawResourceTemplate {
            uri_template: "mock://resources/{id}".into(),
            name: "mock-template".into(),
            title: Some("Mock Resource Template".into()),
            description: Some("Parameterized mock resource".into()),
            mime_type: Some("text/plain".into()),
            icons: None,
        }
        .no_annotation();
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
        let mut result = GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            "hello from mock",
        )]);
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
