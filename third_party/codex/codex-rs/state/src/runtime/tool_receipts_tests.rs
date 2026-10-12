// SPDX-License-Identifier: Apache-2.0
use super::tests::observe;
use super::tests::setup;
#[tokio::test]
async fn cell_receipts_are_bounded_to_the_current_thread_and_owner_scope() {
    let (store, mut request) = setup().await;
    for index in 0..8 {
        request.logical_id = format!("call-{index}");
        observe(&store, &request).await;
    }
    let receipts = store
        .list_tool_cell_receipts(&request.thread_id, "cell-1", "origin-scope")
        .await
        .unwrap();
    assert_eq!(
        receipts
            .iter()
            .map(|fact| fact.request.logical_id.as_str())
            .collect::<Vec<_>>(),
        ["call-7", "call-6", "call-5", "call-4"]
    );
    assert!(
        store
            .list_tool_cell_receipts("foreign-thread", "cell-1", "origin-scope")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .list_tool_cell_receipts(&request.thread_id, "cell-1", "previous-owner-scope")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .list_tool_cell_receipts(&request.thread_id, "other-cell", "origin-scope")
            .await
            .unwrap()
            .is_empty()
    );
}
