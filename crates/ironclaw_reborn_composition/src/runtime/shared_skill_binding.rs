//! Typed forwarding keeps the existing concrete setup-marker path unchanged.
use super::{
    ComposedSelectableSkillContextSource, ComposedSkillExecutionAdapter, RebornSkillExecutionPlan,
};
use ironclaw_first_party_extension_ports::{
    SelectableSkillContextSource, SkillActivationObserver, SkillActivationPlan,
    SkillActivationRequest, SkillActivationSelectionError, SkillBundleAsset,
    SkillBundleAssetReadError, SkillExecutionAdapter,
};
use ironclaw_loop_host::SkillBundleSource;
use ironclaw_turns::{AcceptedMessageRef, TurnRunId, TurnScope, run_profile::LoopRunContext};
use std::sync::Arc;

#[derive(Clone)]
pub(crate) enum RuntimeSkillSelector {
    Local(Arc<ComposedSelectableSkillContextSource>),
    Production(Arc<SelectableSkillContextSource<dyn SkillBundleSource>>),
}
impl RuntimeSkillSelector {
    pub(crate) async fn activate_skills_for_run(
        &self,
        run: &LoopRunContext,
        names: &[String],
    ) -> Result<SkillActivationPlan, SkillActivationSelectionError> {
        match self {
            Self::Local(v) => v.activate_skills_for_run(run, names).await,
            Self::Production(v) => v.activate_skills_for_run(run, names).await,
        }
    }
    pub(crate) fn record_user_message(
        &self,
        scope: TurnScope,
        accepted: AcceptedMessageRef,
        text: impl Into<String>,
    ) -> Result<(), SkillActivationSelectionError> {
        let text = text.into();
        match self {
            Self::Local(v) => v.record_user_message(scope, accepted, text),
            Self::Production(v) => v.record_user_message(scope, accepted, text),
        }
    }
    pub(crate) fn clear_accepted_message(
        &self,
        scope: &TurnScope,
        accepted: &AcceptedMessageRef,
    ) -> Result<(), SkillActivationSelectionError> {
        match self {
            Self::Local(v) => v.clear_accepted_message(scope, accepted),
            Self::Production(v) => v.clear_accepted_message(scope, accepted),
        }
    }
    pub(crate) fn set_activation_observer(
        &self,
        observer: Arc<dyn SkillActivationObserver>,
    ) -> Result<(), SkillActivationSelectionError> {
        match self {
            Self::Local(v) => v.set_activation_observer(observer),
            Self::Production(v) => v.set_activation_observer(observer),
        }
    }
}
#[derive(Clone)]
pub(crate) enum RuntimeSkillExecutor {
    Local(Arc<ComposedSkillExecutionAdapter>),
    Production(Arc<SkillExecutionAdapter<dyn SkillBundleSource>>),
}
impl RuntimeSkillExecutor {
    pub(crate) fn record_user_message_for_execution(
        &self,
        scope: TurnScope,
        accepted: AcceptedMessageRef,
        text: impl Into<String>,
    ) -> Result<(), SkillActivationSelectionError> {
        let text = text.into();
        match self {
            Self::Local(v) => v.record_user_message_for_execution(scope, accepted, text),
            Self::Production(v) => v.record_user_message_for_execution(scope, accepted, text),
        }
    }
    pub(crate) fn take_execution_plan_for_run(
        &self,
        scope: &TurnScope,
        run: TurnRunId,
    ) -> Result<Option<RebornSkillExecutionPlan>, SkillActivationSelectionError> {
        match self {
            Self::Local(v) => v
                .take_execution_plan_for_run(scope, run)
                .map(|p| p.map(RebornSkillExecutionPlan::from_first_party)),
            Self::Production(v) => v
                .take_execution_plan_for_run(scope, run)
                .map(|p| p.map(RebornSkillExecutionPlan::from_first_party)),
        }
    }
    pub(crate) async fn read_file_for_activation(
        &self,
        run: &LoopRunContext,
        plan: &SkillActivationPlan,
        activation: &SkillActivationRequest,
        path: impl AsRef<str>,
    ) -> Result<SkillBundleAsset, SkillBundleAssetReadError> {
        let path = path.as_ref();
        match self {
            Self::Local(v) => {
                v.read_file_for_activation(run, plan, activation, path)
                    .await
            }
            Self::Production(v) => {
                v.read_file_for_activation(run, plan, activation, path)
                    .await
            }
        }
    }
}
