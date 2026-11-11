use super::*;
use serde_json::json;

fn msgs(v: Value) -> Vec<Value> {
    v.as_array().unwrap().clone()
}

// ── Transcript ────────────────────────────────────────────────────────────────

#[test]
fn transcript_splits_system_and_reads_text_parts() {
    let t = Transcript::from_chat_messages(&msgs(json!([
        {"role": "system", "content": "be brief"},
        {"role": "developer", "content": [{"type": "text", "text": "and kind"}]},
        {"role": "user", "content": [{"type": "text", "text": "a"}, {"type": "input_text", "text": "b"}]},
    ])))
    .unwrap();
    assert_eq!(t.system.as_deref(), Some("be brief\n\nand kind"));
    assert_eq!(t.turns, vec![Turn { role: "user".into(), text: "a\nb".into() }]);
}

#[test]
fn transcript_refuses_what_a_session_cannot_carry() {
    let cases = [
        json!([{"role": "user", "content": [{"type": "image_url", "image_url": {"url": "x"}}]}]),
        json!([{"role": "user", "content": "q"}, {"role": "assistant", "content": null, "tool_calls": [{}]}, {"role": "user", "content": "q"}]),
        json!([{"role": "tool", "content": "r"}, {"role": "user", "content": "q"}]),
        json!([{"role": "user", "content": "q"}, {"role": "assistant", "content": "a"}]),
        json!([{"role": "narrator", "content": "q"}]),
        json!([{"role": "user", "content": 7}]),
        json!([]),
    ];
    for case in cases {
        assert!(Transcript::from_chat_messages(&msgs(case.clone())).is_err(), "{case}");
    }
}

#[test]
fn render_sends_one_turn_bare_and_a_history_labelled() {
    let one = [Turn { role: "user".into(), text: "hi".into() }];
    assert_eq!(render(&one), "hi");
    let many = [
        Turn { role: "user".into(), text: "q1".into() },
        Turn { role: "assistant".into(), text: "a1".into() },
        Turn { role: "user".into(), text: "q2".into() },
    ];
    let text = render(&many);
    assert!(text.ends_with("User: q1\n\nAssistant: a1\n\nUser: q2"), "{text}");
    assert_eq!(render_new(&many[2..]), "q2");
}

#[test]
fn resumable_prefix_ends_at_the_last_assistant_turn() {
    let t = Transcript::from_chat_messages(&msgs(json!([
        {"role": "user", "content": "q1"},
        {"role": "assistant", "content": "a1"},
        {"role": "user", "content": "q2"},
        {"role": "user", "content": "q3"},
    ])))
    .unwrap();
    assert_eq!(t.resumable_prefix(), Some(2));
    let first = Transcript::from_chat_messages(&msgs(json!([{"role": "user", "content": "q"}]))).unwrap();
    assert_eq!(first.resumable_prefix(), None);
}

#[test]
fn prefix_hash_covers_the_system_prompt_and_ignores_extra_fields() {
    let a = Transcript::from_chat_messages(&msgs(json!([
        {"role": "user", "content": "q"}, {"role": "assistant", "content": "a"}, {"role": "user", "content": "n"},
    ])))
    .unwrap();
    let b = Transcript::from_chat_messages(&msgs(json!([
        {"role": "user", "content": [{"type": "text", "text": "q"}]},
        {"role": "assistant", "content": "a", "refusal": null},
        {"role": "user", "content": "different"},
    ])))
    .unwrap();
    assert_eq!(a.prefix_hash(2), b.prefix_hash(2));
    let mut c = a.clone();
    c.system = Some("s".into());
    assert_ne!(a.prefix_hash(2), c.prefix_hash(2));
}

// ── Store and routing ─────────────────────────────────────────────────────────

fn key(transcript: &str) -> SessionKey {
    SessionKey {
        scope: SessionScope { user_id: 1, project: None },
        tool: Tool::Claude,
        model: "m".into(),
        transcript: transcript.into(),
    }
}

#[test]
fn a_claimed_session_is_gone_and_an_idle_one_expires() {
    let store = SessionStore::default();
    store.remember(key("t"), "s1".into());
    assert_eq!(store.claim(&key("t"), Duration::from_secs(60)), Some("s1".into()));
    assert_eq!(store.claim(&key("t"), Duration::from_secs(60)), None);

    store.remember(key("u"), "s2".into());
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(store.claim(&key("u"), Duration::from_millis(1)), None);
    assert!(store.is_empty());
}

#[test]
fn subscription_provider_follows_the_config() {
    let mut cfg = SubscriptionConfig::default();
    assert_eq!(subscription_provider(&cfg, "anthropic"), None);
    cfg.enabled = true;
    assert_eq!(subscription_provider(&cfg, "anthropic"), Some("claude-cli"));
    assert_eq!(subscription_provider(&cfg, "openai"), Some("codex-cli"));
    assert_eq!(subscription_provider(&cfg, "gemini"), Some("gemini-cli"));
    assert_eq!(subscription_provider(&cfg, "vertex"), None);
    cfg.providers = vec!["openai".into()];
    assert_eq!(subscription_provider(&cfg, "anthropic"), None);
    for tool in Tool::ALL {
        assert_eq!(Tool::from_provider_name(tool.provider_name()), Some(tool));
    }
    assert_eq!(Tool::from_provider_name("anthropic"), None);
}

#[test]
fn sanitized_project_directories_stay_distinct() {
    assert_ne!(sanitize("a/b"), sanitize("a_b"));
    assert!(sanitize("../etc").chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
}

// ── Per-CLI arguments and parsing ─────────────────────────────────────────────

fn inv(resume: Option<&str>, system: Option<&str>) -> Invocation {
    Invocation {
        model: "m1".into(),
        resume: resume.map(String::from),
        system: system.map(String::from),
        prompt: "hello".into(),
    }
}

#[test]
fn claude_args_disable_tools_and_resume_by_id() {
    let fresh = claude::args(&inv(None, None));
    assert!(fresh.windows(2).any(|w| w == ["--model", "m1"]));
    assert!(fresh.windows(2).any(|w| w == ["--tools", ""]));
    assert!(fresh.windows(2).any(|w| w[0] == "--system-prompt" && w[1] == claude::DEFAULT_SYSTEM_PROMPT));
    assert!(!fresh.contains(&"--resume".to_string()));
    let resumed = claude::args(&inv(Some("abc"), Some("sys")));
    assert!(resumed.windows(2).any(|w| w == ["--resume", "abc"]));
    assert!(resumed.windows(2).any(|w| w == ["--system-prompt", "sys"]));
    assert_eq!(claude::stdin(&inv(None, Some("sys"))), "hello");
}

#[test]
fn claude_parse_reads_usage_and_errors() {
    let ok = r#"{"type":"result","subtype":"success","is_error":false,"result":"OK","session_id":"s-1",
        "usage":{"input_tokens":10,"output_tokens":42,"cache_read_input_tokens":3,"cache_creation_input_tokens":6730,
        "output_tokens_details":{"thinking_tokens":35}}}"#;
    let r = claude::parse(ok).unwrap();
    assert_eq!(r.text, "OK");
    assert_eq!(r.session_id.as_deref(), Some("s-1"));
    assert_eq!((r.input_tokens, r.output_tokens, r.cache_read_tokens, r.cache_write_tokens), (10, 42, 3, 6730));
    assert_eq!(r.reasoning_tokens, Some(35));

    let err = claude::parse(r#"{"is_error":true,"subtype":"success","result":"Credit balance too low","api_error_status":402}"#)
        .unwrap_err();
    assert!(err.to_string().contains("Credit balance too low") && err.to_string().contains("402"), "{err}");
    assert!(claude::parse(r#"{"subtype":"error_max_turns","is_error":false}"#).is_err());
    assert!(claude::parse("not json").is_err());
}

#[test]
fn codex_args_and_system_prompt_placement() {
    let fresh = codex::args(&inv(None, None));
    assert_eq!(fresh.first().map(String::as_str), Some("exec"));
    assert!(fresh.windows(2).any(|w| w == ["--sandbox", "read-only"]));
    assert_eq!(fresh.last().map(String::as_str), Some("-"));
    let resumed = codex::args(&inv(Some("t-1"), None));
    assert_eq!(&resumed[..2], ["exec", "resume"]);
    assert!(resumed.windows(2).any(|w| w == ["t-1", "-"]));

    assert_eq!(codex::stdin(&inv(None, Some("sys"))), "<system>\nsys\n</system>\n\nhello");
    assert_eq!(codex::stdin(&inv(Some("t-1"), Some("sys"))), "hello");
    assert_eq!(codex::stdin(&inv(None, None)), "hello");
}

#[test]
fn codex_parse_reads_the_event_stream() {
    let ok = "{\"type\":\"thread.started\",\"thread_id\":\"t-9\"}\n\
              {\"type\":\"turn.started\"}\n\
              noise that is not json\n\
              {\"type\":\"item.completed\",\"item\":{\"type\":\"reasoning\",\"text\":\"thinking\"}}\n\
              {\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"PLUM\"}}\n\
              {\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":27229,\"cached_input_tokens\":20224,\
              \"cache_write_input_tokens\":5,\"output_tokens\":11,\"reasoning_output_tokens\":2}}\n";
    let r = codex::parse(ok).unwrap();
    assert_eq!(r.session_id.as_deref(), Some("t-9"));
    assert_eq!(r.text, "PLUM");
    assert_eq!((r.input_tokens, r.cache_read_tokens, r.cache_write_tokens), (7005, 20224, 5));
    assert_eq!((r.output_tokens, r.reasoning_tokens), (11, Some(2)));

    let failed = "{\"type\":\"turn.failed\",\"error\":{\"message\":\"model not supported\"}}";
    assert!(codex::parse(failed).unwrap_err().to_string().contains("model not supported"));
    // An upstream API error relayed as JSON in the message string.
    let relayed = r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"message\":\"The 'x' model is not supported\"}}"}}"#;
    let err = codex::parse(relayed).unwrap_err();
    assert!(err.is::<VendorError>());
    assert_eq!(err.to_string(), "codex reported an error: The 'x' model is not supported");
    assert!(codex::parse("{\"type\":\"error\",\"message\":\"boom\"}").is_err());
    assert!(codex::parse("{\"type\":\"thread.started\",\"thread_id\":\"t\"}").is_err());
}

#[test]
fn gemini_args_and_parse() {
    let fresh = gemini::args(&inv(None, None));
    assert!(fresh.windows(2).any(|w| w == ["--output-format", "json"]));
    assert!(fresh.windows(2).any(|w| w == ["-m", "m1"]));
    assert!(gemini::args(&inv(Some("g-1"), None)).windows(2).any(|w| w == ["--resume", "g-1"]));
    assert!(gemini::stdin(&inv(None, Some("sys"))).starts_with("<system>"));

    let ok = r#"{"session_id":"g-2","response":"hi","stats":{"models":{
        "gemini-2.5-pro":{"tokens":{"prompt":100,"candidates":20,"cached":40,"thoughts":7,"total":127}},
        "gemini-2.5-flash":{"tokens":{"prompt":10,"candidates":2,"cached":0,"total":12}}}}}"#;
    let r = gemini::parse(ok).unwrap();
    assert_eq!(r.session_id.as_deref(), Some("g-2"));
    assert_eq!(r.text, "hi");
    assert_eq!((r.input_tokens, r.cache_read_tokens, r.output_tokens), (70, 40, 22));
    assert_eq!(r.reasoning_tokens, Some(7));

    assert!(gemini::parse(r#"{"error":{"type":"auth","message":"login required"}}"#)
        .unwrap_err()
        .to_string()
        .contains("login required"));
    assert!(gemini::parse("nope").is_err());
}

// ── The adapter against a fake CLI ────────────────────────────────────────────

/// A stand-in `claude`: logs its arguments, answers `echo:<prompt>` and
/// keeps the session id it was asked to resume (else mints a new one).
#[cfg(unix)]
fn fake_claude(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("fake-claude");
    std::fs::write(
        &script,
        r#"#!/bin/sh
log="$(dirname "$0")/calls.log"
session="new-$$"
prev=""
for a in "$@"; do
  if [ "$prev" = "--resume" ]; then session="$a"; fi
  prev="$a"
done
prompt="$(cat)"
echo "$session|$PWD|${ANTHROPIC_API_KEY:-unset}|$*" >> "$log"
printf '{"type":"result","subtype":"success","is_error":false,"result":"echo:%s","session_id":"%s","usage":{"input_tokens":5,"output_tokens":2,"cache_read_input_tokens":1,"cache_creation_input_tokens":0}}' "$(echo "$prompt" | tr '\n' ' ')" "$session"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
fn adapter_with(script: &Path, work: &Path) -> CliSessionAdapter {
    let mut cfg = SubscriptionConfig { enabled: true, ..Default::default() };
    cfg.claude.command = script.display().to_string();
    cfg.work_dir = work.display().to_string();
    CliSessionAdapter::new(Tool::Claude, &cfg)
}

#[cfg(unix)]
fn request(messages: Value, user_id: i64, project: Option<&str>) -> NormalizedRequest {
    NormalizedRequest {
        model: "claude-x".into(),
        messages: msgs(messages),
        session_scope: Some(SessionScope { user_id, project: project.map(String::from) }),
        ..Default::default()
    }
}

#[cfg(unix)]
fn calls(dir: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(dir.join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(|l| l.splitn(4, '|').map(String::from).collect())
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn a_continued_conversation_resumes_its_session_with_only_the_new_turn() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = adapter_with(&fake_claude(tmp.path()), &tmp.path().join("work"));

    let first = adapter
        .complete(&request(json!([{"role": "user", "content": "q1"}]), 1, Some("proj")))
        .await
        .unwrap();
    assert_eq!(first.content, "echo:q1 ");
    assert_eq!((first.prompt_tokens, first.completion_tokens, first.cache_read_tokens), (5, 2, 1));

    let second = adapter
        .complete(&request(
            json!([
                {"role": "user", "content": "q1"},
                {"role": "assistant", "content": first.content},
                {"role": "user", "content": "q2"},
            ]),
            1,
            Some("proj"),
        ))
        .await
        .unwrap();
    assert_eq!(second.content, "echo:q2 ", "only the new turn is sent");

    let log = calls(tmp.path());
    assert_eq!(log.len(), 2);
    assert!(log[0][3].split(' ').all(|a| a != "--resume"));
    assert!(log[1][3].contains(&format!("--resume {}", log[0][0])), "{:?}", log[1]);
    assert_eq!(log[0][1], log[1][1], "same working directory");
    assert!(log[0][1].contains("claude-cli/user-1/proj-"));
    assert_eq!(log[0][2], "unset", "the API key is withheld");
}

#[cfg(unix)]
#[tokio::test]
async fn a_history_the_adapter_does_not_hold_starts_a_new_session() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = adapter_with(&fake_claude(tmp.path()), &tmp.path().join("work"));
    let first = adapter
        .complete(&request(json!([{"role": "user", "content": "q1"}]), 1, None))
        .await
        .unwrap();
    let continued = |user_id: i64, assistant: &str| {
        request(
            json!([
                {"role": "user", "content": "q1"},
                {"role": "assistant", "content": assistant},
                {"role": "user", "content": "q2"},
            ]),
            user_id,
            None,
        )
    };

    // Another user with the same history, and an edited history, both start
    // fresh and replay the transcript.
    let other = adapter.complete(&continued(2, &first.content)).await.unwrap();
    assert!(other.content.contains("User: q1"), "{}", other.content);
    adapter.complete(&continued(1, "an edited answer")).await.unwrap();

    // The real continuation resumes — once. Its session is then claimed and
    // re-filed under the longer transcript, so a replay of the same request
    // starts over.
    adapter.complete(&continued(1, &first.content)).await.unwrap();
    adapter.complete(&continued(1, &first.content)).await.unwrap();

    let resumed: Vec<bool> = calls(tmp.path()).iter().map(|c| c[3].contains("--resume")).collect();
    assert_eq!(resumed, vec![false, false, false, true, false]);
}

#[cfg(unix)]
#[tokio::test]
async fn a_failing_cli_is_an_error_naming_it() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("broken");
    std::fs::write(&script, "#!/bin/sh\ncat >/dev/null\necho 'not logged in' >&2\nexit 3\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let adapter = adapter_with(&script, &tmp.path().join("work"));
    let err = adapter
        .complete(&request(json!([{"role": "user", "content": "q"}]), 1, None))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not logged in") && err.contains("exit"), "{err}");
    assert!(adapter.sessions().is_empty());

    // A CLI that exits non-zero but reports the reason in its output: the
    // reason is what the caller sees.
    let reporting = tmp.path().join("reporting");
    std::fs::write(
        &reporting,
        "#!/bin/sh\ncat >/dev/null\necho '{\"is_error\":true,\"result\":\"Not logged in\"}'\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&reporting, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = adapter_with(&reporting, &tmp.path().join("work"))
        .complete(&request(json!([{"role": "user", "content": "q"}]), 1, None))
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(err, "claude reported an error: Not logged in");

    let mut with_tools = request(json!([{"role": "user", "content": "q"}]), 1, None);
    with_tools.tools = Some(vec![json!({"type": "function"})]);
    assert!(adapter.complete(&with_tools).await.unwrap_err().to_string().contains("tool calls"));

    let missing = adapter_with(&tmp.path().join("no-such-cli"), &tmp.path().join("work"));
    let err = missing
        .complete(&request(json!([{"role": "user", "content": "q"}]), 1, None))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("could not start"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_slow_cli_times_out() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("slow");
    std::fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut adapter = adapter_with(&script, &tmp.path().join("work"));
    adapter.timeout = Duration::from_millis(200);
    let err = adapter
        .complete(&request(json!([{"role": "user", "content": "q"}]), 1, None))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("did not answer"), "{err}");
}

#[cfg(unix)]
#[tokio::test]
async fn stream_replays_the_answer_with_usage() {
    let tmp = tempfile::tempdir().unwrap();
    let adapter = adapter_with(&fake_claude(tmp.path()), &tmp.path().join("work"));
    let mut stream = adapter
        .stream(&request(json!([{"role": "user", "content": "q"}]), 1, None))
        .await
        .unwrap();
    use futures::StreamExt;
    let mut sse = String::new();
    while let Some(chunk) = stream.next().await {
        sse.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
    }
    assert!(sse.contains("\"content\":\"echo:q \""));
    // The whole prompt (5 uncached + 1 cached), with the cached share named.
    assert!(sse.contains("\"prompt_tokens\":6") && sse.contains("\"cached_tokens\":1"), "{sse}");
    assert!(sse.trim_end().ends_with("data: [DONE]"));
    assert_eq!(adapter.effective_settings(&request(json!([]), 1, None)).temperature, None);
}
