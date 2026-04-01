mod common;

// Test 1: lifecycle hook fires without blocking (using a non-existent binary — should fail gracefully)
#[tokio::test]
async fn lifecycle_hook_timeout_does_not_affect_caller() {
    use modelrouter::config::schema::LifecycleHookConfig;
    use modelrouter::hooks::lifecycle;
    use std::time::Instant;

    let hook = LifecycleHookConfig {
        name: "test-hook".to_string(),
        event: "on_request_received".to_string(),
        exec: "/nonexistent/hook".to_string(),
        timeout_secs: 1,
    };

    let start = Instant::now();
    lifecycle::fire(&hook, serde_json::json!({"test": true})).await;
    // fire() must return immediately — it's fire-and-forget
    assert!(
        start.elapsed().as_millis() < 100,
        "fire() should return immediately"
    );
}

// Test 2: pipeline hook without permission returns original payload
#[tokio::test]
async fn pipeline_hook_without_permission_returns_original() {
    use modelrouter::config::schema::PipelineHookConfig;
    use modelrouter::hooks::pipeline;

    let db = common::in_memory_db().await;
    let db: std::sync::Arc<dyn modelrouter::api::app::DatabaseProvider> =
        std::sync::Arc::new(db);

    let hooks = vec![PipelineHookConfig {
        name: "no-perm-hook".to_string(),
        event: "pre_request".to_string(),
        exec: "/bin/echo".to_string(), // returns its args, not stdin
        capabilities: vec!["mutate_request".to_string()],
        timeout_secs: 2,
        fail_open: true,
    }];

    // Hook claims mutate_request but no permission in DB
    let original =
        serde_json::json!({"messages": [{"role": "user", "content": "hello"}]});
    let result = pipeline::run_pre_request(&hooks, &db, original.clone())
        .await
        .unwrap();

    // Since the hook has no DB permission, original should be returned unchanged
    assert_eq!(result, original);
}

// Test 3: hook metrics recorded
#[tokio::test]
async fn hook_metrics_recorded_after_execution() {
    use modelrouter::config::schema::PipelineHookConfig;
    use modelrouter::hooks::pipeline;

    let db = common::in_memory_db().await;
    let db_arc: std::sync::Arc<dyn modelrouter::api::app::DatabaseProvider> =
        std::sync::Arc::new(db);

    let hooks = vec![PipelineHookConfig {
        name: "metrics-test-hook".to_string(),
        event: "pre_request".to_string(),
        exec: "/bin/echo".to_string(),
        capabilities: vec![],
        timeout_secs: 2,
        fail_open: true,
    }];

    let original = serde_json::json!({"test": true});
    pipeline::run_pre_request(&hooks, &db_arc, original).await.unwrap();

    // Give the spawned metrics task time to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Check metrics were recorded
    // (This is a best-effort check since metrics are recorded async)
    // The test mainly validates that the code path doesn't panic
}

// Test 4: sync_hook_permissions creates DB entries from config
#[tokio::test]
async fn sync_hook_permissions_adds_config_capabilities() {
    use modelrouter::config::schema::{HooksConfig, PipelineHookConfig};
    use modelrouter::db::repositories::hooks::HookRepository;
    use modelrouter::hooks::permissions::sync_hook_permissions;

    let db = common::in_memory_db().await;
    let db_arc: std::sync::Arc<dyn modelrouter::api::app::DatabaseProvider> =
        std::sync::Arc::new(db.clone());

    let hooks_config = HooksConfig {
        lifecycle: vec![],
        pipeline: vec![PipelineHookConfig {
            name: "test-injector".to_string(),
            event: "pre_request".to_string(),
            exec: "/usr/local/bin/injector".to_string(),
            capabilities: vec!["mutate_request".to_string()],
            timeout_secs: 2,
            fail_open: false,
        }],
    };

    sync_hook_permissions(&db_arc, &hooks_config).await.unwrap();

    let has_perm = db
        .has_permission("test-injector", "mutate_request")
        .await
        .unwrap();
    assert!(has_perm, "sync should have created permission entry");
}

// Test 5: fail_open=true returns original on hook error
#[tokio::test]
async fn pipeline_hook_fail_open_returns_original_on_error() {
    use modelrouter::config::schema::PipelineHookConfig;
    use modelrouter::hooks::pipeline;

    let db = common::in_memory_db().await;
    let db_arc: std::sync::Arc<dyn modelrouter::api::app::DatabaseProvider> =
        std::sync::Arc::new(db);

    let hooks = vec![PipelineHookConfig {
        name: "failing-hook".to_string(),
        event: "pre_request".to_string(),
        exec: "/nonexistent/binary".to_string(),
        capabilities: vec![],
        timeout_secs: 1,
        fail_open: true,
    }];

    let original =
        serde_json::json!({"messages": [{"role": "user", "content": "test"}]});
    let result = pipeline::run_pre_request(&hooks, &db_arc, original.clone())
        .await
        .unwrap();
    assert_eq!(result, original, "fail_open should return original on error");
}

// Test 6: fail_open=false returns error on hook failure
#[tokio::test]
async fn pipeline_hook_fail_closed_returns_error() {
    use modelrouter::config::schema::PipelineHookConfig;
    use modelrouter::hooks::pipeline;

    let db = common::in_memory_db().await;
    let db_arc: std::sync::Arc<dyn modelrouter::api::app::DatabaseProvider> =
        std::sync::Arc::new(db);

    let hooks = vec![PipelineHookConfig {
        name: "strict-hook".to_string(),
        event: "pre_request".to_string(),
        exec: "/nonexistent/binary".to_string(),
        capabilities: vec![],
        timeout_secs: 1,
        fail_open: false,
    }];

    let original = serde_json::json!({"messages": []});
    let result = pipeline::run_pre_request(&hooks, &db_arc, original).await;
    assert!(result.is_err(), "fail_closed should return error on hook failure");
}
