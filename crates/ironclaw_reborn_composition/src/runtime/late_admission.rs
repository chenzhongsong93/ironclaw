//! Same-runtime Root startup admission through existing lifecycle, never direct registry publication.
use std::sync::Arc;
use super::{RebornRuntime,RebornRuntimeError};
use ironclaw_product_workflow::{LifecycleProductAction,LifecycleProductContext,LifecycleProductFacade,LifecycleProductSurfaceContext,LifecyclePhase,LifecycleProductResponse};
#[derive(Clone)] pub(super) struct LateAdmissionPorts {
    pub(super) store:Arc<crate::operator_late_admission::OperatorLateAdmissionStore>,
    pub(super) manager:Arc<crate::extension_host::extension_lifecycle::RebornLocalExtensionManagementPort>,
    pub(super) facade:Arc<dyn LifecycleProductFacade>,
}
fn rejected()->RebornRuntimeError {stage_rejected("ports")}
fn stage_rejected(stage: &str)->RebornRuntimeError {RebornRuntimeError::InvalidArgument {reason:format!("sameRuntimeHostAdmission:{stage}")}}
impl RebornRuntime {
    fn late_admission_ports(&self)->Result<LateAdmissionPorts,RebornRuntimeError> {
        #[cfg(test)] if let Some(ports)=self.late_admission_fixture.as_ref(){return Ok(ports.clone());}
        #[cfg(any(feature="libsql",feature="postgres"))]
        if let Some(graph)=self.services.production_runtime.as_ref() {
            if self.services.local_runtime.as_ref().and_then(|runtime|runtime.operator_admission.as_ref()).is_some() {
                return Err(stage_rejected("authorityAmbiguous"));
            }
            let normal=graph.normal_capability_binding().map_err(|_|rejected())?;
            return Ok(LateAdmissionPorts {store:graph.operator_admission(),manager:normal.extensions.management().ok_or_else(rejected)?,facade:graph.lifecycle_facade().map_err(|_|rejected())?});
        }
        if let Some(local)=self.services.local_runtime.as_ref() {
            return Ok(LateAdmissionPorts {
                store:local.operator_admission.clone().ok_or_else(rejected)?,
                manager:local.extension_management.clone().ok_or_else(rejected)?,
                facade:local.lifecycle_facade.clone().ok_or_else(rejected)?,
            });
        }
        Err(rejected())
    }
    pub(super) async fn admit_root_bundle_inner(&self,bundle:crate::RebornHostExtensionBundle)->Result<Vec<LifecycleProductResponse>,RebornRuntimeError> {
        let current=self.requester_runtime_protocol_snapshot();
        if !current.ready()||current.unknown()||bundle.tenant()!=current.tenant_id()
            ||bundle.bindings().iter().any(|b|b.runtime_instance_id()!=current.runtime_instance_id()) {return Err(stage_rejected("initialReady"));}
        let ports=self.late_admission_ports()?;let bundle=Arc::new(bundle);let bindings=bundle.bindings();let providers:Vec<_>=bindings.iter().map(|b|b.provider().clone()).collect();
        // Register deny-pending before any lifecycle publication. Unknown/failure retains original files.
        ports.store.reserve(bundle.clone(),current.tenant_id(),current.runtime_instance_id(),self.resident_admission_observer()).map_err(|_|stage_rejected("reserve"))?;
        ports.manager.revalidate_root_bundle(&bundle).await.map_err(|_|stage_rejected("revalidate"))?;
        let context=LifecycleProductContext::Surface(LifecycleProductSurfaceContext {tenant_id:self.thread_scope.tenant_id.clone(),user_id:self.actor_user_id.clone(),agent_id:Some(self.thread_scope.agent_id.clone()),project_id:self.thread_scope.project_id.clone()});
        let mut responses=Vec::new();
        for package_ref in bundle.package_refs() {
            if !self.requester_runtime_protocol_snapshot().ready(){return Err(stage_rejected("installReady"));}
            // Same normal facade checks the actual visible installation and caller scope.
            // Revalidation above already bound the exact package/source; never treat an install error as success.
            let projected=ports.facade.project_package(context.clone(),package_ref.clone()).await.map_err(|_|stage_rejected("project"))?;
            let Some(ironclaw_product_workflow::LifecycleProductPayload::ExtensionList { extensions,count })=&projected.payload else {return Err(stage_rejected("projectContract"));};
            if *count!=1||extensions.len()!=1||!projected.blockers.is_empty()||projected.package_ref.as_ref()!=Some(&package_ref)
                ||extensions[0].summary.package_ref!=package_ref||extensions[0].phase!=projected.phase {return Err(stage_rejected("projectContract"));}
            match (projected.phase,extensions[0].install_scope) {
                (LifecyclePhase::Discovered,None)=> {
                    let installed=ports.facade.execute(context.clone(),LifecycleProductAction::ExtensionInstall {package_ref:package_ref.clone()}).await.map_err(|_|stage_rejected("install"))?;
                    if installed.phase!=LifecyclePhase::Installed||!matches!(&installed.payload,Some(ironclaw_product_workflow::LifecycleProductPayload::ExtensionInstall {installed:true,..}))
                        ||!installed.blockers.is_empty()||installed.package_ref.as_ref()!=Some(&package_ref) {return Err(stage_rejected("installContract"));}
                    responses.push(installed);
                },
                (LifecyclePhase::Installed|LifecyclePhase::Active,Some(_))=>responses.push(projected),
                // Disabled is an existing administrative choice, never implicitly undone by a restart.
                _=>return Err(stage_rejected("projectContract")),
            }
            let activated=ports.facade.execute(context.clone(),LifecycleProductAction::ExtensionActivate {package_ref:package_ref.clone()}).await.map_err(|_|stage_rejected("activate"))?;
            if activated.phase!=LifecyclePhase::Active||!activated.blockers.is_empty()||activated.package_ref.as_ref()!=Some(&package_ref) {return Err(stage_rejected("activateContract"));}responses.push(activated);
        }
        if !self.requester_runtime_protocol_snapshot().ready(){return Err(stage_rejected("finalReady"));}
        if !ports.manager.root_bundle_is_current_active(&bundle){return Err(stage_rejected("finalActive"));}
        ports.store.commit(&providers).map_err(|_|stage_rejected("commit"))?;
        if !self.requester_runtime_protocol_snapshot().ready(){return Err(stage_rejected("commitReady"));}
        Ok(responses)
    }
}
