use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::{
    DispatchError, ExtensionId, ResourceEstimate, RunId, RuntimeCredentialAccountProviderId,
    RuntimeCredentialAuthRequirement, RuntimeKind, UserId,
};
use ironclaw_mcp::{
    McpClient, McpError, McpExecutionRequest, McpExecutionResult, McpExecutor, McpHostHttp,
    McpHostHttpClient, McpHostHttpEgressPlan, McpHostHttpError, McpHostHttpResponse, McpRuntime,
    McpRuntimeConfig, McpTrustedExecutionContext, StaticMcpHostHttpEgressPlanner,
};
use serde_json::json;

use super::*;

#[tokio::test]
async fn mcp_adapter_maps_executor_auth_required_to_dispatch_auth_required() {
    let requirement = RuntimeCredentialAuthRequirement {
        provider: RuntimeCredentialAccountProviderId::new("github").unwrap(),
        setup: ironclaw_host_api::RuntimeCredentialAccountSetup::OAuth {
            scopes: vec!["repo".to_string()],
        },
        requester_extension: ExtensionId::new("mcp").unwrap(),
        provider_scopes: vec!["repo".to_string()],
    };
    let adapter = McpRuntimeAdapter::from_executor(Arc::new(AuthRequiredMcpExecutor {
        requirement: requirement.clone(),
    }));
    let descriptor = test_descriptor(RuntimeKind::Mcp, Vec::new());
    let filesystem = DiskFilesystem::new();
    let governor = InMemoryResourceGovernor::new();
    let package = test_package(MCP_MANIFEST, "test");
    let policy = policy_with(
        FilesystemBackendKind::HostWorkspace,
        ProcessBackendKind::LocalHost,
        NetworkMode::DirectLogged,
        SecretMode::ScrubbedEnv,
    );

    let result = adapter
        .dispatch_json(RuntimeAdapterRequest {
            run_id: None,
            package: &package,
            descriptor: &descriptor,
            filesystem: &filesystem,
            governor: &governor,
            runtime_policy: &policy,
            capability_id: &descriptor.id,
            scope: sample_scope(),
            authenticated_actor_user_id: None,
            estimate: ResourceEstimate::default(),
            mounts: None,
            resource_reservation: None,
            input: json!({"query": "auth through adapter"}),
        })
        .await;

    match result {
        Err(DispatchError::AuthRequired {
            capability,
            required_secrets,
            credential_requirements,
        }) => {
            assert_eq!(capability, descriptor.id);
            assert!(required_secrets.is_empty());
            assert_eq!(credential_requirements, vec![requirement]);
        }
        other => panic!("expected AuthRequired, got {other:?}"),
    }
}

#[tokio::test]
async fn mcp_adapter_forwards_host_identity_outside_model_arguments() {
    let capture = Arc::new(CapturingMcpExecutor::default());
    let adapter = McpRuntimeAdapter::from_executor(capture.clone());
    let descriptor = test_descriptor(RuntimeKind::Mcp, Vec::new());
    let filesystem = DiskFilesystem::new();
    let governor = InMemoryResourceGovernor::new();
    let package = test_package(MCP_MANIFEST, "test");
    let policy = policy_with(
        FilesystemBackendKind::HostWorkspace,
        ProcessBackendKind::LocalHost,
        NetworkMode::DirectLogged,
        SecretMode::ScrubbedEnv,
    );
    let run_id = RunId::new();
    let actor_user_id = UserId::new("trusted-user").unwrap();
    let model_input = json!({
        "query":"查世界状态",
        "runId":"model-forged-run",
        "authenticatedActorUserId":"model-forged-user"
    });

    let result = adapter
        .dispatch_json(RuntimeAdapterRequest {
            package: &package,
            descriptor: &descriptor,
            filesystem: &filesystem,
            governor: &governor,
            runtime_policy: &policy,
            capability_id: &descriptor.id,
            scope: sample_scope(),
            authenticated_actor_user_id: Some(actor_user_id.clone()),
            run_id: Some(run_id),
            estimate: ResourceEstimate::default(),
            mounts: None,
            resource_reservation: None,
            input: model_input.clone(),
        })
        .await;

    assert!(matches!(result, Err(DispatchError::AuthRequired { .. })));
    assert_eq!(
        capture.trusted_contexts.lock().unwrap().as_slice(),
        &[Some(McpTrustedExecutionContext {
            authenticated_actor_user_id: Some(actor_user_id),
            run_id: Some(run_id),
        })]
    );
    assert_eq!(
        capture.model_inputs.lock().unwrap().as_slice(),
        std::slice::from_ref(&model_input)
    );

    let non_loop_input = json!({"query":"non-loop call"});
    let non_loop_result = adapter
        .dispatch_json(RuntimeAdapterRequest {
            package: &package,
            descriptor: &descriptor,
            filesystem: &filesystem,
            governor: &governor,
            runtime_policy: &policy,
            capability_id: &descriptor.id,
            scope: sample_scope(),
            authenticated_actor_user_id: None,
            run_id: None,
            estimate: ResourceEstimate::default(),
            mounts: None,
            resource_reservation: None,
            input: non_loop_input.clone(),
        })
        .await;

    assert!(matches!(
        non_loop_result,
        Err(DispatchError::AuthRequired { .. })
    ));
    assert_eq!(
        capture.trusted_contexts.lock().unwrap().as_slice(),
        &[
            Some(McpTrustedExecutionContext {
                authenticated_actor_user_id: Some(UserId::new("trusted-user").unwrap()),
                run_id: Some(run_id),
            }),
            None,
        ]
    );
    assert_eq!(
        capture.model_inputs.lock().unwrap().as_slice(),
        &[model_input, non_loop_input]
    );
}

const MCP_MANIFEST: &str = r#"schema_version = "reborn.extension_manifest.v2"
id = "test"
name = "Test MCP"
version = "0.1.0"
description = "MCP adapter test extension"
trust = "third_party"

[runtime]
kind = "mcp"
transport = "http"
url = "https://mcp.example.test/rpc"

[[capabilities]]
id = "test.capability"
description = "Search through MCP"
effects = ["network"]
default_permission = "allow"
visibility = "model"
input_schema_ref = "schemas/test-mcp/search.input.v1.json"
output_schema_ref = "schemas/test-mcp/search.output.v1.json"
"#;

// 覆盖:mcp.md §5 MCP tool errors / ISSUE-IRONCLAW-047：真实协议解析→结算→adapter。
#[tokio::test]
async fn mcp_adapter_tool_error_flag_preserves_failure_and_accounting() {
    for (flag, expected) in [
        (
            Some(json!(true)),
            Some(RuntimeDispatchErrorKind::OperationFailed),
        ),
        (
            Some(json!("true")),
            Some(RuntimeDispatchErrorKind::InvalidResult),
        ),
        (
            Some(json!(null)),
            Some(RuntimeDispatchErrorKind::InvalidResult),
        ),
        (Some(json!(false)), None),
        (None, None),
    ] {
        let mut output = json!({"content":[{"type":"text","text":"private-content-sentinel"}]});
        if let Some(flag) = flag {
            output["isError"] = flag;
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let receipts = Arc::new(Mutex::new(Vec::new()));
        let client = McpHostHttpClient::new(
            ToolResultHttp {
                output,
                calls: calls.clone(),
            },
            StaticMcpHostHttpEgressPlanner::new(McpHostHttpEgressPlan {
                network_policy: NetworkPolicy {
                    deny_redirects: true,
                    allowed_targets: Vec::new(),
                    deny_private_ip_ranges: true,
                    max_egress_bytes: Some(4096),
                },
                credential_injections: Vec::new(),
                response_body_limit: Some(4096),
                timeout_ms: Some(1000),
            }),
        );
        let adapter = McpRuntimeAdapter::from_executor(Arc::new(ReceiptObserver {
            runtime: McpRuntime::new(McpRuntimeConfig::for_testing(), client),
            receipts: receipts.clone(),
        }));
        let descriptor = test_descriptor(RuntimeKind::Mcp, Vec::new());
        let filesystem = DiskFilesystem::new();
        let governor = InMemoryResourceGovernor::new();
        let package = test_package(MCP_MANIFEST, "test");
        let policy = policy_with(
            FilesystemBackendKind::HostWorkspace,
            ProcessBackendKind::LocalHost,
            NetworkMode::DirectLogged,
            SecretMode::ScrubbedEnv,
        );
        let result = adapter
            .dispatch_json(RuntimeAdapterRequest {
                run_id: None,
                package: &package,
                descriptor: &descriptor,
                filesystem: &filesystem,
                governor: &governor,
                runtime_policy: &policy,
                capability_id: &descriptor.id,
                scope: sample_scope(),
                authenticated_actor_user_id: None,
                estimate: ResourceEstimate::default(),
                mounts: None,
                resource_reservation: None,
                input: json!({"query":"invalid tool input"}),
            })
            .await;
        match expected {
            Some(expected) => {
                let error = result.expect_err("tool errors cannot be successful dispatches");
                assert!(matches!(&error, DispatchError::Mcp { kind, .. } if *kind == expected));
                assert!(!format!("{error:?} {error}").contains("private-content-sentinel"));
            }
            None => {
                result.expect("missing or false isError remains successful");
            }
        }
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            &["initialize", "notifications/initialized", "tools/call"]
        );
        let receipts = receipts.lock().unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].status, ReservationStatus::Reconciled);
    }
}

struct ToolResultHttp {
    output: serde_json::Value,
    calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl McpHostHttp for ToolResultHttp {
    async fn request(
        &self,
        request: ironclaw_host_api::CapabilityHostHttpRequest,
    ) -> Result<McpHostHttpResponse, McpHostHttpError> {
        let rpc: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let method = rpc["method"].as_str().unwrap();
        self.calls.lock().unwrap().push(method.to_owned());
        let result = match method {
            "initialize" => json!({"protocolVersion":"2025-06-18","capabilities":{}}),
            "notifications/initialized" => serde_json::Value::Null,
            "tools/call" => self.output.clone(),
            _ => panic!("unexpected HTTP method"),
        };
        Ok(McpHostHttpResponse {
            status: if method == "notifications/initialized" {
                202
            } else {
                200
            },
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: serde_json::to_vec(&json!({"jsonrpc":"2.0","id":rpc["id"],"result":result}))
                .unwrap(),
            saved_body: None,
            request_bytes: request.body.len() as u64,
            response_bytes: 0,
            redaction_applied: false,
        })
    }
}

struct ReceiptObserver<C> {
    runtime: McpRuntime<C>,
    receipts: Arc<Mutex<Vec<ResourceReceipt>>>,
}

#[async_trait]
impl<C: McpClient> McpExecutor for ReceiptObserver<C> {
    async fn execute_extension_json(
        &self,
        governor: &dyn ResourceGovernor,
        request: McpExecutionRequest<'_>,
    ) -> Result<McpExecutionResult, McpError> {
        let result = self
            .runtime
            .execute_extension_json(governor, request)
            .await?;
        self.receipts.lock().unwrap().push(result.receipt.clone());
        Ok(result)
    }
}

struct AuthRequiredMcpExecutor {
    requirement: RuntimeCredentialAuthRequirement,
}

#[derive(Default)]
struct CapturingMcpExecutor {
    trusted_contexts: std::sync::Mutex<Vec<Option<McpTrustedExecutionContext>>>,
    model_inputs: std::sync::Mutex<Vec<serde_json::Value>>,
}

#[async_trait]
impl McpExecutor for CapturingMcpExecutor {
    async fn execute_extension_json(
        &self,
        _governor: &dyn ResourceGovernor,
        request: McpExecutionRequest<'_>,
    ) -> Result<McpExecutionResult, McpError> {
        self.trusted_contexts
            .lock()
            .unwrap()
            .push(request.trusted_context);
        self.model_inputs
            .lock()
            .unwrap()
            .push(request.invocation.input);
        Err(McpError::AuthRequired {
            required_secrets: Vec::new(),
            credential_requirements: Vec::new(),
        })
    }
}

#[async_trait]
impl McpExecutor for AuthRequiredMcpExecutor {
    async fn execute_extension_json(
        &self,
        _governor: &dyn ResourceGovernor,
        _request: McpExecutionRequest<'_>,
    ) -> Result<McpExecutionResult, McpError> {
        Err(McpError::AuthRequired {
            required_secrets: Vec::new(),
            credential_requirements: vec![self.requirement.clone()],
        })
    }
}
