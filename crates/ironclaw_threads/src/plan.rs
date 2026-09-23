//! 线程任务计划及其持久化领域事件。
//!
//! 每次替换在同一条 CAS 记录中保存完整 `plan_update` 事件。该事件就是当前
//! 快照，重连通过 revision 读取最新事件，不依赖进程内通知或工具调用计数。
//! 这里保存用户可见任务文本，禁止将其放进只允许元数据的 RuntimeEvent 流。

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_filesystem::{
    CasApply, CasUpdateError, Entry, RecordKind, RootFilesystem, ScopedFilesystem, cas_update,
};
use ironclaw_host_api::{ResourceScope, RunId, ScopedPath, ThreadId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const THREAD_PLAN_MAX_STEPS: usize = 100;
pub const THREAD_PLAN_MAX_TITLE_CHARS: usize = 160;
pub const THREAD_PLAN_MAX_STEP_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadPlanStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadPlanStep {
    pub index: u32,
    pub title: String,
    pub status: ThreadPlanStatus,
}

/// 模型输入不接受 scope 或 thread_id；它们由已授权的调用上下文提供。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadPlanWrite {
    pub title: String,
    pub steps: Vec<ThreadPlanStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadPlanEventKind {
    PlanUpdate,
}

/// 最后一次成功持久化的完整计划事件。空 steps 明确表示已清空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPlanUpdate {
    pub kind: ThreadPlanEventKind,
    pub thread_id: ThreadId,
    pub title: String,
    pub steps: Vec<ThreadPlanStep>,
    pub revision: u64,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ThreadPlanError {
    #[error("task plan requires a thread")]
    MissingThread,
    #[error("invalid task plan input")]
    InvalidInput,
    #[error("task plan revision changed; read the current plan before retrying")]
    RevisionConflict,
    #[error("task plan storage unavailable")]
    StorageUnavailable,
    #[error("task plan stored record is invalid")]
    InvalidRecord,
    #[error("task plan write could not be verified")]
    UnverifiedWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredThreadPlan {
    scope_key: String,
    event: ThreadPlanUpdate,
}

/// 领域存储只接受已组合的 /threads mount，不选择存储后端。
pub struct FilesystemThreadPlanStore<F: RootFilesystem + ?Sized> {
    filesystem: Arc<ScopedFilesystem<F>>,
}

/// 产品读取端只获得此端口；修改权只提供给已授权的任务能力。
#[async_trait]
pub trait ThreadPlanReader: Send + Sync {
    async fn read(
        &self,
        scope: &ResourceScope,
    ) -> Result<Option<ThreadPlanUpdate>, ThreadPlanError>;
}

#[async_trait]
impl<F: RootFilesystem + ?Sized> ThreadPlanReader for FilesystemThreadPlanStore<F> {
    async fn read(
        &self,
        scope: &ResourceScope,
    ) -> Result<Option<ThreadPlanUpdate>, ThreadPlanError> {
        FilesystemThreadPlanStore::read(self, scope).await
    }
}

impl<F: RootFilesystem + ?Sized> FilesystemThreadPlanStore<F> {
    pub fn new(filesystem: Arc<ScopedFilesystem<F>>) -> Self {
        Self { filesystem }
    }

    pub async fn read(
        &self,
        scope: &ResourceScope,
    ) -> Result<Option<ThreadPlanUpdate>, ThreadPlanError> {
        let (key, path) = plan_location(scope)?;
        let entry = self
            .filesystem
            .get(scope, &path)
            .await
            .map_err(|_| ThreadPlanError::StorageUnavailable)?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        let stored = decode(&entry.entry.body)?;
        validate_stored(&stored, &key, scope)?;
        Ok(Some(stored.event))
    }

    pub async fn update(
        &self,
        scope: &ResourceScope,
        input: ThreadPlanWrite,
        run_id: Option<RunId>,
    ) -> Result<ThreadPlanUpdate, ThreadPlanError> {
        validate_input(&input)?;
        let (key, path) = plan_location(scope)?;
        let thread_id = scope
            .thread_id
            .clone()
            .ok_or(ThreadPlanError::MissingThread)?;
        let event = cas_update(
            self.filesystem.as_ref(),
            scope,
            &path,
            decode,
            |record: &StoredThreadPlan| {
                Entry::record(
                    RecordKind::new("thread_plan_update")
                        .map_err(|_| ThreadPlanError::InvalidRecord)?,
                    &serde_json::to_value(record).map_err(|_| ThreadPlanError::InvalidRecord)?,
                )
                .map_err(|_| ThreadPlanError::InvalidRecord)
            },
            |current: Option<StoredThreadPlan>| {
                let input = input.clone();
                let key = key.clone();
                let thread_id = thread_id.clone();
                async move {
                    if let Some(ref stored) = current {
                        validate_stored(stored, &key, scope)?;
                    }
                    let revision = current.as_ref().map_or(0, |record| record.event.revision);
                    if input
                        .expected_revision
                        .is_some_and(|expected| expected != revision)
                    {
                        return Err(ThreadPlanError::RevisionConflict);
                    }
                    let event = ThreadPlanUpdate {
                        kind: ThreadPlanEventKind::PlanUpdate,
                        thread_id,
                        title: input.title,
                        steps: input.steps,
                        revision: revision
                            .checked_add(1)
                            .ok_or(ThreadPlanError::InvalidRecord)?,
                        updated_at: Utc::now(),
                        run_id,
                    };
                    Ok(CasApply::new(
                        StoredThreadPlan {
                            scope_key: key,
                            event: event.clone(),
                        },
                        event,
                    ))
                }
            },
        )
        .await
        .map_err(|error| match error {
            CasUpdateError::Apply(error) => error,
            _ => ThreadPlanError::StorageUnavailable,
        })?;
        // 后续并发更新可以合法覆盖当前快照；同 revision 必须逐值一致，较新 revision
        // 则证明此写入已进入单调 CAS 序列。读取失败明确报未验证，不冒充成功。
        let read_back = self
            .read(scope)
            .await
            .map_err(|_| ThreadPlanError::UnverifiedWrite)?;
        match read_back {
            Some(current) if current == event || current.revision > event.revision => Ok(event),
            _ => Err(ThreadPlanError::UnverifiedWrite),
        }
    }
}

fn validate_input(input: &ThreadPlanWrite) -> Result<(), ThreadPlanError> {
    let valid_text = |value: &str, limit: usize| {
        value.chars().count() <= limit && !value.chars().any(char::is_control)
    };
    if !valid_text(&input.title, THREAD_PLAN_MAX_TITLE_CHARS)
        || input.steps.len() > THREAD_PLAN_MAX_STEPS
        || (!input.steps.is_empty() && input.title.trim().is_empty())
    {
        return Err(ThreadPlanError::InvalidInput);
    }
    let mut indexes = HashSet::new();
    for step in &input.steps {
        if step.title.trim().is_empty()
            || !valid_text(&step.title, THREAD_PLAN_MAX_STEP_CHARS)
            || !indexes.insert(step.index)
        {
            return Err(ThreadPlanError::InvalidInput);
        }
    }
    Ok(())
}

fn decode(body: &[u8]) -> Result<StoredThreadPlan, ThreadPlanError> {
    if body.len() > 256 * 1024 {
        return Err(ThreadPlanError::InvalidRecord);
    }
    serde_json::from_slice(body).map_err(|_| ThreadPlanError::InvalidRecord)
}

fn validate_stored(
    stored: &StoredThreadPlan,
    key: &str,
    scope: &ResourceScope,
) -> Result<(), ThreadPlanError> {
    if stored.scope_key != key
        || Some(&stored.event.thread_id) != scope.thread_id.as_ref()
        || stored.event.revision == 0
    {
        return Err(ThreadPlanError::InvalidRecord);
    }
    validate_input(&ThreadPlanWrite {
        title: stored.event.title.clone(),
        steps: stored.event.steps.clone(),
        expected_revision: None,
    })
    .map_err(|_| ThreadPlanError::InvalidRecord)
}

fn plan_location(scope: &ResourceScope) -> Result<(String, ScopedPath), ThreadPlanError> {
    let thread_id = scope
        .thread_id
        .as_ref()
        .ok_or(ThreadPlanError::MissingThread)?;
    // 不包含 invocation_id：同一线程后续 turn 恢复同一份计划。所有其余 scope
    // 维度均参与键，即使底层 mount 共享，也不能串租户、用户、项目或子线程。
    let identity = serde_json::to_vec(&(
        &scope.tenant_id,
        &scope.user_id,
        &scope.agent_id,
        &scope.project_id,
        &scope.mission_id,
        thread_id,
    ))
    .map_err(|_| ThreadPlanError::InvalidInput)?;
    let key: String = Sha256::digest(identity)
        .iter()
        .flat_map(|byte| {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            [
                char::from(HEX[usize::from(byte >> 4)]),
                char::from(HEX[usize::from(byte & 15)]),
            ]
        })
        .collect();
    let path = ScopedPath::new(format!("/threads/plans/{key}.json"))
        .map_err(|_| ThreadPlanError::InvalidInput)?;
    Ok((key, path))
}
