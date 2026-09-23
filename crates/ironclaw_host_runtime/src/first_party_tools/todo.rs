//! 任务能力仅适配线程计划领域存储；不从工具事件或模型文本推断任务。

use std::{sync::Arc, time::Instant};

use async_trait::async_trait;
use ironclaw_extensions::{CapabilityManifest, ExtensionError};
use ironclaw_filesystem::RootFilesystem;
use ironclaw_host_api::{
    CapabilityId, EffectKind, HostApiError, PermissionMode, ResourceUsage, RuntimeDispatchErrorKind,
};
use ironclaw_threads::plan::{FilesystemThreadPlanStore, ThreadPlanError, ThreadPlanWrite};
use serde_json::json;

use super::{
    FIRST_PARTY_MAX_OUTPUT_BYTES, bounded_input_size, bounded_output_bytes,
    first_party_capability_manifest, resource_profile,
};
use crate::{
    FirstPartyCapabilityError, FirstPartyCapabilityHandler, FirstPartyCapabilityRegistry,
    FirstPartyCapabilityRequest, FirstPartyCapabilityResult,
};

pub const TODO_READ_CAPABILITY_ID: &str = "builtin.todo_read";
pub const TODO_WRITE_CAPABILITY_ID: &str = "builtin.todo_write";

pub(super) fn manifests() -> Result<Vec<CapabilityManifest>, ExtensionError> {
    Ok(vec![
        first_party_capability_manifest(
            TODO_READ_CAPABILITY_ID,
            "Read the actual task plan for this conversation. Returns null if no plan was created. Plans are isolated by conversation and survive later turns and restarts.",
            vec![EffectKind::ReadFilesystem],
            PermissionMode::Allow,
            resource_profile(),
        )?,
        first_party_capability_manifest(
            TODO_WRITE_CAPABILITY_ID,
            "Create or replace the actual task plan for this conversation. Supply the full title and steps on every update; an empty steps array clears it. Read first and pass expected_revision to avoid overwriting a concurrent update. Only record actual tasks and real completion; tool call counts are not task progress.",
            vec![EffectKind::ReadFilesystem, EffectKind::WriteFilesystem],
            PermissionMode::Allow,
            resource_profile(),
        )?,
    ])
}

/// 由 composition 注入唯一持久化 store；不向模型暴露内部 mount 或其他线程。
/// 调用者：Reborn factory；约束测试：first_party_builtin_tools::todo_contract。
pub struct ThreadPlanTools<F: RootFilesystem + ?Sized> {
    store: Arc<FilesystemThreadPlanStore<F>>,
}

impl<F: RootFilesystem + ?Sized + 'static> ThreadPlanTools<F> {
    pub fn new(store: Arc<FilesystemThreadPlanStore<F>>) -> Self {
        Self { store }
    }

    pub fn insert_into(
        self,
        registry: &mut FirstPartyCapabilityRegistry,
    ) -> Result<(), HostApiError> {
        let handler = Arc::new(self);
        registry.insert_handler(CapabilityId::new(TODO_READ_CAPABILITY_ID)?, handler.clone());
        registry.insert_handler(CapabilityId::new(TODO_WRITE_CAPABILITY_ID)?, handler);
        Ok(())
    }
}

#[async_trait]
impl<F: RootFilesystem + ?Sized + 'static> FirstPartyCapabilityHandler for ThreadPlanTools<F> {
    async fn dispatch(
        &self,
        request: FirstPartyCapabilityRequest,
    ) -> Result<FirstPartyCapabilityResult, FirstPartyCapabilityError> {
        bounded_input_size(request.capability_id.as_str(), &request.input)?;
        // CapabilityHost 已验证可信 subject scope；actor 可以是获得委派权的
        // 另一主体。这里不重定义授权规则，只拒绝模型输入携带 scope/thread_id。
        let start = Instant::now();
        let output = match request.capability_id.as_str() {
            TODO_READ_CAPABILITY_ID => {
                if !request
                    .input
                    .as_object()
                    .is_some_and(|object| object.is_empty())
                {
                    return Err(plan_error(ThreadPlanError::InvalidInput));
                }
                json!(self.store.read(&request.scope).await.map_err(plan_error)?)
            }
            TODO_WRITE_CAPABILITY_ID => {
                let input: ThreadPlanWrite = serde_json::from_value(request.input)
                    .map_err(|_| plan_error(ThreadPlanError::InvalidInput))?;
                json!(
                    self.store
                        .update(&request.scope, input, request.run_id)
                        .await
                        .map_err(plan_error)?
                )
            }
            _ => {
                return Err(FirstPartyCapabilityError::new(
                    RuntimeDispatchErrorKind::UnsupportedRunner,
                ));
            }
        };
        let output_bytes = bounded_output_bytes(&output, FIRST_PARTY_MAX_OUTPUT_BYTES)?;
        Ok(FirstPartyCapabilityResult::new(
            output,
            ResourceUsage::default()
                .set_output_bytes(output_bytes)
                .set_wall_clock_ms(start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        ))
    }
}

fn plan_error(error: ThreadPlanError) -> FirstPartyCapabilityError {
    let (kind, summary) = match error {
        ThreadPlanError::MissingThread => (
            RuntimeDispatchErrorKind::InputEncode,
            "Task plan requires an active conversation.",
        ),
        ThreadPlanError::InvalidInput => (
            RuntimeDispatchErrorKind::InputEncode,
            "Invalid task plan. Use a title and unique steps with supported statuses.",
        ),
        ThreadPlanError::RevisionConflict => (
            RuntimeDispatchErrorKind::InputEncode,
            "Task plan changed. Read the current plan and retry with its revision.",
        ),
        ThreadPlanError::UnverifiedWrite => (
            RuntimeDispatchErrorKind::Backend,
            "Task plan write could not be verified. Read the current plan before retrying.",
        ),
        ThreadPlanError::InvalidRecord | ThreadPlanError::StorageUnavailable => (
            RuntimeDispatchErrorKind::Backend,
            "Task plan could not be read or saved. Retry when storage is available.",
        ),
    };
    FirstPartyCapabilityError::with_safe_summary(kind, summary)
}
