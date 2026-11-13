//! 中立宿主入料适配器：后端仅由可信composition注入，不消费model参数。
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ThreadId;
use ironclaw_turns::{
    TurnRunId, TurnScope,
    run_profile::{AgentLoopHostError, AgentLoopHostErrorKind, LoopRunContext},
};
use sha2::{Digest, Sha256};

use crate::subagent_prompt_port::{SubagentPromptMaterial, SubagentPromptMaterialSource};

/// 无Deserialize且构造器私有；不能把后端record或provider DTO当receipt。
#[derive(Clone)]
pub struct OpaquePromptInputReceipt {
    binding_ref: String,
    material_ref: String,
    content_hash: String,
    source_version: String,
}

impl OpaquePromptInputReceipt {
    fn from_checked_record(record: &OpaquePromptInputRecord) -> Result<Self, AgentLoopHostError> {
        if record.binding_ref.is_empty()
            || record.material_ref.is_empty()
            || record.source_version.is_empty()
            || record.content_hash.len() != 64
            || !record.content_hash.bytes().all(|byte| {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            })
        {
            return Err(invalid("opaque input receipt invalid"));
        }
        Ok(Self {
            binding_ref: record.binding_ref.clone(),
            material_ref: record.material_ref.clone(),
            content_hash: record.content_hash.clone(),
            source_version: record.source_version.clone(),
        })
    }

    pub fn binding_ref(&self) -> &str { &self.binding_ref }
    pub fn material_ref(&self) -> &str { &self.material_ref }
    pub fn content_hash(&self) -> &str { &self.content_hash }
    pub fn source_version(&self) -> &str { &self.source_version }
}

/// 创建者必须是已获授权的spawn owner，child id来自实际requested_run_id。
pub struct HostSpawnInputFacts<'a> {
    pub parent_context: &'a LoopRunContext,
    pub child_run_id: TurnRunId,
    pub child_thread_id: &'a ThreadId,
    pub registered_kind: &'a str,
}

pub struct OpaquePromptInputMaterial {
    pub receipt: OpaquePromptInputReceipt,
    pub body: String,
    pub charged_bytes: usize,
}

#[async_trait]
pub trait SubagentOpaqueInputPort: Send + Sync {
    async fn bind_before_submit(
        &self,
        facts: HostSpawnInputFacts<'_>,
    ) -> Result<Option<OpaquePromptInputReceipt>, AgentLoopHostError>;

    async fn material_for_child(
        &self,
        child: &LoopRunContext,
    ) -> Result<Option<OpaquePromptInputMaterial>, AgentLoopHostError>;
}

/// 仅后端明确判定无需入料才可转成None；错误、缺失、未知必须Err。
pub enum OpaquePromptInputOutcome<T> {
    NotRequired,
    Required(T),
}

/// 泛型存储记录，不授予receipt资格；父上下文保留实际run/turn/thread/scope。
/// 不定义任何领域schema、recipe或领域资产DTO。
pub struct OpaquePromptInputRecord {
    pub parent_context: LoopRunContext,
    pub child_run_id: TurnRunId,
    pub child_thread_id: ThreadId,
    pub child_scope: TurnScope,
    pub registered_kind: String,
    pub binding_ref: String,
    pub material_ref: String,
    pub content_hash: String,
    pub source_version: String,
}

/// host_*必须由可信thread/run元数据恢复，不能从record/body自声明复制。
pub struct OpaquePromptInputRecoveredMaterial {
    pub host_parent_context: LoopRunContext,
    pub host_child_context: LoopRunContext,
    pub host_registered_kind: String,
    pub record: OpaquePromptInputRecord,
    pub body: String,
}

/// 注入资格来自宿主configuration，不能由provider参数选择此实现。
/// persist必须原子持久化再返回；响应未知或required binding缺失必须Err。
/// load必须从可信host元数据独立恢复parent/child/role，再返回未授信record。
#[async_trait]
pub trait OpaquePromptInputBackend: Send + Sync {
    /// 同composition原stores建立后调用；既有prebuilt实现默认保持。
    async fn initialize_host_services(&self,_services:OpaquePromptInputHostServices)->Result<(),AgentLoopHostError>{Ok(())}

    async fn persist_before_submit(
        &self,
        facts: &HostSpawnInputFacts<'_>,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecord>, AgentLoopHostError>;

    async fn load_for_child(
        &self,
        child: &LoopRunContext,
    ) -> Result<OpaquePromptInputOutcome<OpaquePromptInputRecoveredMaterial>, AgentLoopHostError>;
}

struct HostCheckedOpaqueInputPort {
    backend: Arc<dyn OpaquePromptInputBackend>,
    max_material_bytes: usize,
}

#[async_trait]
impl SubagentOpaqueInputPort for HostCheckedOpaqueInputPort {
    async fn bind_before_submit(
        &self,
        facts: HostSpawnInputFacts<'_>,
    ) -> Result<Option<OpaquePromptInputReceipt>, AgentLoopHostError> {
        validate_host_facts(&facts)?;
        match self.backend.persist_before_submit(&facts).await? {
            OpaquePromptInputOutcome::NotRequired => Ok(None),
            OpaquePromptInputOutcome::Required(record) => {
                check_record(&record, &facts)?;
                check_charge(&record, "", self.max_material_bytes)?;
                Ok(Some(OpaquePromptInputReceipt::from_checked_record(&record)?))
            }
        }
    }

    async fn material_for_child(
        &self,
        child: &LoopRunContext,
    ) -> Result<Option<OpaquePromptInputMaterial>, AgentLoopHostError> {
        if child.thread_id != child.scope.thread_id {
            return Err(invalid("actual child context thread mismatch"));
        }
        match self.backend.load_for_child(child).await? {
            OpaquePromptInputOutcome::NotRequired => Ok(None),
            OpaquePromptInputOutcome::Required(recovered) => {
                if recovered.host_child_context != *child {
                    return Err(invalid("recovered host child context mismatch"));
                }
                let facts = HostSpawnInputFacts {
                    parent_context: &recovered.host_parent_context,
                    child_run_id: child.run_id,
                    child_thread_id: &child.thread_id,
                    registered_kind: &recovered.host_registered_kind,
                };
                validate_host_facts(&facts)?;
                check_record(&recovered.record, &facts)?;
                if recovered.record.child_scope != child.scope {
                    return Err(invalid("opaque material actual child scope mismatch"));
                }
                let charged_bytes = check_charge(
                    &recovered.record, &recovered.body, self.max_material_bytes,
                )?;
                let hash = hex::encode(Sha256::digest(recovered.body.as_bytes()));
                if hash != recovered.record.content_hash {
                    return Err(invalid("opaque material body hash mismatch"));
                }
                let receipt = OpaquePromptInputReceipt::from_checked_record(&recovered.record)?;
                Ok(Some(OpaquePromptInputMaterial {
                    receipt, body: recovered.body, charged_bytes,
                }))
            }
        }
    }
}

fn invalid(message: &'static str) -> AgentLoopHostError {
    AgentLoopHostError::new(AgentLoopHostErrorKind::Invalid, message)
}

fn validate_host_facts(facts: &HostSpawnInputFacts<'_>) -> Result<(), AgentLoopHostError> {
    if facts.parent_context.thread_id != facts.parent_context.scope.thread_id
        || facts.parent_context.scope.agent_id.is_none()
        || facts.registered_kind.is_empty()
        || facts.child_run_id == facts.parent_context.run_id
        || *facts.child_thread_id == facts.parent_context.thread_id
    {
        return Err(invalid("opaque input host facts invalid"));
    }
    Ok(())
}

fn actual_child_scope(facts: &HostSpawnInputFacts<'_>) -> TurnScope {
    let parent = &facts.parent_context.scope;
    // 与finish_spawn创建child_turn_scope的owner规则保持一致。
    match parent.explicit_owner_user_id() {
        Some(owner) => TurnScope::new_with_owner(
            parent.tenant_id.clone(), parent.agent_id.clone(), parent.project_id.clone(),
            (*facts.child_thread_id).clone(), Some(owner.clone()),
        ),
        None => TurnScope::new(
            parent.tenant_id.clone(), parent.agent_id.clone(), parent.project_id.clone(),
            (*facts.child_thread_id).clone(),
        ),
    }
}

fn check_record(
    record: &OpaquePromptInputRecord,
    facts: &HostSpawnInputFacts<'_>,
) -> Result<(), AgentLoopHostError> {
    if record.parent_context != *facts.parent_context
        || record.child_run_id != facts.child_run_id
        || record.child_thread_id != *facts.child_thread_id
        || record.child_scope != actual_child_scope(facts)
        || record.registered_kind != facts.registered_kind
    {
        return Err(invalid("opaque record differs from actual host spawn facts"));
    }
    Ok(())
}

fn check_charge(
    record: &OpaquePromptInputRecord,
    body: &str,
    max_bytes: usize,
) -> Result<usize, AgentLoopHostError> {
    let parent_bytes = serde_json::to_vec(&record.parent_context)
        .map_err(|_| invalid("opaque input parent context encoding failed"))?;
    let scope_bytes = serde_json::to_vec(&record.child_scope)
        .map_err(|_| invalid("opaque input child scope encoding failed"))?;
    let child_run = record.child_run_id.to_string();
    let child_thread = record.child_thread_id.to_string();
    let mut charged = 0_usize;
    for size in [
        body.len(), parent_bytes.len(), scope_bytes.len(), child_run.len(), child_thread.len(),
        record.registered_kind.len(), record.binding_ref.len(), record.material_ref.len(),
        record.content_hash.len(), record.source_version.len(),
    ] {
        charged = charged.checked_add(size)
            .ok_or_else(|| invalid("opaque material whole budget overflow"))?;
        if charged > max_bytes {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::BudgetExceeded, "opaque material whole budget exceeded",
            ));
        }
    }
    Ok(charged)
}

/// 中立decorator：原source保有direction/goal/capability逻辑，同port同时供spawn及composer使用。
pub struct OpaqueBackedSubagentPromptMaterialSource {
    original: Arc<dyn SubagentPromptMaterialSource>,
    port: Arc<HostCheckedOpaqueInputPort>,
}

impl OpaqueBackedSubagentPromptMaterialSource {
    pub fn new(
        original: Arc<dyn SubagentPromptMaterialSource>,
        backend: Arc<dyn OpaquePromptInputBackend>,
    ) -> Self {
        Self::with_max_material_bytes(original, backend, 64 * 1024)
    }

    pub fn with_max_material_bytes(
        original: Arc<dyn SubagentPromptMaterialSource>,
        backend: Arc<dyn OpaquePromptInputBackend>,
        max_material_bytes: usize,
    ) -> Self {
        Self { original, port: Arc::new(HostCheckedOpaqueInputPort { backend, max_material_bytes }) }
    }
}

#[async_trait]
impl SubagentPromptMaterialSource for OpaqueBackedSubagentPromptMaterialSource {
    async fn material_for_run(
        &self,
        run_context: &LoopRunContext,
    ) -> Result<SubagentPromptMaterial, AgentLoopHostError> {
        self.original.material_for_run(run_context).await
    }

    fn opaque_input_port(&self) -> Option<Arc<dyn SubagentOpaqueInputPort>> {
        Some(self.port.clone())
    }
}

/// 无Deserialize；只由composition传入原host服务，不从model DTO构造。
#[derive(Clone)]
pub struct OpaquePromptInputHostServices {
 pub turns:Arc<dyn ironclaw_turns::TurnSpawnTreeStateStore>,
 pub threads:Arc<dyn ironclaw_threads::SessionThreadService>,
 pub definitions:Arc<dyn crate::SubagentDefinitionResolver>,
}
#[async_trait]
pub trait OpaquePromptInputBackendFactory:Send+Sync {
 async fn create(&self,services:OpaquePromptInputHostServices)->Result<Arc<dyn OpaquePromptInputBackend>,AgentLoopHostError>;
}
/// 启动前仅携带可信factory；同原runtime一次初始化实际backend，不建第二Store。
pub struct FactoryOpaquePromptInputBackend {
 factory:Arc<dyn OpaquePromptInputBackendFactory>,
 started:std::sync::atomic::AtomicBool,
 actual:std::sync::OnceLock<Arc<dyn OpaquePromptInputBackend>>,
}
impl FactoryOpaquePromptInputBackend {
 pub fn new(factory:Arc<dyn OpaquePromptInputBackendFactory>)->Self{Self{factory,started:std::sync::atomic::AtomicBool::new(false),actual:std::sync::OnceLock::new()}}
 fn actual(&self)->Result<&Arc<dyn OpaquePromptInputBackend>,AgentLoopHostError>{self.actual.get().ok_or_else(||invalid("opaque factory not initialized or unknown"))}
}
#[async_trait]
impl OpaquePromptInputBackend for FactoryOpaquePromptInputBackend {
 async fn initialize_host_services(&self,services:OpaquePromptInputHostServices)->Result<(),AgentLoopHostError>{
  if self.started.compare_exchange(false,true,std::sync::atomic::Ordering::AcqRel,std::sync::atomic::Ordering::Acquire).is_err(){return Err(invalid("opaque factory already attempted"))}
  let actual=self.factory.create(services).await?;
  self.actual.set(actual).map_err(|_|invalid("opaque factory already initialized"))?;Ok(())
 }
 async fn persist_before_submit(&self,facts:&HostSpawnInputFacts<'_>)->Result<OpaquePromptInputOutcome<OpaquePromptInputRecord>,AgentLoopHostError>{self.actual()?.persist_before_submit(facts).await}
 async fn load_for_child(&self,child:&LoopRunContext)->Result<OpaquePromptInputOutcome<OpaquePromptInputRecoveredMaterial>,AgentLoopHostError>{self.actual()?.load_for_child(child).await}
}
