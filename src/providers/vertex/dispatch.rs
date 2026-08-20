#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publisher {
    Google,
    Anthropic,
    /// Any other Model Garden publisher (mistralai, meta, deepseek-ai, qwen,
    /// ai21, openai, …) — served through Vertex's OpenAI-compatible MaaS
    /// endpoint rather than a per-publisher method. The model id keeps its
    /// full `publisher/model` form on this arm because that is how the MaaS
    /// endpoint addresses publishers.
    Maas,
}

/// Parse a model identifier into `(publisher, model_id)`.
///
/// Accepts either a publisher-prefixed id (`google/gemini-2.5-pro`,
/// `anthropic/claude-sonnet-4-6@20250514`, `mistralai/mistral-medium-3`) or a
/// bare id whose name prefix disambiguates the publisher (`gemini-*` → Google,
/// `claude-*` → Anthropic).
///
/// Google and Anthropic return the BARE model id (their endpoints carry the
/// publisher in the URL path); every other publisher returns the FULL
/// `publisher/model` id (the MaaS endpoint carries it in the request body).
pub fn parse_model_id(model: &str) -> anyhow::Result<(Publisher, String)> {
    if let Some((prefix, rest)) = model.split_once('/') {
        return Ok(match prefix {
            "google" => (Publisher::Google, rest.to_string()),
            "anthropic" => (Publisher::Anthropic, rest.to_string()),
            _ => (Publisher::Maas, model.to_string()),
        });
    }
    if model.starts_with("gemini-") {
        return Ok((Publisher::Google, model.to_string()));
    }
    if model.starts_with("claude-") {
        return Ok((Publisher::Anthropic, model.to_string()));
    }
    anyhow::bail!("Unsupported Vertex publisher (cannot infer from model id '{}')", model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_publishers_keep_bare_ids() {
        assert_eq!(
            parse_model_id("google/gemini-2.5-pro").unwrap(),
            (Publisher::Google, "gemini-2.5-pro".to_string())
        );
        assert_eq!(
            parse_model_id("anthropic/claude-opus-4-5@20251101").unwrap(),
            (Publisher::Anthropic, "claude-opus-4-5@20251101".to_string())
        );
    }

    #[test]
    fn other_publishers_route_to_maas_with_full_id() {
        assert_eq!(
            parse_model_id("mistralai/mistral-medium-3").unwrap(),
            (Publisher::Maas, "mistralai/mistral-medium-3".to_string())
        );
        assert_eq!(
            parse_model_id("meta/llama-4-maverick-17b-128e-instruct-maas").unwrap(),
            (Publisher::Maas, "meta/llama-4-maverick-17b-128e-instruct-maas".to_string())
        );
    }

    #[test]
    fn bare_ids_still_infer() {
        assert_eq!(
            parse_model_id("gemini-2.5-flash").unwrap(),
            (Publisher::Google, "gemini-2.5-flash".to_string())
        );
        assert!(parse_model_id("mistral-medium-3").is_err());
    }
}
