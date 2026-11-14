//! Safe resident protocol observation. Constructors remain composition-private.
use serde::Serialize;
use ironclaw_host_api::TenantId;
use crate::{RebornCompositionProfile,RebornReadiness,RebornReadinessState,RebornFacadeReadiness,RebornWorkerReadiness};
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize)]
#[serde(rename_all="camelCase")]
pub enum RebornResidentWorkerState {Running,NotConfigured,Missing,Finished,Cancelled,Unknown}
/// This value is serializable only, with no public fields/constructor or Deserialize authority-shaped input.
/// It reports one fresh runtime observation, not native/source/PID/provider acceptance.
#[derive(Debug,Serialize)]
#[serde(rename_all="camelCase")]
pub struct RebornRequesterRuntimeProtocolSnapshot {
    schema:&'static str,
    protocol:&'static str,
    tenant_id:TenantId,
    runtime_instance_id:String,
    readiness_state:RebornReadinessState,
    facades:RebornFacadeReadiness,
    declared_workers:RebornWorkerReadiness,
    turn_runner:RebornResidentWorkerState,
    trigger_poller:RebornResidentWorkerState,
    trigger_configured:bool,
    shutdown_requested:bool,
    blocking_diagnostics:usize,
    unknown_diagnostics:usize,
    /// Worker construction/liveness is distinct from execution permission or HTTP readiness.
    execution_pending:bool,
    ready:bool,
    unknown:bool,
}
impl RebornRequesterRuntimeProtocolSnapshot {
    pub fn ready(&self)->bool {self.ready}
    pub fn execution_pending(&self)->bool {self.execution_pending}
    pub(super) fn with_execution_pending(mut self,pending:bool)->Self {self.execution_pending=pending;self}
    pub fn unknown(&self)->bool {self.unknown}
    pub fn tenant_id(&self)->&TenantId {&self.tenant_id}
    pub fn runtime_instance_id(&self)->&str {&self.runtime_instance_id}
    pub(super) fn from_runtime_observation(tenant_id:TenantId,runtime_instance_id:String,readiness:&RebornReadiness,
        turn_runner:RebornResidentWorkerState,trigger_poller:RebornResidentWorkerState,
        trigger_configured:bool,shutdown_requested:bool)->Self {
        let blocking=readiness.diagnostics.iter().filter(|d|d.blocks_production).count();
        let unknown_diagnostics=readiness.diagnostics.iter().filter(|d|matches!(&d.status,crate::readiness::RebornReadinessDiagnosticStatus::Unknown(_))).count();
        let turn_live=readiness.workers.turn_runner&&turn_runner==RebornResidentWorkerState::Running;
        let trigger_live=if trigger_configured {readiness.workers.trigger_poller&&trigger_poller==RebornResidentWorkerState::Running}
            else {!readiness.workers.trigger_poller&&trigger_poller==RebornResidentWorkerState::NotConfigured};
        let profile_validated=(readiness.profile==RebornCompositionProfile::Production&&readiness.state==RebornReadinessState::ProductionValidated)
            ||(readiness.profile==RebornCompositionProfile::HostedSingleTenant&&readiness.state==RebornReadinessState::HostedSingleTenantValidated);
        let ready=profile_validated
            &&readiness.facades.host_runtime&&readiness.facades.turn_coordinator&&readiness.facades.product_auth
            &&blocking==0&&unknown_diagnostics==0&&turn_live&&trigger_live&&!shutdown_requested;
        Self {schema:"reborn.requester_runtime_protocol/2",protocol:crate::reborn_requester_egress_protocol_version(),tenant_id,runtime_instance_id,
            readiness_state:readiness.state,facades:readiness.facades,declared_workers:readiness.workers,turn_runner,trigger_poller,
            trigger_configured,shutdown_requested,blocking_diagnostics:blocking,unknown_diagnostics,ready,unknown:!ready,execution_pending:false}
    }
}

/// stdout line framing is fixed; Root must read it from the original owned process pipe.
pub(super) fn write_frame(snapshot:&RebornRequesterRuntimeProtocolSnapshot,output:&mut impl std::io::Write)->Result<(),&'static str> {
    const MAX:usize=16*1024;
    let raw=serde_json::to_vec(snapshot).map_err(|_|"residentSnapshotSerializationFailed")?;
    if raw.len()>MAX {return Err("residentSnapshotFrameBudgetExceeded");}
    let prefix=format!("IRONCLAW_RESIDENT_PROTOCOL_V2 {} ",raw.len());
    output.write_all(prefix.as_bytes()).map_err(|_|"residentSnapshotFrameWriteFailed")?;
    output.write_all(&raw).map_err(|_|"residentSnapshotFrameWriteFailed")?;
    output.write_all(b"\n").map_err(|_|"residentSnapshotFrameWriteFailed")?;
    output.flush().map_err(|_|"residentSnapshotFrameFlushFailed")
}
