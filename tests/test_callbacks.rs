mod common;
use modelrouter::callbacks::{CallbackDispatcher, CallbackEvent};

#[tokio::test]
async fn dispatcher_with_no_backends_is_a_no_op() {
    let dispatcher = CallbackDispatcher::new(vec![]);
    dispatcher.dispatch(CallbackEvent {
        trace_id: "test-id".to_string(),
        user_id: 1,
        model: "gpt-4o".to_string(),
        provider: "openai".to_string(),
        input: serde_json::json!([{"role": "user", "content": "Hello"}]),
        output: "Hello back".to_string(),
        prompt_tokens: 10,
        completion_tokens: 5,
        cost_usd: 0.001,
        latency_ms: 200,
    });
    // No panic = pass
}
