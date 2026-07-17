//! TianQuan 16 SOUL subagent 接入 IronClaw Reborn。
//!
//! 实现 SubagentDefinitionResolver + SubagentPromptMaterialSource 两个 trait,
//! 把 16 SOUL(novelist/worldsmith 等)作为 Reborn subagent 的 direction persona 注入。
//!
//! 对齐 architecture.md §14:IronClaw agent 编排(外壳)派子 agent 时按 tier:role
//! 加载对应 SOUL persona,子 agent 经 MCP 调天权左脑引擎(底座)。
//!
//! 改动点(ironclaw 源码):
//! - reborn_composition/runtime.rs:3743 StaticSubagentDefinitionResolver → TianquanSubagentDefinitionResolver
//! - ironclaw_runner/runtime.rs:664 GateBackedSubagentPromptMaterialSource → TianquanSubagentPromptMaterialSource

pub mod directions;
pub mod flavors;

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::CapabilityId;
use ironclaw_loop_host::{
    SubagentDefinition, SubagentDefinitionResolver, SubagentKindId, SubagentPromptGoal,
    SubagentPromptMaterial, SubagentPromptMaterialSource, SubagentThreadKind,
    SubagentThreadMetadata,
};
use ironclaw_runner::planned_driver_factory::SUBAGENT_PLANNED_PROFILE_ID;
use ironclaw_runner::subagent::goal_store::{SubagentGoalStore, SubagentGoalStoreError};
use ironclaw_threads::{SessionThreadService, ThreadHistoryRequest, ThreadScope};
use ironclaw_turns::{
    RunProfileRequest,
    run_profile::{AgentLoopHostError, AgentLoopHostErrorKind, LoopRunContext},
};

use flavors::{allowed_capabilities_for, is_tianquan_kind, lookup_soul_flavor};

// ---------------------------------------------------------------------------
// TianquanSubagentDefinitionResolver
// ---------------------------------------------------------------------------

/// 天权 16 SOUL 的 subagent definition resolver。
///
/// 替换 ironclaw 默认的 StaticSubagentDefinitionResolver(只认 4 内置 flavor),
/// 让 Reborn 识别 16 天权 SOUL kind,各自带 allow_nesting + planned run profile。
///
/// 注:ironclaw 内置 4 flavor(general/explorer/coder/planner)不在天权 resolver 范围,
/// 若需保留内置 flavor,可用复合 resolver(先查天权,miss 再查内置)。本轮天权优先。
#[derive(Default)]
pub struct TianquanSubagentDefinitionResolver;

impl TianquanSubagentDefinitionResolver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SubagentDefinitionResolver for TianquanSubagentDefinitionResolver {
    async fn resolve_kind(
        &self,
        kind: &SubagentKindId,
    ) -> Result<Option<SubagentDefinition>, AgentLoopHostError> {
        let kind_str = kind.as_str();
        let Some(flavor) = lookup_soul_flavor(kind_str) else {
            return Ok(None);
        };
        let run_profile = RunProfileRequest::new(SUBAGENT_PLANNED_PROFILE_ID).map_err(|reason| {
            AgentLoopHostError::new(AgentLoopHostErrorKind::Internal, reason)
        })?;
        Ok(Some(SubagentDefinition {
            subagent_kind: kind.clone(),
            allow_nesting: flavor.allow_nesting,
            requested_run_profile: run_profile,
        }))
    }
}

// ---------------------------------------------------------------------------
// TianquanSubagentPromptMaterialSource
// ---------------------------------------------------------------------------

/// 天权 16 SOUL 的 prompt material source。
///
/// 替换 ironclaw 默认的 GateBackedSubagentPromptMaterialSource(用 direction_prompt
/// 取 4 内置 flavor persona)。天权版从 thread metadata 取 subagent_kind → 查 16 SOUL
/// direction_prompt + 天权工具白名单。
///
/// goal 取数逻辑对齐 GateBackedSubagentPromptMaterialSource(优先 goal_store,
/// fallback thread history),但 prompt material 用天权 SOUL(非 ironclaw 内置 direction)。
pub struct TianquanSubagentPromptMaterialSource<G>
where
    G: SubagentGoalStore + ?Sized,
{
    goal_store: Arc<G>,
    thread_service: Arc<dyn SessionThreadService>,
}

impl<G> TianquanSubagentPromptMaterialSource<G>
where
    G: SubagentGoalStore + ?Sized,
{
    pub fn new(goal_store: Arc<G>, thread_service: Arc<dyn SessionThreadService>) -> Self {
        Self {
            goal_store,
            thread_service,
        }
    }
}

#[async_trait]
impl<G> SubagentPromptMaterialSource for TianquanSubagentPromptMaterialSource<G>
where
    G: SubagentGoalStore + Send + Sync + ?Sized,
{
    async fn material_for_run(
        &self,
        run_context: &LoopRunContext,
    ) -> Result<SubagentPromptMaterial, AgentLoopHostError> {
        // 1. 从 thread metadata 取 subagent_kind(spawn 时记录)
        let kind_str = thread_subagent_kind_for_run(self.thread_service.as_ref(), run_context)
            .await?
            .ok_or_else(|| {
                AgentLoopHostError::new(
                    AgentLoopHostErrorKind::InvalidInvocation,
                    "subagent run has no recorded flavor (tianquan)",
                )
            })?;

        if !is_tianquan_kind(&kind_str) {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                format!("subagent run recorded a non-tianquan flavor: {kind_str}"),
            ));
        }

        // 2. 取 goal(task + handoff)
        let goal = goal_for_run(self.goal_store.as_ref(), Some(self.thread_service.as_ref()), run_context)
            .await?;

        // 3. 取 SOUL direction prompt(persona 正文)
        let direction_markdown = directions::direction_prompt_for_kind(&kind_str)
            .ok_or_else(|| {
                AgentLoopHostError::new(
                    AgentLoopHostErrorKind::Invalid,
                    format!("unknown tianquan soul direction for kind: {kind_str}"),
                )
            })?
            .to_string();

        // 4. 取工具白名单
        let allowed_capabilities: BTreeSet<CapabilityId> = allowed_capabilities_for(&kind_str)
            .map_err(|e| {
                AgentLoopHostError::new(AgentLoopHostErrorKind::Invalid, e)
            })?;

        Ok(SubagentPromptMaterial {
            direction_markdown,
            goal,
            allowed_capabilities,
        })
    }
}

// ---------------------------------------------------------------------------
// 辅助:从 thread metadata 取 subagent_kind + goal 取数
// (对齐 ironclaw_runner::subagent::prompt_material 的私有实现,用 pub API 重写)
// ---------------------------------------------------------------------------

/// 从 thread metadata 取 subagent_kind(对齐 thread_metadata_for_run)。
async fn thread_subagent_kind_for_run(
    thread_service: &dyn SessionThreadService,
    run_context: &LoopRunContext,
) -> Result<Option<String>, AgentLoopHostError> {
    let thread_scope = thread_scope_for_run(run_context)?;
    let thread = thread_service
        .read_thread(ThreadHistoryRequest {
            scope: thread_scope,
            thread_id: run_context.thread_id.clone(),
        })
        .await
        .map_err(|error| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                format!("subagent thread metadata unavailable: {error}"),
            )
        })?;
    let kind = thread
        .metadata_json
        .as_deref()
        .and_then(parse_subagent_thread_metadata)
        .map(|metadata| metadata.subagent_kind.into_inner());
    Ok(kind)
}

fn parse_subagent_thread_metadata(raw: &str) -> Option<SubagentThreadMetadata> {
    serde_json::from_str::<SubagentThreadMetadata>(raw)
        .ok()
        .filter(|metadata| metadata.kind == SubagentThreadKind::Subagent)
}

fn thread_scope_for_run(run_context: &LoopRunContext) -> Result<ThreadScope, AgentLoopHostError> {
    let agent_id = run_context.scope.agent_id.clone().ok_or_else(|| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::InvalidInvocation,
            "subagent run scope is missing agent id",
        )
    })?;
    Ok(ThreadScope {
        tenant_id: run_context.scope.tenant_id.clone(),
        agent_id,
        project_id: run_context.scope.project_id.clone(),
        owner_user_id: run_context
            .actor
            .as_ref()
            .map(|actor| actor.user_id.clone()),
        mission_id: None,
    })
}

/// 取 goal(对齐 goal_for_run:优先 goal_store,fallback thread history)。
async fn goal_for_run<G>(
    goal_store: &G,
    thread_service: Option<&dyn SessionThreadService>,
    run_context: &LoopRunContext,
) -> Result<SubagentPromptGoal, AgentLoopHostError>
where
    G: SubagentGoalStore + Send + Sync + ?Sized,
{
    match goal_store
        .get_goal(&run_context.scope, run_context.run_id)
        .await
    {
        Ok(goal) => Ok(SubagentPromptGoal {
            task: goal.task,
            handoff: goal.handoff,
        }),
        Err(SubagentGoalStoreError::NotFound { .. }) => {
            let Some(thread_service) = thread_service else {
                return Err(map_goal_error(SubagentGoalStoreError::NotFound {
                    run_id: run_context.run_id,
                }));
            };
            goal_from_thread(thread_service, run_context).await
        }
        Err(error) => Err(map_goal_error(error)),
    }
}

/// 从 thread history 取 goal(fallback,对齐 ironclaw goal_from_thread 简化版)。
async fn goal_from_thread(
    thread_service: &dyn SessionThreadService,
    run_context: &LoopRunContext,
) -> Result<SubagentPromptGoal, AgentLoopHostError> {
    let thread_scope = thread_scope_for_run(run_context)?;
    let history = thread_service
        .list_thread_history(ThreadHistoryRequest {
            scope: thread_scope,
            thread_id: run_context.thread_id.clone(),
        })
        .await
        .map_err(|error| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                format!("subagent thread history unavailable: {error}"),
            )
        })?;
    // 取首条消息内容作 task(对齐 ironclaw 的 child_initial_message 逻辑)
    let task = history
        .messages
        .first()
        .map(|m| m.content.clone().unwrap_or_default())
        .unwrap_or_default();
    Ok(SubagentPromptGoal {
        task,
        handoff: None,
    })
}

fn map_goal_error(error: SubagentGoalStoreError) -> AgentLoopHostError {
    AgentLoopHostError::new(
        AgentLoopHostErrorKind::Unavailable,
        format!("subagent goal store error: {error}"),
    )
}
