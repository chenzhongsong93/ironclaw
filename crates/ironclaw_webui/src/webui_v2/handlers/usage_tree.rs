//! generic WebUI adapter；作为handlers私有子模块，不包含TQ资产/金额/结算操作。
use super::*;

const MAX_PAGES: usize = 32;
const MAX_PAGE_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_PAGES * MAX_PAGE_BYTES + 16 * 1024;
const READ_BUDGET: Duration = Duration::from_secs(30);

fn not_ready() -> WebUiV2HttpError {
    RebornServicesError {
        code: RebornServicesErrorCode::Unavailable,
        kind: ironclaw_product_workflow::RebornServicesErrorKind::ServiceUnavailable,
        status_code: 503,
        retryable: true,
        field: None,
        validation_code: None,
    }
    .into()
}

pub async fn read_usage_tree(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<WebUiAuthenticatedCaller>,
    Path((thread_id, run_id)): Path<(String, String)>,
) -> Result<Response, WebUiV2HttpError> {
    let end = tokio::time::Instant::now() + READ_BUDGET;
    let read = async {
        let services = state.services();
        let mut session = services
            .open_run_tree_usage(
                caller.clone(),
                ironclaw_product_workflow::RebornGetRunStateRequest { thread_id, run_id },
            )
            .await?;
        let scope = session.root_scope().clone();
        let actor = session.original_actor().clone();
        let generation = session.log_generation().to_owned();
        let root = session.root_run_id();
        let mut cursor: Option<String> = None;
        let mut pages = Vec::new();
        let mut sequence = None;
        loop {
            if tokio::time::Instant::now() >= end || pages.len() >= MAX_PAGES {
                return Err(not_ready());
            }
            let page = services
                .read_run_tree_usage_page(caller.clone(), &mut session, cursor.as_deref(), 128)
                .await?;
            if tokio::time::Instant::now() >= end
                || page.root_run_id != root
                || page.permanent_closed
                || sequence.is_some_and(|h| h != page.snapshot_sequence)
                || serde_json::to_vec(&page).map_err(|_| not_ready())?.len() > MAX_PAGE_BYTES
            {
                return Err(not_ready());
            }
            sequence = Some(page.snapshot_sequence);
            let done = page.pages_complete_at_sequence;
            cursor = page.next_cursor.clone();
            if done != cursor.is_none() {
                return Err(not_ready());
            }
            pages.push(page);
            if done {
                break;
            }
        }
        // 最后一页的facade调用已在返回前复核原generation/实际caller与全成员ACL。
        // 这里仅序列化其原对象，不再开启另一session或用DTO合成缺页。
        let body = serde_json::to_vec(&serde_json::json!({
            "schema":"OwnedRunTreeUsageExport/1","root_scope":scope,"original_actor":actor,
            "log_generation":generation,"root_run_id":root,"snapshot_sequence":sequence,
            "pages":pages,
        }))
        .map_err(|_| not_ready())?;
        if body.len() > MAX_RESPONSE_BYTES || tokio::time::Instant::now() >= end {
            return Err(not_ready());
        }
        Ok::<_, WebUiV2HttpError>(
            (
                [
                    (header::CONTENT_TYPE, "application/json"),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                body,
            )
                .into_response(),
        )
    };
    // 纯read限额，不产生run、模型、资金、永久seal；没有独立spawn/detach任务。
    tokio::time::timeout_at(end, read)
        .await
        .map_err(|_| not_ready())?
}
