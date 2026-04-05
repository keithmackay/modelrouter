// tests/test_anthropic_cache.rs
use modelrouter::providers::anthropic::translate_messages;
use serde_json::json;

#[test]
fn string_system_message_still_works() {
    let messages = vec![
        json!({"role": "system", "content": "Be helpful."}),
        json!({"role": "user", "content": "Hi"}),
    ];
    let (system, filtered) = translate_messages(&messages);
    assert_eq!(system.as_deref(), Some("Be helpful."));
    assert_eq!(filtered.len(), 1);
}

#[test]
fn array_content_system_message_text_is_extracted() {
    let messages = vec![
        json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "Be helpful.", "cache_control": {"type": "ephemeral"}}
            ]
        }),
        json!({"role": "user", "content": "Hi"}),
    ];
    let (system, filtered) = translate_messages(&messages);
    assert_eq!(system.as_deref(), Some("Be helpful."));
    assert_eq!(filtered.len(), 1);
}

#[test]
fn array_content_user_message_preserved_as_array() {
    let messages = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Hello", "cache_control": {"type": "ephemeral"}}
        ]
    })];
    let (system, filtered) = translate_messages(&messages);
    assert!(system.is_none());
    assert_eq!(filtered.len(), 1);
    assert!(filtered[0]["content"].is_array(), "array content must be preserved");
    assert_eq!(filtered[0]["content"][0]["cache_control"]["type"], "ephemeral");
}

#[test]
fn message_with_null_content_is_excluded() {
    let messages = vec![
        json!({"role": "user", "content": null}),
        json!({"role": "assistant", "content": "Hi"}),
    ];
    let (_, filtered) = translate_messages(&messages);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0]["role"], "assistant");
}

#[test]
fn multiple_text_blocks_in_system_array_are_joined_with_newline() {
    let messages = vec![
        json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "Block one."},
                {"type": "text", "text": "Block two."}
            ]
        }),
        json!({"role": "user", "content": "Hi"}),
    ];
    let (system, _) = translate_messages(&messages);
    assert_eq!(system.as_deref(), Some("Block one.\nBlock two."));
}
