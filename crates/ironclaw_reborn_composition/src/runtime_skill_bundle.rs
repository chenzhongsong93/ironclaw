//! Generic first-party skills bundle; no product assets, tool registry or authority DTO.
use ironclaw_first_party_extension_ports::{
    SelectableSkillContextSource, SkillActivationSelectorConfig, SkillExecutionAdapter,
};
use ironclaw_loop_host::{HostSkillContextSource, SkillBundleSource};
use std::sync::Arc;

/// Complete typed bundle. The constructor creates execution from the SAME
/// selector; fields are private so context-only sources cannot masquerade as it.
#[derive(Clone)]
pub struct RebornRuntimeSkillBundle {
    source: Arc<dyn SkillBundleSource>,
    selector: Arc<SelectableSkillContextSource<dyn SkillBundleSource>>,
    execution: Arc<SkillExecutionAdapter<dyn SkillBundleSource>>,
}
impl RebornRuntimeSkillBundle {
    pub fn new(source: Arc<dyn SkillBundleSource>, config: SkillActivationSelectorConfig) -> Self {
        let selector = Arc::new(SelectableSkillContextSource::new(source.clone(), config));
        let execution = Arc::new(SkillExecutionAdapter::new(selector.clone()));
        Self {
            source,
            selector,
            execution,
        }
    }
    pub(crate) fn source(&self) -> Arc<dyn SkillBundleSource> {
        self.source.clone()
    }
    pub fn context_source(&self) -> Arc<dyn HostSkillContextSource> {
        self.selector.clone()
    }
    /// Trusted generic capability assembly shares the original selector; no new one.
    pub(crate) fn activation_source(
        &self,
    ) -> Arc<SelectableSkillContextSource<dyn SkillBundleSource>> {
        self.selector.clone()
    }
    pub(crate) fn execution_adapter(&self) -> Arc<SkillExecutionAdapter<dyn SkillBundleSource>> {
        self.execution.clone()
    }
    pub(crate) fn execution(&self) -> Arc<SkillExecutionAdapter<dyn SkillBundleSource>> {
        self.execution.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_filesystem::{DiskFilesystem, ScopedFilesystem};
    use ironclaw_host_api::{
        AgentId, HostPath, MountAlias, MountGrant, MountPermissions, MountView, ProjectId,
        ScopedPath, TenantId, ThreadId, UserId, VirtualPath,
    };
    use ironclaw_loop_host::{
        FilesystemSkillBundleRoot, FilesystemSkillBundleSource, HostSkillContextCandidatePayload,
    };
    use ironclaw_turns::{
        AcceptedMessageRef, RunProfileResolutionRequest, RunProfileResolver, TurnId, TurnRunId,
        TurnScope,
        run_profile::{InMemoryRunProfileResolver, LoopRunContext},
    };
    async fn run() -> LoopRunContext {
        let scope = TurnScope::new_with_owner(
            TenantId::new("shared-skill-tenant").unwrap(),
            Some(AgentId::new("shared-skill-agent").unwrap()),
            Some(ProjectId::new("shared-skill-project").unwrap()),
            ThreadId::new("shared-skill-thread").unwrap(),
            Some(UserId::new("shared-skill-owner").unwrap()),
        );
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        LoopRunContext::new(scope, TurnId::new(), TurnRunId::new(), profile)
            .with_accepted_message_ref(
                AcceptedMessageRef::new("msg:shared-skill-fixture".to_owned()).unwrap(),
            )
    }
    fn normal_filesystem_bundle(
        body: &str,
        budget: usize,
    ) -> (tempfile::TempDir, RebornRuntimeSkillBundle, String) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("shared-writing")).unwrap();
        let manifest = format!(
            "---\nname: shared-writing\ndescription: Generic writing method\nactivation:\n  keywords: [writing]\n---\n{body}\n"
        );
        std::fs::write(tmp.path().join("shared-writing/SKILL.md"), &manifest).unwrap();
        std::fs::write(
            tmp.path().join("shared-writing/rules.txt"),
            b"whole original rule bytes",
        )
        .unwrap();
        let mut root = DiskFilesystem::new();
        root.mount_local(
            VirtualPath::new("/system/skills").unwrap(),
            HostPath::from_path_buf(tmp.path().to_path_buf()),
        )
        .unwrap();
        let view = MountView::new(vec![MountGrant::new(
            MountAlias::new("/system/skills").unwrap(),
            VirtualPath::new("/system/skills").unwrap(),
            MountPermissions::read_only(),
        )])
        .unwrap();
        let scoped = Arc::new(ScopedFilesystem::new(Arc::new(root), move |scope| {
            if scope.tenant_id.as_str() != "shared-skill-tenant" {
                return Err(ironclaw_host_api::HostApiError::InvalidPath {
                    value: "/system/skills".to_owned(),
                    reason: "tenant denied".to_owned(),
                });
            }
            Ok(view.clone())
        }));
        let source: Arc<dyn SkillBundleSource> = Arc::new(
            FilesystemSkillBundleSource::new(
                scoped,
                vec![FilesystemSkillBundleRoot::system(
                    ScopedPath::new("/system/skills").unwrap(),
                )],
            )
            .unwrap(),
        );
        let bundle = RebornRuntimeSkillBundle::new(
            source,
            SkillActivationSelectorConfig::default().set_max_context_tokens(budget),
        );
        (tmp, bundle, manifest)
    }
    #[tokio::test]
    async fn normal_filesystem_execution_record_load_take_and_read_share_one_selector() {
        let (_tmp, bundle, manifest) =
            normal_filesystem_bundle(&"complete generic method ".repeat(2300), 40000);
        let run = run().await;
        let execution = bundle.execution_adapter();
        execution
            .record_user_message_for_execution(
                run.scope.clone(),
                run.accepted_message_ref.clone().unwrap(),
                "$shared-writing",
            )
            .unwrap();
        let candidates = bundle
            .context_source()
            .load_skill_context_candidates(&run)
            .await
            .unwrap();
        assert!(candidates.iter().any(|c|matches!(&c.payload,HostSkillContextCandidatePayload::LoadedSkillMd(body) if body==&manifest)));
        let plan = execution
            .take_execution_plan_for_run(&run.scope, run.run_id)
            .unwrap()
            .unwrap();
        assert_eq!(plan.activated_bundles().len(), 1);
        let activation = &plan.activation_plan().selection.activations[0];
        let asset = execution
            .read_file_for_activation(&run, plan.activation_plan(), activation, "rules.txt")
            .await
            .unwrap();
        assert_eq!(asset.bytes, b"whole original rule bytes");
        assert!(
            execution
                .take_execution_plan_for_run(&run.scope, run.run_id)
                .unwrap()
                .is_none()
        );
        assert!(Arc::ptr_eq(&bundle.activation_source(), &bundle.selector));
    }
    #[tokio::test]
    async fn clear_removes_original_accepted_activation_and_small_budget_never_truncates_required_body()
     {
        let (_tmp, bundle, _) = normal_filesystem_bundle(&"required method ".repeat(4000), 1);
        let run = run().await;
        bundle
            .activation_source()
            .record_user_message(
                run.scope.clone(),
                run.accepted_message_ref.clone().unwrap(),
                "$shared-writing",
            )
            .unwrap();
        assert!(
            bundle
                .context_source()
                .load_skill_context_candidates(&run)
                .await
                .is_err()
        );
        bundle
            .activation_source()
            .clear_accepted_message(&run.scope, run.accepted_message_ref.as_ref().unwrap())
            .unwrap();
        let candidates = bundle
            .context_source()
            .load_skill_context_candidates(&run)
            .await
            .unwrap();
        assert!(!candidates.iter().any(|c| matches!(
            c.payload,
            HostSkillContextCandidatePayload::LoadedSkillMd(_)
        )));
    }
    #[tokio::test]
    async fn high_facade_prepares_and_reads_original_adapter_without_returning_lower_handles() {
        let (_tmp, bundle, _) = normal_filesystem_bundle("whole public facade method", 40000);
        let run = run().await;
        let metadata = bundle.available_skill_bundles(&run).await.unwrap();
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].skill_name, "shared-writing");
        let plan = bundle
            .prepare_execution(&run, "$shared-writing")
            .await
            .unwrap();
        assert_eq!(plan.active_bundles().len(), 1);
        let asset = bundle
            .read_execution_asset(&plan, &plan.activations()[0], "rules.txt")
            .await
            .unwrap();
        assert_eq!(asset.bytes, b"whole original rule bytes");
        assert!(
            bundle
                .read_execution_asset(&plan, &plan.activations()[0], "../outside.txt")
                .await
                .is_err()
        );
    }
    // 覆盖:TQH-L1-03，正式identity全文与真实profile缺失成对，不造默认资料。
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    #[tokio::test]
    async fn normal_production_context_preserves_identity_and_unknown_profile() {
        let tmp = tempfile::tempdir().unwrap();
        let tenants = tmp.path().join("tenants");
        std::fs::create_dir(&tenants).unwrap();
        let prompt = tmp.path().join("SYSTEM.md");
        let body = "正式完整身份：按真实图谱读取，不自授发布批准。\n";
        std::fs::write(&prompt, body).unwrap();
        let mut filesystem = DiskFilesystem::new();
        filesystem.mount_local(VirtualPath::new("/tenants").unwrap(), HostPath::from_path_buf(tenants)).unwrap();
        let sources = crate::runtime::production_context_sources(
            Arc::new(filesystem), tmp.path().to_path_buf(), prompt, false,
        ).unwrap();
        let run = run().await;
        let candidates = sources.identity.load_identity_candidates(&run, ironclaw_turns::run_profile::PromptMode::TextOnly).await.unwrap();
        assert_eq!(candidates.len(), 1);
        let content = sources.identity.resolve_identity_message_content(&run, candidates[0].message_ref.as_ref().unwrap()).await.unwrap().unwrap();
        assert_eq!(content.content, body);
        assert!(sources.user_profile.resolve_user_profile(&run).await.is_none());
    }

    // 覆盖:TQH-L1-04，operator identity越过声明root拒绝。
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    #[tokio::test]
    async fn normal_production_context_rejects_identity_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let prompt = foreign.path().join("SYSTEM.md");
        std::fs::write(&prompt, "outside identity").unwrap();
        assert!(crate::runtime::production_context_sources(
            Arc::new(DiskFilesystem::new()), root.path().to_path_buf(), prompt, false,
        ).is_err());
    }

    // 覆盖:TQH-L1-05，native动态owner mount与同selector execution不串主文/附件。
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    #[tokio::test]
    async fn normal_production_skill_mount_isolates_owner_body_and_attachment() {
        let tmp = tempfile::tempdir().unwrap();
        let tenants = tmp.path().join("tenants");
        for (owner, body) in [("shared-skill-owner", "owner-A-body"), ("other-owner", "owner-B-body")] {
            let dir = tenants.join("shared-skill-tenant/users").join(owner).join("skills/shared-writing");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\nname: shared-writing\ndescription: Generic writing method\n---\n{body}\n")).unwrap();
            std::fs::write(dir.join("rules.txt"), body).unwrap();
        }
        let mut filesystem = DiskFilesystem::new();
        filesystem.mount_local(VirtualPath::new("/tenants").unwrap(), HostPath::from_path_buf(tenants)).unwrap();
        let scoped = Arc::new(ScopedFilesystem::new(Arc::new(filesystem), crate::factory::production_skill_management_mount_view));
        let source: Arc<dyn SkillBundleSource> = Arc::new(FilesystemSkillBundleSource::new(scoped,
            vec![FilesystemSkillBundleRoot::user(ScopedPath::new("/skills").unwrap())]).unwrap().with_thread_owner_scope());
        let bundle = RebornRuntimeSkillBundle::new(source, SkillActivationSelectorConfig::default().set_max_context_tokens(4000));
        for (owner, body) in [("shared-skill-owner", "owner-A-body"), ("other-owner", "owner-B-body")] {
            let mut run = run().await;
            run.scope = TurnScope::new_with_owner(TenantId::new("shared-skill-tenant").unwrap(),
                Some(AgentId::new("shared-skill-agent").unwrap()), Some(ProjectId::new("shared-skill-project").unwrap()),
                ThreadId::new("shared-skill-thread").unwrap(), Some(UserId::new(owner).unwrap()));
            run = run.with_actor(ironclaw_turns::TurnActor::new(UserId::new("other-owner").unwrap()));
            bundle.execution_adapter().record_user_message_for_execution(run.scope.clone(), run.accepted_message_ref.clone().unwrap(), "$shared-writing").unwrap();
            let candidates = bundle.context_source().load_skill_context_candidates(&run).await.unwrap();
            assert!(candidates.iter().any(|c|matches!(&c.payload, HostSkillContextCandidatePayload::LoadedSkillMd(text) if text.ends_with(&format!("{body}\n")))));
            let plan = bundle.execution_adapter().take_execution_plan_for_run(&run.scope, run.run_id).unwrap().unwrap();
            let activation = &plan.activation_plan().selection.activations[0];
            let asset = bundle.execution_adapter().read_file_for_activation(&run, plan.activation_plan(), activation, "rules.txt").await.unwrap();
            assert_eq!(asset.bytes, body.as_bytes());
        }
    }

    // 覆盖:TQH-L1-06，A验证同名bundle不得授权B缺manifest的附件读取。
    #[cfg(any(feature = "libsql", feature = "postgres"))]
    #[tokio::test]
    async fn normal_production_manifest_validation_does_not_cross_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let tenants = tmp.path().join("tenants");
        for owner in ["shared-skill-owner", "other-owner"] {
            let dir = tenants.join("shared-skill-tenant/users").join(owner).join("skills/shared-writing");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("rules.txt"), owner).unwrap();
            if owner == "shared-skill-owner" {
                std::fs::write(dir.join("SKILL.md"), "---\nname: shared-writing\ndescription: valid A bundle\n---\nA instructions\n").unwrap();
            }
        }
        let mut filesystem = DiskFilesystem::new();
        filesystem.mount_local(VirtualPath::new("/tenants").unwrap(), HostPath::from_path_buf(tenants)).unwrap();
        let scoped = Arc::new(ScopedFilesystem::new(Arc::new(filesystem), crate::factory::production_skill_management_mount_view));
        let source = FilesystemSkillBundleSource::new(scoped,
            vec![FilesystemSkillBundleRoot::user(ScopedPath::new("/skills").unwrap())]).unwrap();
        let first = run().await.with_actor(ironclaw_turns::TurnActor::new(UserId::new("shared-skill-owner").unwrap()));
        assert_eq!(source.list_skill_bundles(&first).await.unwrap().len(), 1);
        let mut other = run().await;
        other.scope = TurnScope::new_with_owner(TenantId::new("shared-skill-tenant").unwrap(),
            Some(AgentId::new("shared-skill-agent").unwrap()), Some(ProjectId::new("shared-skill-project").unwrap()),
            ThreadId::new("shared-skill-thread").unwrap(), Some(UserId::new("other-owner").unwrap()));
        other = other.with_actor(ironclaw_turns::TurnActor::new(UserId::new("other-owner").unwrap()));
        let id = ironclaw_loop_host::SkillBundleId::new(ironclaw_loop_host::SkillSourceKind::User, "shared-writing").unwrap();
        let asset = ironclaw_loop_host::SkillFilePath::new("rules.txt").unwrap();
        assert!(source.read_skill_bundle_file(&other, &id, &asset).await.is_err());
    }

}
