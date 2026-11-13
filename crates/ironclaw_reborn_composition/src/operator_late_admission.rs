//! Private same-runtime binding store. No HTTP/model mutation or permission override.
use std::{collections::BTreeMap,sync::{Arc,RwLock}};
use ironclaw_host_api::{ExtensionId,TenantId};
use ironclaw_mcp::McpTrustedContextSigner;
use ironclaw_product_workflow::ProductWorkflowError;
use crate::{RebornHostExtensionBundle,operator_host_bundle::OperatorOwnedMcpBinding};
fn rejected()->ProductWorkflowError {ProductWorkflowError::InvalidBindingRequest {reason:"sameRuntimeHostAdmissionRejected".into()}}
struct Entry {binding:OperatorOwnedMcpBinding,active:bool}
#[derive(Default)] struct State {entries:BTreeMap<ExtensionId,Entry>,held:Vec<Arc<RebornHostExtensionBundle>>,instance:Option<String>,gate:Option<Arc<dyn Fn()->bool+Send+Sync>>}
#[derive(Default)] pub(crate) struct OperatorLateAdmissionStore {state:RwLock<State>,signer:RwLock<Option<McpTrustedContextSigner>>}
impl std::fmt::Debug for OperatorLateAdmissionStore {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.debug_struct("OperatorLateAdmissionStore").finish_non_exhaustive()}}
impl OperatorLateAdmissionStore {
    pub(crate) fn set_actual_signer(&self,signer:&McpTrustedContextSigner)->Result<(),ProductWorkflowError> {let mut slot=self.signer.write().map_err(|_|rejected())?;if slot.is_some(){return Err(rejected());}*slot=Some(signer.clone());Ok(())}
    pub(crate) fn bind_runtime(&self,instance:&str,gate:Arc<dyn Fn()->bool+Send+Sync>)->Result<(),ProductWorkflowError> {
        let mut state=self.state.write().map_err(|_|rejected())?;if state.instance.is_some(){return Err(rejected());}
        state.instance=Some(instance.into());state.gate=Some(gate);Ok(())
    }
    pub(crate) fn old_binding_valid(&self,binding:&OperatorOwnedMcpBinding)->bool {
        {let Ok(state)=self.state.read() else{return false;};let same=state.instance.as_deref()==Some(binding.runtime_instance_id());let gate=state.gate.clone();drop(state);same&&gate.is_some_and(|gate|gate())}
    }
    pub(crate) fn reserve(&self,bundle:Arc<RebornHostExtensionBundle>,tenant:&TenantId,instance:&str,gate:Arc<dyn Fn()->bool+Send+Sync>)->Result<(),ProductWorkflowError> {
        if !gate()||bundle.tenant()!=tenant{return Err(rejected());}
        let bindings=bundle.bindings();if bindings.is_empty()||bindings.iter().any(|b|b.tenant()!=tenant||b.runtime_instance_id()!=instance){return Err(rejected());}
        let signer={self.signer.read().map_err(|_|rejected())?.as_ref().cloned().ok_or_else(rejected)?};
        if bindings.iter().any(|b|!signer.is_configured()||signer.provider_id()!=b.provider()||signer.audience()!=Some(b.audience())){return Err(rejected());}
        let mut state=self.state.write().map_err(|_|rejected())?;
        if state.instance.as_deref().is_some_and(|old|old!=instance)||bindings.iter().any(|b|state.entries.contains_key(b.provider())) {return Err(rejected());}
        if state.instance.as_deref()!=Some(instance){return Err(rejected());}
        for binding in bindings {state.entries.insert(binding.provider().clone(),Entry {binding,active:false});}
        state.held.push(bundle);Ok(())
    }
    pub(crate) fn commit(&self,providers:&[ExtensionId])->Result<(),ProductWorkflowError> {
        let gate={self.state.read().map_err(|_|rejected())?.gate.clone().ok_or_else(rejected)?};
        if !gate(){return Err(rejected());}
        let mut state=self.state.write().map_err(|_|rejected())?;
        if providers.iter().any(|p|!state.entries.contains_key(p)){return Err(rejected());}
        for provider in providers {state.entries.get_mut(provider).ok_or_else(rejected)?.active=true;}Ok(())
    }
    pub(crate) fn requires_admission(&self,provider:&ExtensionId)->bool {
        let selected={match self.signer.read(){Ok(slot)=>slot.as_ref().is_some_and(|signer|signer.provider_id()==provider),Err(_)=>return true}};
        selected||self.contains(provider)
    }
    pub(crate) fn contains(&self,provider:&ExtensionId)->bool {self.state.read().map_or(true,|state|state.entries.contains_key(provider))}
    pub(crate) fn binding(&self,provider:&ExtensionId)->Option<OperatorOwnedMcpBinding> {let (binding,gate)={let state=self.state.read().ok()?;let entry=state.entries.get(provider)?;(entry.active.then(||entry.binding.clone())?,state.gate.clone()?)};gate().then_some(binding)}
    /// Used only during normal installed-package discovery before model dispatch is enabled.
    pub(crate) fn discovery(&self,provider:&ExtensionId)->Option<(OperatorOwnedMcpBinding,McpTrustedContextSigner)> {
        let signer={self.signer.read().ok()?.as_ref()?.clone()};
        let (binding,gate)={let state=self.state.read().ok()?;(state.entries.get(provider)?.binding.clone(),state.gate.clone()?)};
        gate().then_some((binding,signer))
    }
}
