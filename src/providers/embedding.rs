use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
    /// Requested embedding width.
    ///
    /// This exists because fallback across embedding providers is otherwise
    /// unsafe: `nomic-embed-text` emits 768 dimensions and
    /// `text-embedding-3-small` emits 1536, so failing over between them
    /// silently produces vectors of a different shape than every vector already
    /// in the caller's store. Callers that pin a width (the pilot application
    /// sets EMBEDDING_DIMENSIONS=768 and refuses to truncate or pad) send it here,
    /// OpenAI-family models are asked to honour it, and the result is VERIFIED
    /// against it before being returned — see `verify_dimensions`.
    pub dimensions: Option<u32>,
}

impl EmbeddingResult {
    /// Reject a result whose width is not what the caller asked for.
    ///
    /// A wrong-width vector is worse than a failed call: the call surfaces
    /// immediately, whereas the vector is accepted, stored, and quietly corrupts
    /// every similarity comparison made against it afterwards.
    pub fn verify_dimensions(&self, requested: Option<u32>) -> anyhow::Result<()> {
        let Some(want) = requested else {
            return Ok(());
        };
        if let Some(got) = self.embeddings.first().map(|e| e.len()) {
            if got != want as usize {
                anyhow::bail!(
                    "embedding width mismatch: requested {} dimensions, provider returned {}. \
                     Refusing to return vectors that do not match the caller's store.",
                    want,
                    got
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct EmbeddingResult {
    pub embeddings: Vec<Vec<f32>>,
    pub prompt_tokens: u32,
}

#[async_trait]
pub trait EmbeddingAdapter: Send + Sync {
    async fn embed(&self, req: &EmbeddingRequest) -> anyhow::Result<EmbeddingResult>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_with_width(width: usize) -> EmbeddingResult {
        EmbeddingResult {
            embeddings: vec![vec![0.0; width]],
            prompt_tokens: 1,
        }
    }

    /// The live shape of the risk: the pilot application pins 768 (nomic-embed-text) and refuses
    /// to truncate or pad. If an openai fallback answered with its native 1536,
    /// the vectors would be accepted and silently poison every later similarity
    /// comparison. Failing the call is strictly better than storing the vector.
    #[test]
    fn a_wider_vector_than_requested_is_rejected() {
        let err = result_with_width(1536)
            .verify_dimensions(Some(768))
            .expect_err("1536 must not satisfy a 768-dimension request");
        let msg = err.to_string();
        assert!(msg.contains("768"), "error must name what was requested: {msg}");
        assert!(msg.contains("1536"), "error must name what arrived: {msg}");
    }

    #[test]
    fn a_matching_width_is_accepted() {
        result_with_width(768).verify_dimensions(Some(768)).unwrap();
    }

    #[test]
    fn a_caller_that_pins_no_width_accepts_whatever_the_model_emits() {
        result_with_width(1536).verify_dimensions(None).unwrap();
        result_with_width(768).verify_dimensions(None).unwrap();
    }

    #[test]
    fn an_empty_result_does_not_trip_the_check() {
        let empty = EmbeddingResult { embeddings: vec![], prompt_tokens: 0 };
        empty.verify_dimensions(Some(768)).unwrap();
    }
}
