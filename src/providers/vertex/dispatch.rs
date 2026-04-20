#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publisher {
    Google,
    Anthropic,
}

/// Parse a model identifier into `(publisher, bare_model_id)`.
///
/// Accepts either a publisher-prefixed id (`google/gemini-2.5-pro`,
/// `anthropic/claude-sonnet-4-6@20250514`) or a bare id whose name prefix
/// disambiguates the publisher (`gemini-*` → Google, `claude-*` → Anthropic).
pub fn parse_model_id(model: &str) -> anyhow::Result<(Publisher, String)> {
    if let Some((prefix, rest)) = model.split_once('/') {
        let publisher = match prefix {
            "google" => Publisher::Google,
            "anthropic" => Publisher::Anthropic,
            other => anyhow::bail!("Unsupported Vertex publisher '{}'", other),
        };
        return Ok((publisher, rest.to_string()));
    }
    if model.starts_with("gemini-") {
        return Ok((Publisher::Google, model.to_string()));
    }
    if model.starts_with("claude-") {
        return Ok((Publisher::Anthropic, model.to_string()));
    }
    anyhow::bail!("Unsupported Vertex publisher (cannot infer from model id '{}')", model)
}
