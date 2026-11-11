//! Chat through the vendors' own CLIs (`claude -p`, `codex exec`, `gemini`)
//! instead of their HTTP APIs, so a request runs on the CLI's subscription
//! login and is still metered by the router.
//!
//! ## Sessions
//!
//! Each CLI keeps a server-side conversation ("session") that a later call can
//! resume. A chat-completions caller, though, re-sends the whole transcript on
//! every turn. The adapter bridges the two: after answering, it remembers the
//! session under a hash of the transcript it now holds (the caller's messages
//! plus the reply). When the next request's messages begin with exactly that
//! transcript, the adapter resumes the session and sends only the new
//! messages. Otherwise — a first turn, an edited history, a restart, or the
//! session already claimed by a concurrent request — it starts a new session
//! and replays the transcript as text.
//!
//! Sessions are scoped by user, API-key project, CLI and model, and each
//! user/project gets its own working directory (the Claude CLI files sessions
//! by directory). A remembered session is claimed by the request that resumes
//! it, so two requests never drive one session at once.

mod claude;
mod codex;
mod gemini;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::schema::{CliToolConfig, SubscriptionConfig};
use crate::providers::adapter::{
    CompletionResult, EffectiveSettings, NormalizedRequest, ProviderAdapter, SessionScope,
    SseStream,
};

/// A vendor CLI the router can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    Claude,
    Codex,
    Gemini,
}

impl Tool {
    pub const ALL: [Tool; 3] = [Tool::Claude, Tool::Codex, Tool::Gemini];

    /// The provider name requests through this CLI are routed and metered as.
    pub fn provider_name(self) -> &'static str {
        match self {
            Tool::Claude => "claude-cli",
            Tool::Codex => "codex-cli",
            Tool::Gemini => "gemini-cli",
        }
    }

    pub fn from_provider_name(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.provider_name() == name)
    }

    /// The API provider whose traffic this CLI can take over.
    pub fn upstream(self) -> &'static str {
        match self {
            Tool::Claude => "anthropic",
            Tool::Codex => "openai",
            Tool::Gemini => "gemini",
        }
    }

    fn config(self, cfg: &SubscriptionConfig) -> &CliToolConfig {
        match self {
            Tool::Claude => &cfg.claude,
            Tool::Codex => &cfg.codex,
            Tool::Gemini => &cfg.gemini,
        }
    }

    /// API-key variables this CLI would prefer over its subscription login.
    /// They are removed from the child's environment so the call is billed to
    /// the subscription, which is the point of the mode.
    fn api_key_vars(self) -> &'static [&'static str] {
        match self {
            Tool::Claude => &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"],
            Tool::Codex => &["OPENAI_API_KEY", "CODEX_API_KEY"],
            Tool::Gemini => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        }
    }
}

/// The provider `upstream` is served by under `[subscription]`, or `None`
/// when the mode is off or does not cover it.
pub fn subscription_provider(cfg: &SubscriptionConfig, upstream: &str) -> Option<&'static str> {
    if !cfg.enabled || !cfg.providers.iter().any(|p| p == upstream) {
        return None;
    }
    Tool::ALL
        .into_iter()
        .find(|t| t.upstream() == upstream)
        .map(Tool::provider_name)
}

// ── Transcript ────────────────────────────────────────────────────────────────

/// One chat message reduced to what a CLI session sees: role and text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Turn {
    pub role: String,
    pub text: String,
}

/// The caller's messages split into the system prompt and the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub system: Option<String>,
    pub turns: Vec<Turn>,
}

impl Transcript {
    /// Read OpenAI-shaped chat messages. Content may be a string or an array
    /// of text parts; anything else (images, tool calls) is refused, since a
    /// CLI session cannot be handed it faithfully.
    pub fn from_chat_messages(messages: &[Value]) -> anyhow::Result<Self> {
        let mut system = Vec::new();
        let mut turns = Vec::new();
        for m in messages {
            let role = m["role"].as_str().unwrap_or_default();
            if m.get("tool_calls").is_some_and(|c| !c.is_null()) || role == "tool" {
                anyhow::bail!("subscription CLI sessions do not support tool calls");
            }
            let text = content_text(&m["content"])?;
            match role {
                "system" | "developer" => system.push(text),
                "user" | "assistant" => turns.push(Turn { role: role.to_string(), text }),
                other => anyhow::bail!("unsupported message role {other:?}"),
            }
        }
        if turns.last().map(|t| t.role.as_str()) != Some("user") {
            anyhow::bail!("the last message must come from the user");
        }
        let system = (!system.is_empty()).then(|| system.join("\n\n"));
        Ok(Transcript { system, turns })
    }

    /// Fingerprint of the system prompt plus `turns[..len]`.
    fn prefix_hash(&self, len: usize) -> String {
        let mut h = Sha256::new();
        h.update(serde_json::to_vec(&(&self.system, &self.turns[..len])).unwrap_or_default());
        hex::encode(h.finalize())
    }

    /// Length of the prefix ending at the last assistant turn — the
    /// transcript a remembered session would hold — or `None` on a first turn.
    fn resumable_prefix(&self) -> Option<usize> {
        self.turns.iter().rposition(|t| t.role == "assistant").map(|i| i + 1)
    }
}

/// The text of a message `content`: a string, or the concatenated text parts.
pub fn content_text(content: &Value) -> anyhow::Result<String> {
    match content {
        Value::String(s) => Ok(s.clone()),
        Value::Null => Ok(String::new()),
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match part["type"].as_str() {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        out.push(part["text"].as_str().unwrap_or_default().to_string())
                    }
                    other => anyhow::bail!(
                        "subscription CLI sessions accept text only, got a {} part",
                        other.unwrap_or("untyped")
                    ),
                }
            }
            Ok(out.join("\n"))
        }
        _ => anyhow::bail!("unsupported message content"),
    }
}

/// Render turns as the text a fresh session is given. A single user turn is
/// sent as-is; a longer history is replayed with role labels so the model
/// sees the conversation it is continuing.
fn render(turns: &[Turn]) -> String {
    if let [only] = turns {
        return only.text.clone();
    }
    let mut out = String::from(
        "Continue this conversation. Earlier turns are given for context; reply \
         only to the final user message.\n\n",
    );
    for t in turns {
        let label = if t.role == "assistant" { "Assistant" } else { "User" };
        out.push_str(&format!("{label}: {}\n\n", t.text));
    }
    out.trim_end().to_string()
}

// ── Session store ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SessionKey {
    scope: SessionScope,
    tool: Tool,
    model: String,
    transcript: String,
}

/// Remembered CLI sessions, keyed by the transcript each one holds.
#[derive(Default)]
pub struct SessionStore {
    entries: Mutex<HashMap<SessionKey, (String, Instant)>>,
}

impl SessionStore {
    /// Take the session holding `key`'s transcript, if one is remembered and
    /// was used within `idle`. Taking it means a concurrent request with the
    /// same history starts its own session instead of sharing this one.
    fn claim(&self, key: &SessionKey, idle: Duration) -> Option<String> {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.retain(|_, (_, used)| used.elapsed() < idle);
        entries.remove(key).map(|(id, _)| id)
    }

    fn remember(&self, key: SessionKey, session_id: String) {
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        entries.insert(key, (session_id, Instant::now()));
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ── One CLI call ──────────────────────────────────────────────────────────────

/// What to send a CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub model: String,
    /// The session to resume, or `None` to start one.
    pub resume: Option<String>,
    /// The caller's system prompt.
    pub system: Option<String>,
    pub prompt: String,
}

/// A CLI's answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub session_id: Option<String>,
    pub text: String,
    /// Input tokens not served from the CLI's prompt cache.
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cache_read_tokens: u32,
    pub cache_write_tokens: u32,
    pub reasoning_tokens: Option<u32>,
}

/// An error the CLI itself reported (a refused model, a login problem), as
/// opposed to one in running or reading it.
#[derive(Debug)]
pub struct VendorError(pub String);

impl std::fmt::Display for VendorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VendorError {}

fn vendor_error(message: String) -> anyhow::Error {
    anyhow::Error::new(VendorError(message))
}

/// What a CLI run produced.
struct Output {
    success: bool,
    status: String,
    stdout: String,
    stderr: String,
}

/// Run `program` with `args` in `dir`, feeding `stdin`. The vendor's API-key
/// variables are dropped from the environment.
async fn run(
    tool: Tool,
    program: &str,
    args: &[String],
    dir: &Path,
    stdin: &str,
    timeout: Duration,
) -> anyhow::Result<Output> {
    use tokio::io::AsyncWriteExt;

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for var in tool.api_key_vars() {
        cmd.env_remove(var);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start `{program}`: {e}"))?;
    if let Some(mut input) = child.stdin.take() {
        input.write_all(stdin.as_bytes()).await?;
    }
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("`{program}` did not answer within {}s", timeout.as_secs()))??;
    Ok(Output {
        success: out.status.success(),
        status: out.status.to_string(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

// ── Adapter ───────────────────────────────────────────────────────────────────

/// A [`ProviderAdapter`] that answers through one vendor CLI.
pub struct CliSessionAdapter {
    tool: Tool,
    program: String,
    base_args: Vec<String>,
    work_dir: PathBuf,
    idle: Duration,
    timeout: Duration,
    sessions: Arc<SessionStore>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl CliSessionAdapter {
    pub fn new(tool: Tool, cfg: &SubscriptionConfig) -> Self {
        let tool_cfg = tool.config(cfg);
        Self {
            tool,
            program: tool_cfg.command.clone(),
            base_args: tool_cfg.args.clone(),
            work_dir: PathBuf::from(shellexpand_home(&cfg.work_dir)),
            idle: Duration::from_secs(cfg.session_idle_secs),
            timeout: Duration::from_secs(cfg.timeout_secs),
            sessions: Arc::new(SessionStore::default()),
            permits: Arc::new(tokio::sync::Semaphore::new(cfg.max_concurrent.max(1))),
        }
    }

    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    /// The working directory for `scope`'s sessions with this CLI, created
    /// owner-only on first use.
    fn session_dir(&self, scope: &SessionScope) -> anyhow::Result<PathBuf> {
        let project = scope.project.as_deref().map(sanitize).unwrap_or_else(|| "_".into());
        let dir = self
            .work_dir
            .join(self.tool.provider_name())
            .join(format!("user-{}", scope.user_id))
            .join(project);
        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(dir)
    }

    fn args(&self, inv: &Invocation) -> Vec<String> {
        let mut args = self.base_args.clone();
        args.extend(match self.tool {
            Tool::Claude => claude::args(inv),
            Tool::Codex => codex::args(inv),
            Tool::Gemini => gemini::args(inv),
        });
        args
    }

    fn stdin(&self, inv: &Invocation) -> String {
        match self.tool {
            Tool::Claude => claude::stdin(inv),
            Tool::Codex => codex::stdin(inv),
            Tool::Gemini => gemini::stdin(inv),
        }
    }

    fn parse(&self, stdout: &str) -> anyhow::Result<Reply> {
        match self.tool {
            Tool::Claude => claude::parse(stdout),
            Tool::Codex => codex::parse(stdout),
            Tool::Gemini => gemini::parse(stdout),
        }
    }

    /// Answer `req`, resuming the caller's session when its history matches
    /// one this adapter is holding.
    async fn converse(&self, req: &NormalizedRequest) -> anyhow::Result<Reply> {
        // The routes gate tools on `supports_tools`; refuse here as well so
        // no path can drop a caller's tools silently.
        if req.tools.as_ref().is_some_and(|t| !t.is_empty()) {
            anyhow::bail!("subscription CLI sessions do not support tool calls");
        }
        let transcript = Transcript::from_chat_messages(&req.messages)?;
        let scope = req.session_scope.clone().unwrap_or_default();
        let key = |transcript_hash: String| SessionKey {
            scope: scope.clone(),
            tool: self.tool,
            model: req.model.clone(),
            transcript: transcript_hash,
        };

        let resumed = transcript.resumable_prefix().and_then(|len| {
            self.sessions
                .claim(&key(transcript.prefix_hash(len)), self.idle)
                .map(|id| (id, len))
        });
        let inv = match &resumed {
            Some((id, len)) => Invocation {
                model: req.model.clone(),
                resume: Some(id.clone()),
                system: transcript.system.clone(),
                prompt: render_new(&transcript.turns[*len..]),
            },
            None => Invocation {
                model: req.model.clone(),
                resume: None,
                system: transcript.system.clone(),
                prompt: render(&transcript.turns),
            },
        };
        tracing::debug!(
            provider = self.tool.provider_name(),
            model = req.model.as_str(),
            resumed = inv.resume.is_some(),
            "subscription CLI call"
        );

        let dir = self.session_dir(&scope)?;
        let _permit = self.permits.acquire().await?;
        let out = run(self.tool, &self.program, &self.args(&inv), &dir, &self.stdin(&inv), self.timeout).await?;
        let reply = match self.parse(&out.stdout) {
            Ok(reply) if out.success => reply,
            // The CLI said what went wrong: that is the useful message.
            Err(e) if e.is::<VendorError>() => return Err(e),
            Err(e) if out.success => return Err(e),
            _ => {
                let detail = if out.stderr.trim().is_empty() { &out.stdout } else { &out.stderr };
                anyhow::bail!(
                    "`{}` exited with {}: {}",
                    self.program,
                    out.status,
                    truncate(detail.trim(), 2000)
                );
            }
        };

        // Remember the session under the transcript it now holds, so the
        // caller's next turn resumes it.
        if let Some(id) = reply.session_id.clone().or_else(|| inv.resume.clone()) {
            let mut held = transcript.clone();
            held.turns.push(Turn { role: "assistant".into(), text: reply.text.clone() });
            let len = held.turns.len();
            self.sessions.remember(key(held.prefix_hash(len)), id);
        }
        Ok(reply)
    }
}

/// Render the turns sent to a resumed session: normally one user message;
/// several consecutive ones are joined.
fn render_new(turns: &[Turn]) -> String {
    turns.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n\n")
}

fn sanitize(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    // Distinct projects that sanitize alike still get distinct directories.
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{clean}-{}", &hex::encode(h.finalize())[..8])
}

fn shellexpand_home(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest).display().to_string(),
        None => path.to_string(),
    }
}

impl Reply {
    fn into_completion(self) -> CompletionResult {
        CompletionResult {
            content: self.text,
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            finish_reason: "stop".into(),
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            reasoning_tokens: self.reasoning_tokens,
            ttft_ms: None,
            tool_calls: None,
        }
    }
}

#[async_trait::async_trait]
impl ProviderAdapter for CliSessionAdapter {
    async fn complete(&self, req: &NormalizedRequest) -> anyhow::Result<CompletionResult> {
        Ok(self.converse(req).await?.into_completion())
    }

    /// The CLIs answer in one piece here, so the stream is the finished
    /// answer replayed as chat chunks, with usage on the last one.
    async fn stream(&self, req: &NormalizedRequest) -> anyhow::Result<SseStream> {
        let result = self.complete(req).await?;
        let id = format!("chatcmpl-cli-{}", uuid::Uuid::new_v4());
        let created = chrono::Utc::now().timestamp();
        let mut sse: String =
            crate::router::cache::stream::chat_replay_chunks(&result, &id, &req.model, created)
                .iter()
                .map(|chunk| format!("data: {chunk}\n\n"))
                .collect();
        sse.push_str(&format!("data: {}\n\n", usage_chunk(&result, &id, &req.model, created)));
        sse.push_str("data: [DONE]\n\n");
        Ok(Box::pin(futures::stream::once(async move {
            Ok(bytes::Bytes::from(sse))
        })))
    }

    /// The CLIs take no sampling or reasoning parameters, so none are sent.
    fn effective_settings(&self, _req: &NormalizedRequest) -> EffectiveSettings {
        EffectiveSettings {
            temperature: None,
            max_tokens: None,
            timeout_secs: Some(self.timeout.as_secs()),
            reasoning: None,
        }
    }
}

/// The OpenAI usage chunk for a replayed stream. `prompt_tokens` is the whole
/// prompt, cache reads and writes included, with the reads also reported as
/// `cached_tokens` — the shape the stream metering reads.
fn usage_chunk(result: &CompletionResult, id: &str, model: &str, created: i64) -> Value {
    serde_json::json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [],
        "usage": {
            "prompt_tokens": result.prompt_tokens + result.cache_read_tokens + result.cache_write_tokens,
            "completion_tokens": result.completion_tokens,
            "total_tokens": result.prompt_tokens + result.cache_read_tokens
                + result.cache_write_tokens + result.completion_tokens,
            "prompt_tokens_details": {"cached_tokens": result.cache_read_tokens},
        },
    })
}

#[cfg(test)]
mod tests;
