mod support {
    pub mod runtime_fixture;
}
use support::runtime_fixture as fixture;
#[tokio::test(flavor = "current_thread")]
async fn roundtrip() {
    fixture::roundtrip().await;
}
#[tokio::test(flavor = "current_thread")]
async fn queued_success_cancel() {
    fixture::cancel_at_completion().await;
}
#[tokio::test(flavor = "current_thread")]
async fn late_known_tool_outcome() {
    fixture::tool_cancel().await;
}
#[tokio::test(flavor = "current_thread")]
async fn intent_fsync_cancel() {
    fixture::durable().await;
}
#[test]
fn public_batch_cannot_bypass_durable_policy() {
    fixture::durable_bypass();
}
#[test]
fn mixed_registry_safe_batch() {
    fixture::mixed_registry();
}
#[tokio::test(flavor = "current_thread")]
async fn admission() {
    fixture::admission().await;
}
#[tokio::test(flavor = "current_thread")]
async fn resync() {
    fixture::resync().await;
}
#[tokio::test(flavor = "current_thread")]
async fn invalid_response() {
    fixture::invalid_response().await;
}
#[tokio::test(flavor = "current_thread")]
async fn auth_cancel() {
    fixture::auth_cancel().await;
}
#[tokio::test(flavor = "current_thread")]
async fn auth_drop() {
    fixture::auth_drop().await;
}
#[tokio::test(flavor = "current_thread")]
async fn preview_flood() {
    fixture::preview_flood().await;
}
#[tokio::test(flavor = "current_thread")]
async fn shutdown() {
    fixture::shutdown().await;
}
#[tokio::test(flavor = "current_thread")]
async fn restart_limit() {
    fixture::restart_limit().await;
}
#[tokio::test(flavor = "current_thread")]
async fn known_outcome_storage_failure() {
    fixture::storage_failure().await;
}
#[tokio::test(flavor = "current_thread")]
async fn summary_commit() {
    fixture::summary(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn late_summary_cancel() {
    fixture::summary(true).await;
}
#[tokio::test(flavor = "current_thread")]
async fn checkpoint_cancellation() {
    fixture::checkpoint_cancellation().await;
}
#[tokio::test(flavor = "current_thread")]
async fn restart_resume() {
    fixture::restart_resume().await;
}
#[tokio::test(flavor = "current_thread")]
async fn plan_admission() {
    fixture::plan_admission().await;
}
#[tokio::test(flavor = "current_thread")]
async fn unknown_restart() {
    fixture::unknown_restart().await;
}
#[tokio::test(flavor = "current_thread")]
async fn unresolved_restart() {
    fixture::unresolved_restart().await;
}
#[tokio::test(flavor = "current_thread")]
async fn multi_round() {
    fixture::multi_round().await;
}
#[tokio::test(flavor = "current_thread")]
async fn fast_events() {
    fixture::fast_events().await;
}
