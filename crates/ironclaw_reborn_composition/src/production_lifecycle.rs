//! Production fixed-tenant lifecycle facade; lower installation stores stay private.
use async_trait::async_trait;
use ironclaw_host_api::TenantId;
use ironclaw_product_workflow::{
    LifecyclePackageRef, LifecycleProductAction, LifecycleProductContext, LifecycleProductFacade,
    LifecycleProductResponse, ProductWorkflowError,
};
use std::sync::Arc;

pub(crate) struct TenantBoundLifecycleFacade {
    tenant: TenantId,
    inner: Arc<dyn LifecycleProductFacade>,
}
impl TenantBoundLifecycleFacade {
    pub(crate) fn new(tenant: TenantId, inner: Arc<dyn LifecycleProductFacade>) -> Self {
        Self { tenant, inner }
    }
    fn check(&self, context: &LifecycleProductContext) -> Result<(), ProductWorkflowError> {
        let actual = match context {
            LifecycleProductContext::Surface(surface) => Some(&surface.tenant_id),
            LifecycleProductContext::Command(command) => command.auth_claim.tenant_id(),
        };
        if actual != Some(&self.tenant) {
            return Err(ProductWorkflowError::BindingAccessDenied);
        }
        Ok(())
    }
}
#[async_trait]
impl LifecycleProductFacade for TenantBoundLifecycleFacade {
    async fn execute(
        &self,
        context: LifecycleProductContext,
        action: LifecycleProductAction,
    ) -> Result<LifecycleProductResponse, ProductWorkflowError> {
        self.check(&context)?;
        self.inner.execute(context, action).await
    }
    async fn project_package(
        &self,
        context: LifecycleProductContext,
        package: LifecyclePackageRef,
    ) -> Result<LifecycleProductResponse, ProductWorkflowError> {
        self.check(&context)?;
        self.inner.project_package(context, package).await
    }
    async fn import_extension_bundle(
        &self,
        context: LifecycleProductContext,
        bundle: Vec<u8>,
    ) -> Result<LifecycleProductResponse, ProductWorkflowError> {
        self.check(&context)?;
        self.inner.import_extension_bundle(context, bundle).await
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::UserId;
    use ironclaw_product_workflow::{LifecyclePackageKind, LifecycleProductSurfaceContext};
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct DispatchCounter(AtomicUsize);
    #[async_trait]
    impl LifecycleProductFacade for DispatchCounter {
        async fn execute(
            &self,
            _: LifecycleProductContext,
            _: LifecycleProductAction,
        ) -> Result<LifecycleProductResponse, ProductWorkflowError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ProductWorkflowError::UnknownInstallation)
        }
        async fn project_package(
            &self,
            _: LifecycleProductContext,
            _: LifecyclePackageRef,
        ) -> Result<LifecycleProductResponse, ProductWorkflowError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ProductWorkflowError::UnknownInstallation)
        }
        async fn import_extension_bundle(
            &self,
            _: LifecycleProductContext,
            _: Vec<u8>,
        ) -> Result<LifecycleProductResponse, ProductWorkflowError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(ProductWorkflowError::UnknownInstallation)
        }
    }
    fn context(tenant: &str) -> LifecycleProductContext {
        LifecycleProductContext::Surface(LifecycleProductSurfaceContext {
            tenant_id: TenantId::new(tenant).unwrap(),
            user_id: UserId::new("same-user").unwrap(),
            agent_id: None,
            project_id: None,
        })
    }
    #[tokio::test]
    async fn same_user_foreign_tenant_never_dispatches_lifecycle_or_import() {
        let counter = Arc::new(DispatchCounter(AtomicUsize::new(0)));
        let facade = TenantBoundLifecycleFacade::new(
            TenantId::new("owned-tenant").unwrap(),
            counter.clone(),
        );
        let package = LifecyclePackageRef::new(LifecyclePackageKind::Extension, "example").unwrap();
        for action in [
            LifecycleProductAction::ExtensionInstall {
                package_ref: package.clone(),
            },
            LifecycleProductAction::ExtensionActivate {
                package_ref: package.clone(),
            },
            LifecycleProductAction::ExtensionRemove {
                package_ref: package.clone(),
            },
            LifecycleProductAction::ExtensionList,
        ] {
            assert!(matches!(
                facade.execute(context("foreign-tenant"), action).await,
                Err(ProductWorkflowError::BindingAccessDenied)
            ));
        }
        assert!(matches!(
            facade
                .project_package(context("foreign-tenant"), package)
                .await,
            Err(ProductWorkflowError::BindingAccessDenied)
        ));
        assert!(matches!(
            facade
                .import_extension_bundle(context("foreign-tenant"), vec![1, 2, 3])
                .await,
            Err(ProductWorkflowError::BindingAccessDenied)
        ));
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        assert!(matches!(
            facade
                .execute(
                    context("owned-tenant"),
                    LifecycleProductAction::ExtensionList
                )
                .await,
            Err(ProductWorkflowError::UnknownInstallation)
        ));
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    }
}
