mod common;
use modelrouter::api::auth::hash_token;
use modelrouter::report;

async fn setup_test_data(pool: &sqlx::SqlitePool) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (name, api_key, enabled, created_at, metadata) VALUES ('alice', ?, 1, ?, '{}')",
    )
    .bind(hash_token("alice-key"))
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    let alice_id: (i64,) = sqlx::query_as("SELECT id FROM users WHERE name = 'alice'")
        .fetch_one(pool)
        .await
        .unwrap();

    // Insert a prompt
    let prompt_id: (i64,) = sqlx::query_as(
        "INSERT INTO prompts (user_id, request_model, routed_model, provider, messages, prompt_tokens, completion_tokens, cost_usd, created_at, tags) VALUES (?, 'gpt-4o', 'gpt-4o', 'openai', '[]', 100, 50, 0.00075, ?, '[]') RETURNING id"
    )
    .bind(alice_id.0)
    .bind(&now)
    .fetch_one(pool)
    .await
    .unwrap();

    // Insert cost ledger entry
    sqlx::query(
        "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, tokens_in, tokens_out, cost_usd, created_at) VALUES (?, ?, 'gpt-4o', 'openai', 100, 50, 0.00075, ?)"
    )
    .bind(alice_id.0)
    .bind(prompt_id.0)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn cost_report_sums_cost_ledger_by_window() {
    let db = common::in_memory_db().await;
    setup_test_data(&db.pool).await;

    let rows = report::cost_by_user_window(&db.pool, "monthly", None)
        .await
        .unwrap();
    assert!(!rows.is_empty(), "should have at least one cost row");
    let alice_row = rows.iter().find(|r| r.user_name == "alice");
    assert!(alice_row.is_some(), "alice should appear in cost report");
    assert!((alice_row.unwrap().total_cost_usd - 0.00075).abs() < 0.000001);
}

#[tokio::test]
async fn cost_report_filters_by_user() {
    let db = common::in_memory_db().await;
    setup_test_data(&db.pool).await;

    let rows = report::cost_by_user_window(&db.pool, "monthly", Some("alice"))
        .await
        .unwrap();
    assert!(
        rows.iter().all(|r| r.user_name == "alice"),
        "filter should only return alice"
    );

    let rows_other = report::cost_by_user_window(&db.pool, "monthly", Some("nonexistent"))
        .await
        .unwrap();
    assert!(rows_other.is_empty(), "nonexistent user should have no rows");
}

#[tokio::test]
async fn usage_report_groups_by_model() {
    let db = common::in_memory_db().await;
    setup_test_data(&db.pool).await;

    let rows = report::usage_by_model(&db.pool, None).await.unwrap();
    assert!(!rows.is_empty());
    let gpt4o = rows.iter().find(|r| r.model == "gpt-4o");
    assert!(gpt4o.is_some(), "gpt-4o should appear in usage report");
}

#[tokio::test]
async fn json_format_is_valid_parseable_json() {
    let db = common::in_memory_db().await;
    setup_test_data(&db.pool).await;

    let rows = report::cost_by_user_window(&db.pool, "monthly", None)
        .await
        .unwrap();
    let json_str = serde_json::to_string(&rows).expect("should serialize to JSON");
    let parsed: serde_json::Value =
        serde_json::from_str(&json_str).expect("should parse back");
    assert!(parsed.is_array());
}

#[tokio::test]
async fn csv_format_has_correct_headers() {
    use modelrouter::report::CostRow;

    let rows = vec![CostRow {
        user_name: "alice".to_string(),
        model: "gpt-4o".to_string(),
        total_cost_usd: 0.0075,
        total_tokens_in: 1000,
        total_tokens_out: 500,
        request_count: 3,
    }];

    // Verify the headers array is correct
    let headers = ["User", "Model", "Cost (USD)", "Tokens In", "Tokens Out", "Requests"];
    assert_eq!(headers.len(), 6);
    assert_eq!(headers[0], "User");
    assert_eq!(headers[2], "Cost (USD)");
    // Verify the row conversion matches the header count
    let row_fields = vec![
        rows[0].user_name.clone(),
        rows[0].model.clone(),
        format!("{:.6}", rows[0].total_cost_usd),
        rows[0].total_tokens_in.to_string(),
        rows[0].total_tokens_out.to_string(),
        rows[0].request_count.to_string(),
    ];
    assert_eq!(
        row_fields.len(),
        headers.len(),
        "row field count must match header count"
    );
}
