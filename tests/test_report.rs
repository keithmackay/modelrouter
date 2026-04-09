mod common;
use modelrouter::api::auth::hash_token;
use modelrouter::report;

async fn setup_test_data(pool: &sqlx::SqlitePool) {
    let now = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO users (name, enabled, created_at, metadata) VALUES ('alice', 1, ?, '{}')",
    )
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

    let rows = report::cost_by_user_window(&db.pool, "monthly", None, None, None)
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

    let rows = report::cost_by_user_window(&db.pool, "monthly", Some("alice"), None, None)
        .await
        .unwrap();
    assert!(
        rows.iter().all(|r| r.user_name == "alice"),
        "filter should only return alice"
    );

    let rows_other = report::cost_by_user_window(&db.pool, "monthly", Some("nonexistent"), None, None)
        .await
        .unwrap();
    assert!(rows_other.is_empty(), "nonexistent user should have no rows");
}

#[tokio::test]
async fn usage_report_groups_by_model() {
    let db = common::in_memory_db().await;
    setup_test_data(&db.pool).await;

    let rows = report::usage_by_model(&db.pool, None, None, None).await.unwrap();
    assert!(!rows.is_empty());
    let gpt4o = rows.iter().find(|r| r.model == "gpt-4o");
    assert!(gpt4o.is_some(), "gpt-4o should appear in usage report");
}

#[tokio::test]
async fn json_format_is_valid_parseable_json() {
    let db = common::in_memory_db().await;
    setup_test_data(&db.pool).await;

    let rows = report::cost_by_user_window(&db.pool, "monthly", None, None, None)
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
    use modelrouter::report::formatter::{write_rows, OutputFormat};

    let rows = vec![CostRow {
        user_name: "alice".to_string(),
        model: "gpt-4o".to_string(),
        total_cost_usd: 0.0075,
        total_tokens_in: 1000,
        total_tokens_out: 500,
        request_count: 3,
    }];

    let mut output = Vec::new();
    write_rows(
        &rows,
        &["User", "Model", "Cost (USD)", "Tokens In", "Tokens Out", "Requests"],
        |r| {
            vec![
                r.user_name.clone(),
                r.model.clone(),
                format!("{:.6}", r.total_cost_usd),
                r.total_tokens_in.to_string(),
                r.total_tokens_out.to_string(),
                r.request_count.to_string(),
            ]
        },
        OutputFormat::Csv,
        &mut output,
    ).expect("write_rows should succeed");

    let text = String::from_utf8(output).expect("valid utf8");
    let lines: Vec<&str> = text.lines().collect();
    assert!(!lines.is_empty(), "output should have lines");

    let header_line = lines[0];
    assert_eq!(header_line, "User,Model,Cost (USD),Tokens In,Tokens Out,Requests");

    let data_line = lines[1];
    assert!(data_line.starts_with("alice,gpt-4o,"));
    assert!(data_line.contains("1000"), "tokens_in should appear in data row");
}

#[tokio::test]
async fn hook_latency_stats_computes_correct_percentiles() {
    let db = common::in_memory_db().await;
    let pool = &db.pool;

    // Insert hook_metrics rows for two hook names.
    // hook_a durations: 10, 20, 30, 40, 50, 60, 70, 80, 90, 100 (10 rows)
    //   p50 = ceil(0.50*10) = 5th element (0-indexed: 4) = 50
    //   p95 = ceil(0.95*10) = 10th element (0-indexed: 9) = 100
    //   p99 = ceil(0.99*10) = 10th element (0-indexed: 9) = 100
    // hook_b durations: 5, 15 (2 rows)
    //   p50 = ceil(0.50*2) = 1st element (0-indexed: 0) = 5
    //   p95 = ceil(0.95*2) = 2nd element (0-indexed: 1) = 15
    //   p99 = ceil(0.99*2) = 2nd element (0-indexed: 1) = 15

    let now = chrono::Utc::now().to_rfc3339();
    for duration in [10i64, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
        sqlx::query(
            "INSERT INTO hook_metrics (hook_name, duration_ms, success, invoked_at) VALUES ('hook_a', ?, 1, ?)"
        )
        .bind(duration)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }
    for duration in [5i64, 15] {
        sqlx::query(
            "INSERT INTO hook_metrics (hook_name, duration_ms, success, invoked_at) VALUES ('hook_b', ?, 1, ?)"
        )
        .bind(duration)
        .bind(&now)
        .execute(pool)
        .await
        .unwrap();
    }

    let stats = report::hook_latency_stats(pool).await.unwrap();
    assert_eq!(stats.len(), 2, "should have stats for two hooks");

    let hook_a = stats.iter().find(|s| s.hook_name == "hook_a").expect("hook_a missing");
    assert_eq!(hook_a.invocation_count, 10);
    assert_eq!(hook_a.p50_duration_ms, 50, "hook_a p50 should be 50");
    assert_eq!(hook_a.p95_duration_ms, 100, "hook_a p95 should be 100");
    assert_eq!(hook_a.p99_duration_ms, 100, "hook_a p99 should be 100");
    assert!((hook_a.avg_duration_ms - 55.0).abs() < 0.001, "hook_a avg should be 55");

    let hook_b = stats.iter().find(|s| s.hook_name == "hook_b").expect("hook_b missing");
    assert_eq!(hook_b.invocation_count, 2);
    assert_eq!(hook_b.p50_duration_ms, 5, "hook_b p50 should be 5");
    assert_eq!(hook_b.p95_duration_ms, 15, "hook_b p95 should be 15");
    assert_eq!(hook_b.p99_duration_ms, 15, "hook_b p99 should be 15");
}

#[tokio::test]
async fn usage_by_model_filters_by_project() {
    let db = common::in_memory_db().await;
    let pool = &db.pool;
    let now = chrono::Utc::now().to_rfc3339();

    // Insert a user
    sqlx::query(
        "INSERT INTO users (name, enabled, created_at, metadata) VALUES ('bob', 1, ?, '{}')",
    )
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();
    let user_id: (i64,) = sqlx::query_as("SELECT id FROM users WHERE name = 'bob'")
        .fetch_one(pool)
        .await
        .unwrap();

    // Insert two prompts (one per ledger row)
    let prompt_a: (i64,) = sqlx::query_as(
        "INSERT INTO prompts (user_id, request_model, routed_model, provider, messages, prompt_tokens, completion_tokens, cost_usd, created_at, tags) VALUES (?, 'model-a', 'model-a', 'openai', '[]', 10, 5, 0.001, ?, '[]') RETURNING id"
    )
    .bind(user_id.0)
    .bind(&now)
    .fetch_one(pool)
    .await
    .unwrap();

    let prompt_b: (i64,) = sqlx::query_as(
        "INSERT INTO prompts (user_id, request_model, routed_model, provider, messages, prompt_tokens, completion_tokens, cost_usd, created_at, tags) VALUES (?, 'model-b', 'model-b', 'anthropic', '[]', 10, 5, 0.002, ?, '[]') RETURNING id"
    )
    .bind(user_id.0)
    .bind(&now)
    .fetch_one(pool)
    .await
    .unwrap();

    // Insert cost_ledger row for proj-a
    sqlx::query(
        "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project, tokens_in, tokens_out, cost_usd, created_at) VALUES (?, ?, 'model-a', 'openai', 'proj-a', 10, 5, 0.001, ?)"
    )
    .bind(user_id.0)
    .bind(prompt_a.0)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();

    // Insert cost_ledger row for proj-b
    sqlx::query(
        "INSERT INTO cost_ledger (user_id, prompt_id, model, provider, project, tokens_in, tokens_out, cost_usd, created_at) VALUES (?, ?, 'model-b', 'anthropic', 'proj-b', 10, 5, 0.002, ?)"
    )
    .bind(user_id.0)
    .bind(prompt_b.0)
    .bind(&now)
    .execute(pool)
    .await
    .unwrap();

    // Filter by proj-a — should only return model-a
    let rows = report::usage_by_model(pool, None, None, Some("proj-a"))
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "should return exactly one row for proj-a");
    assert_eq!(rows[0].model, "model-a", "proj-a row should be model-a");

    // Confirm proj-b row is excluded
    assert!(
        rows.iter().all(|r| r.model != "model-b"),
        "proj-b rows should be excluded"
    );
}
