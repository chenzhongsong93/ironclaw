//! Fixed startup diagnostics. Neither the original error nor config enters the reporter.
use crate::runtime::{RebornStartupStage, reborn_startup_stage};

pub(super) fn report_failure<T, E>(
    stage: RebornStartupStage,
    result: Result<T, E>,
) -> Result<T, E> {
    report_failure_with(stage, result, reborn_startup_stage)
}

fn report_failure_with<T, E>(
    stage: RebornStartupStage,
    result: Result<T, E>,
    report: impl FnOnce(RebornStartupStage),
) -> Result<T, E> {
    if result.is_err() {
        report(stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // TEST-SPEC-normal-runtime-llm-config-observation LC05.
    #[test]
    fn success_preserves_value_without_reporting_failure() {
        let value = Arc::new(17);
        let actual = report_failure_with(
            RebornStartupStage::LlmReloadConfigReadFailed,
            Ok::<_, Arc<str>>(Arc::clone(&value)),
            |_| panic!("success must not emit failure"),
        )
        .unwrap();
        assert!(Arc::ptr_eq(&value, &actual));
    }

    #[test]
    fn failure_preserves_original_error_and_reports_only_the_bound_stage() {
        for stage in [
            RebornStartupStage::LlmReloadConfigReadFailed,
            RebornStartupStage::LlmReloadSelectionFailed,
            RebornStartupStage::LlmReloadCredentialReadFailed,
            RebornStartupStage::LlmReloadDefaultBuildFailed,
        ] {
            let original: Arc<str> = Arc::from("private diagnostic fixture");
            let mut seen = Vec::new();
            let actual =
                report_failure_with(stage, Err::<(), _>(Arc::clone(&original)), |s| seen.push(s))
                    .unwrap_err();
            assert!(Arc::ptr_eq(&original, &actual));
            assert_eq!(seen, vec![stage]);
        }
    }

    #[test]
    fn private_error_is_not_formatted_for_diagnostics() {
        struct PrivateError;
        impl std::fmt::Debug for PrivateError {
            fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                panic!("diagnostics must not format the private error")
            }
        }
        let original = Arc::new(PrivateError);
        let actual = report_failure_with(
            RebornStartupStage::LlmReloadCredentialReadFailed,
            Err::<(), _>(Arc::clone(&original)),
            |_| {},
        );
        match actual {
            Err(error) => assert!(Arc::ptr_eq(&error, &original)),
            Ok(_) => panic!("original failure was changed"),
        }
    }
}
