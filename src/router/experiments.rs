//! Controlled experiments (spec §7a): the in-memory registry a request is
//! bound against.
//!
//! The registry is a snapshot of the `experiments` table held behind an
//! [`ArcSwap`], so [`ExperimentRegistry::bind`] does no I/O: it parses the
//! `x-modelrouter-experiment` header, runs the eligibility checks, picks a
//! variant and hands back that variant's pinned overlay. The snapshot is
//! rebuilt from the database after every admin write and by the lifecycle
//! tick in `cli::serve`, which also closes experiments whose `expires_at`
//! has passed. Expiry is checked per request as well, so the first request
//! after the boundary is refused even before the tick has run.
//!
//! Variant assignment for an id-only header is a hash of
//! `"{experiment_id}:{session_id}"` (FNV-1a 64, implemented below rather than
//! pulled in as a crate) over the variant labels sorted bytewise, so a
//! session sees the same variant on every request for the life of the
//! experiment and the split is decided without any shared counter.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arc_swap::ArcSwap;
use axum::http::HeaderMap;

use crate::db::models::{Experiment, ExperimentStatus};
use crate::db::repositories::experiments::{ExperimentRepository, ExperimentStatusFilter};

/// Header selecting an experiment: `<id>` or `<id>:<label>`.
pub const EXPERIMENT_HEADER: &str = "x-modelrouter-experiment";

/// Longest header value accepted, in bytes. Long enough for any id and a
/// 64-character label with room to spare; anything longer is refused
/// without being echoed.
pub const MAX_HEADER_LEN: usize = 128;

/// Longest variant label accepted.
pub const MAX_LABEL_LEN: usize = 64;

/// Everything `bind` needs to know about one experiment, derived from an
/// [`Experiment`] row once at load time.
#[derive(Clone, Debug, PartialEq)]
pub struct ExperimentSnapshot {
    pub status: ExperimentStatus,
    /// Unix seconds; 0 means never.
    pub expires_at: i64,
    /// Empty means every user may bind.
    pub allowed_user_ids: Vec<i64>,
    /// Variant labels sorted bytewise; the hash picks an index into this.
    pub labels: Vec<String>,
    /// Per variant: requested model name -> pinned `provider/model`.
    pub overlays: BTreeMap<String, Arc<BTreeMap<String, String>>>,
    pub retain_content: bool,
}

impl From<&Experiment> for ExperimentSnapshot {
    fn from(exp: &Experiment) -> Self {
        let overlays: BTreeMap<String, Arc<BTreeMap<String, String>>> = exp
            .variants
            .iter()
            .map(|(label, targets)| {
                let overlay = targets
                    .iter()
                    .map(|(name, t)| (name.clone(), format!("{}/{}", t.provider, t.model)))
                    .collect();
                (label.clone(), Arc::new(overlay))
            })
            .collect();
        // BTreeMap iterates in bytewise key order already.
        let labels = overlays.keys().cloned().collect();
        ExperimentSnapshot {
            status: exp.status,
            expires_at: exp.expires_at,
            allowed_user_ids: exp.allowed_user_ids.clone(),
            labels,
            overlays,
            retain_content: exp.retain_content,
        }
    }
}

/// The variant a request was bound to.
#[derive(Clone, Debug, PartialEq)]
pub struct Binding {
    pub experiment_id: i64,
    pub variant: String,
    /// Requested model name -> pinned `provider/model`. Names not in the map
    /// route as usual.
    pub overlay: Arc<BTreeMap<String, String>>,
    pub retain_content: bool,
}

/// Why a request could not be bound. Every message names the header or body
/// field at fault; header text is echoed only after it has passed the
/// charset check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindError {
    /// The header appeared more than once.
    DuplicateHeader,
    /// The header is longer than [`MAX_HEADER_LEN`] bytes.
    HeaderTooLong,
    /// The header is not `<id>` or `<id>:<label>` in the allowed charset.
    Malformed,
    /// The id part is not a positive integer.
    InvalidId(String),
    /// The label part is longer than [`MAX_LABEL_LEN`].
    LabelTooLong,
    /// No experiment with this id.
    UnknownExperiment(i64),
    Closed(i64),
    Expired(i64),
    UserNotAllowed { experiment_id: i64, user_id: i64 },
    MissingCorrelationId,
    UnknownVariant { experiment_id: i64, label: String },
    /// Id-only header but the body carries no `session_id`.
    MissingSessionId,
    /// `session_id` is present but not a JSON string.
    SessionIdNotString,
    /// The experiment has no variants; cannot happen for a row the admin API
    /// created, but a snapshot built by hand can be empty.
    NoVariants(i64),
    /// The header is present on an endpoint that does not run experiments.
    NotSupportedHere,
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::DuplicateHeader => {
                write!(f, "{EXPERIMENT_HEADER} must appear at most once")
            }
            BindError::HeaderTooLong => {
                write!(f, "{EXPERIMENT_HEADER} must be at most {MAX_HEADER_LEN} bytes")
            }
            BindError::Malformed => write!(
                f,
                "{EXPERIMENT_HEADER} must be `<id>` or `<id>:<label>` \
                 (label: [A-Za-z0-9_.-], at most {MAX_LABEL_LEN} characters)"
            ),
            BindError::InvalidId(raw) => {
                write!(f, "{EXPERIMENT_HEADER}: '{raw}' is not a positive experiment id")
            }
            BindError::LabelTooLong => {
                write!(f, "{EXPERIMENT_HEADER}: label must be at most {MAX_LABEL_LEN} characters")
            }
            BindError::UnknownExperiment(id) => {
                write!(f, "{EXPERIMENT_HEADER}: experiment {id} not found")
            }
            BindError::Closed(id) => write!(f, "{EXPERIMENT_HEADER}: experiment {id} is closed"),
            BindError::Expired(id) => {
                write!(f, "{EXPERIMENT_HEADER}: experiment {id} has expired")
            }
            BindError::UserNotAllowed { experiment_id, user_id } => write!(
                f,
                "{EXPERIMENT_HEADER}: user {user_id} is not allowed in experiment {experiment_id}"
            ),
            BindError::MissingCorrelationId => write!(
                f,
                "attribution.correlation_id is required when {EXPERIMENT_HEADER} is set"
            ),
            BindError::UnknownVariant { experiment_id, label } => write!(
                f,
                "{EXPERIMENT_HEADER}: experiment {experiment_id} has no variant '{label}'"
            ),
            BindError::MissingSessionId => write!(
                f,
                "session_id is required when {EXPERIMENT_HEADER} names no variant"
            ),
            BindError::SessionIdNotString => write!(f, "session_id must be a string"),
            BindError::NoVariants(id) => {
                write!(f, "{EXPERIMENT_HEADER}: experiment {id} has no variants")
            }
            BindError::NotSupportedHere => {
                write!(f, "{EXPERIMENT_HEADER} is not supported on this endpoint")
            }
        }
    }
}

impl std::error::Error for BindError {}

/// Refuse the experiment header outright. For endpoints that do not run
/// experiments, so a caller who sets it by mistake finds out rather than
/// getting unmarked traffic.
pub fn reject_header(headers: &HeaderMap) -> Result<(), BindError> {
    if headers.contains_key(EXPERIMENT_HEADER) {
        Err(BindError::NotSupportedHere)
    } else {
        Ok(())
    }
}

/// Live view of the `experiments` table. Cheap to clone the inner map on
/// every read; writers replace the whole thing.
#[derive(Default)]
pub struct ExperimentRegistry {
    snapshots: ArcSwap<HashMap<i64, ExperimentSnapshot>>,
}

impl ExperimentRegistry {
    /// Build the snapshot map from experiment rows. Every row is kept,
    /// closed ones included, so a closed experiment is refused as closed
    /// rather than as unknown.
    pub fn snapshot_from<'a>(
        rows: impl IntoIterator<Item = &'a Experiment>,
    ) -> HashMap<i64, ExperimentSnapshot> {
        rows.into_iter()
            .map(|exp| (exp.id, ExperimentSnapshot::from(exp)))
            .collect()
    }

    /// Replace the live map. Admin writes call this after `load_from`; tests
    /// build a map by hand.
    pub fn store(&self, snapshots: HashMap<i64, ExperimentSnapshot>) {
        self.snapshots.store(Arc::new(snapshots));
    }

    /// Rebuild the live map from the database. On error the previous map
    /// stays in place.
    pub async fn load_from(&self, repo: &dyn ExperimentRepository) -> anyhow::Result<()> {
        let rows = repo.list(ExperimentStatusFilter::All).await?;
        self.store(Self::snapshot_from(&rows));
        Ok(())
    }

    /// Number of experiments in the live map, closed ones included.
    pub fn len(&self) -> usize {
        self.snapshots.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bind a request to an experiment variant. `Ok(None)` when the header
    /// is absent. Checks run in the order listed in the module docs: header
    /// grammar, existence, status, expiry, allow list, correlation id, then
    /// variant selection. No I/O; cost is linear in the header length and
    /// the number of variants.
    pub fn bind(
        &self,
        headers: &HeaderMap,
        body: &serde_json::Value,
        correlation_id: Option<&str>,
        user_id: i64,
        now_epoch: i64,
    ) -> Result<Option<Binding>, BindError> {
        let Some((experiment_id, label)) = parse_header(headers)? else {
            return Ok(None);
        };

        let snapshots = self.snapshots.load();
        let exp = snapshots
            .get(&experiment_id)
            .ok_or(BindError::UnknownExperiment(experiment_id))?;
        if exp.status != ExperimentStatus::Active {
            return Err(BindError::Closed(experiment_id));
        }
        if exp.expires_at != 0 && now_epoch >= exp.expires_at {
            return Err(BindError::Expired(experiment_id));
        }
        if !exp.allowed_user_ids.is_empty() && !exp.allowed_user_ids.contains(&user_id) {
            return Err(BindError::UserNotAllowed { experiment_id, user_id });
        }
        if correlation_id.is_none_or(|c| c.trim().is_empty()) {
            return Err(BindError::MissingCorrelationId);
        }
        if exp.labels.is_empty() {
            return Err(BindError::NoVariants(experiment_id));
        }

        let variant = match label {
            Some(label) => {
                if !exp.overlays.contains_key(label) {
                    return Err(BindError::UnknownVariant {
                        experiment_id,
                        label: label.to_string(),
                    });
                }
                label.to_string()
            }
            None => {
                let session_id = match body.get("session_id") {
                    None | Some(serde_json::Value::Null) => {
                        return Err(BindError::MissingSessionId)
                    }
                    Some(serde_json::Value::String(s)) => s.as_str(),
                    Some(_) => return Err(BindError::SessionIdNotString),
                };
                let key = format!("{experiment_id}:{session_id}");
                let idx = (fnv1a_64(key.as_bytes()) % exp.labels.len() as u64) as usize;
                exp.labels[idx].clone()
            }
        };

        let overlay = exp.overlays[&variant].clone();
        Ok(Some(Binding {
            experiment_id,
            variant,
            overlay,
            retain_content: exp.retain_content,
        }))
    }
}

/// Parse the experiment header into `(id, label)`. `Ok(None)` when absent.
///
/// Grammar: `<id>` or `<id>:<label>`, split on the first `:`; the id is a
/// positive integer, the label matches `[A-Za-z0-9_.-]{1,64}`. The whole
/// value is charset-checked before any of it is echoed into an error.
fn parse_header(headers: &HeaderMap) -> Result<Option<(i64, Option<&str>)>, BindError> {
    let mut values = headers.get_all(EXPERIMENT_HEADER).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(BindError::DuplicateHeader);
    }
    if value.len() > MAX_HEADER_LEN {
        return Err(BindError::HeaderTooLong);
    }
    let raw = value.to_str().map_err(|_| BindError::Malformed)?.trim();
    if raw.is_empty() || !raw.bytes().all(|b| is_label_byte(b) || b == b':') {
        return Err(BindError::Malformed);
    }

    let (id_part, label) = match raw.split_once(':') {
        Some((id, label)) => (id, Some(label)),
        None => (raw, None),
    };
    let id = match id_part.parse::<i64>() {
        Ok(id) if id > 0 => id,
        _ => return Err(BindError::InvalidId(id_part.to_string())),
    };
    if let Some(label) = label {
        if label.is_empty() || label.contains(':') {
            return Err(BindError::Malformed);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(BindError::LabelTooLong);
        }
    }
    Ok(Some((id, label)))
}

fn is_label_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')
}

/// Whether `label` is a legal variant label: `[A-Za-z0-9_.-]{1,64}`. The
/// admin API checks labels at creation with the same rule the header parser
/// applies, so a stored label is always bindable.
pub fn is_valid_label(label: &str) -> bool {
    !label.is_empty() && label.len() <= MAX_LABEL_LEN && label.bytes().all(is_label_byte)
}

/// FNV-1a, 64-bit. Stable across platforms and versions, which matters
/// because a session's variant must not change over the experiment's life.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET, |h, &b| (h ^ u64::from(b)).wrapping_mul(PRIME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::{ExperimentVariants, NewExperiment, VariantTarget};
    use crate::db::sqlite::SqliteDb;
    use axum::http::HeaderValue;
    use serde_json::json;

    fn target(provider: &str, model: &str) -> VariantTarget {
        VariantTarget {
            target: format!("{provider}/{model}"),
            provider: provider.into(),
            model: model.into(),
        }
    }

    fn variants() -> ExperimentVariants {
        let mut control = BTreeMap::new();
        control.insert("fast".to_string(), target("openai", "gpt-5-mini"));
        let mut candidate = BTreeMap::new();
        candidate.insert("fast".to_string(), target("anthropic", "claude-haiku"));
        candidate.insert("deep".to_string(), target("anthropic", "claude-opus"));
        let mut v = BTreeMap::new();
        v.insert("control".to_string(), control);
        v.insert("candidate".to_string(), candidate);
        v
    }

    fn experiment(id: i64, allowed: Vec<i64>, expires_at: i64) -> Experiment {
        Experiment {
            id,
            name: format!("exp-{id}"),
            variants: variants(),
            allowed_user_ids: allowed,
            status: ExperimentStatus::Active,
            feed_learning: false,
            expires_at,
            created_at: "2026-09-01T00:00:00+00:00".into(),
            closed_at: None,
            retain_content: true,
            content_retention_days: 0,
        }
    }

    fn registry(rows: &[Experiment]) -> ExperimentRegistry {
        let reg = ExperimentRegistry::default();
        reg.store(ExperimentRegistry::snapshot_from(rows));
        reg
    }

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(EXPERIMENT_HEADER, HeaderValue::from_str(value).unwrap());
        h
    }

    const NOW: i64 = 1_000_000;

    fn bind(
        reg: &ExperimentRegistry,
        header: &str,
        body: serde_json::Value,
        user_id: i64,
    ) -> Result<Option<Binding>, BindError> {
        reg.bind(&headers(header), &body, Some("run-1"), user_id, NOW)
    }

    #[test]
    fn absent_header_binds_nothing() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let out = reg.bind(&HeaderMap::new(), &json!({}), None, 1, NOW).unwrap();
        assert_eq!(out, None);
    }

    #[test]
    fn explicit_variant_returns_its_overlay() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let b = bind(&reg, "1:candidate", json!({}), 1).unwrap().unwrap();
        assert_eq!(b.experiment_id, 1);
        assert_eq!(b.variant, "candidate");
        assert_eq!(b.overlay["fast"], "anthropic/claude-haiku");
        assert_eq!(b.overlay["deep"], "anthropic/claude-opus");
        assert!(b.retain_content);

        let b = bind(&reg, "1:control", json!({}), 1).unwrap().unwrap();
        assert_eq!(b.variant, "control");
        assert_eq!(b.overlay.len(), 1);
        assert_eq!(b.overlay["fast"], "openai/gpt-5-mini");
    }

    #[test]
    fn id_only_assignment_is_stable_per_session_and_covers_both_variants() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let first = bind(&reg, "1", json!({"session_id": "s-1"}), 1).unwrap().unwrap();
        for _ in 0..10 {
            let again = bind(&reg, "1", json!({"session_id": "s-1"}), 1).unwrap().unwrap();
            assert_eq!(again.variant, first.variant);
        }

        let mut seen = std::collections::HashSet::new();
        for i in 0..200 {
            let b = bind(&reg, "1", json!({"session_id": format!("session-{i}")}), 1)
                .unwrap()
                .unwrap();
            seen.insert(b.variant);
        }
        assert_eq!(seen.len(), 2, "both variants should be hit across many sessions");
    }

    #[test]
    fn assignment_matches_inline_fnv() {
        // Pin the hash so a later refactor cannot silently reshuffle sessions.
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        let reg = registry(&[experiment(7, vec![], 0)]);
        let b = bind(&reg, "7", json!({"session_id": "abc"}), 1).unwrap().unwrap();
        let idx = (fnv1a_64(b"7:abc") % 2) as usize;
        assert_eq!(b.variant, ["candidate", "control"][idx]);
    }

    #[test]
    fn expiry_one_second_in_the_past_is_rejected() {
        let reg = registry(&[experiment(1, vec![], NOW - 1)]);
        let err = bind(&reg, "1:control", json!({}), 1).unwrap_err();
        assert_eq!(err, BindError::Expired(1));
        assert!(err.to_string().contains("expired"));

        // Exactly at the boundary is rejected too; one second before is fine.
        let reg = registry(&[experiment(1, vec![], NOW)]);
        assert_eq!(bind(&reg, "1:control", json!({}), 1).unwrap_err(), BindError::Expired(1));
        let reg = registry(&[experiment(1, vec![], NOW + 1)]);
        assert!(bind(&reg, "1:control", json!({}), 1).unwrap().is_some());
    }

    #[test]
    fn closed_is_rejected() {
        let mut exp = experiment(1, vec![], 0);
        exp.status = ExperimentStatus::Closed;
        let reg = registry(&[exp]);
        let err = bind(&reg, "1:control", json!({}), 1).unwrap_err();
        assert_eq!(err, BindError::Closed(1));
        assert!(err.to_string().contains("closed"));
    }

    #[test]
    fn overlong_header_is_rejected_without_echo() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let value = format!("1:{}", "x".repeat(127));
        assert_eq!(value.len(), 129);
        let err = bind(&reg, &value, json!({}), 1).unwrap_err();
        assert_eq!(err, BindError::HeaderTooLong);
        assert!(!err.to_string().contains("xxxx"));

        // 128 bytes passes the length check and fails on the label length,
        // still without echoing it.
        let value = format!("1:{}", "x".repeat(126));
        assert_eq!(value.len(), 128);
        let err = bind(&reg, &value, json!({}), 1).unwrap_err();
        assert_eq!(err, BindError::LabelTooLong);
        assert!(!err.to_string().contains("xxxx"));
    }

    #[test]
    fn duplicate_header_is_rejected() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let mut h = headers("1:control");
        h.append(EXPERIMENT_HEADER, HeaderValue::from_static("1:candidate"));
        let err = reg.bind(&h, &json!({}), Some("run-1"), 1, NOW).unwrap_err();
        assert_eq!(err, BindError::DuplicateHeader);
        assert!(!err.to_string().contains("control"));
    }

    #[test]
    fn bad_charset_is_rejected_without_echo() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let err = bind(&reg, "1:ctrl<script>", json!({}), 1).unwrap_err();
        assert_eq!(err, BindError::Malformed);
        assert!(!err.to_string().contains("script"));
        assert_eq!(bind(&reg, "", json!({}), 1).unwrap_err(), BindError::Malformed);
        assert_eq!(bind(&reg, "1:", json!({}), 1).unwrap_err(), BindError::Malformed);
        assert_eq!(bind(&reg, "1:a:b", json!({}), 1).unwrap_err(), BindError::Malformed);
    }

    #[test]
    fn non_positive_or_non_numeric_id_is_rejected() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        for raw in ["0", "-1", "abc", "1.5"] {
            let err = bind(&reg, raw, json!({}), 1).unwrap_err();
            assert_eq!(err, BindError::InvalidId(raw.to_string()));
            assert!(err.to_string().contains(EXPERIMENT_HEADER));
        }
    }

    #[test]
    fn numeric_session_id_is_rejected() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let err = bind(&reg, "1", json!({"session_id": 42}), 1).unwrap_err();
        assert_eq!(err, BindError::SessionIdNotString);
        assert!(err.to_string().contains("session_id"));

        let err = bind(&reg, "1", json!({}), 1).unwrap_err();
        assert_eq!(err, BindError::MissingSessionId);
        assert!(err.to_string().contains("session_id"));
    }

    #[test]
    fn unknown_id_names_the_header() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let err = bind(&reg, "99:control", json!({}), 1).unwrap_err();
        assert_eq!(err, BindError::UnknownExperiment(99));
        assert!(err.to_string().contains(EXPERIMENT_HEADER));
        assert!(err.to_string().contains("99"));
    }

    #[test]
    fn unknown_label_names_the_header_and_label() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        let err = bind(&reg, "1:nope", json!({}), 1).unwrap_err();
        assert_eq!(
            err,
            BindError::UnknownVariant { experiment_id: 1, label: "nope".into() }
        );
        let msg = err.to_string();
        assert!(msg.contains(EXPERIMENT_HEADER));
        assert!(msg.contains("'nope'"));
    }

    #[test]
    fn missing_correlation_id_names_the_field() {
        let reg = registry(&[experiment(1, vec![], 0)]);
        for cid in [None, Some(""), Some("  ")] {
            let err = reg
                .bind(&headers("1:control"), &json!({}), cid, 1, NOW)
                .unwrap_err();
            assert_eq!(err, BindError::MissingCorrelationId);
            assert!(err.to_string().contains("correlation_id"));
        }
    }

    #[test]
    fn allow_list_gates_users() {
        let reg = registry(&[experiment(1, vec![3, 7], 0)]);
        let err = bind(&reg, "1:control", json!({}), 5).unwrap_err();
        assert_eq!(err, BindError::UserNotAllowed { experiment_id: 1, user_id: 5 });
        assert!(err.to_string().contains("user 5"));

        assert!(bind(&reg, "1:control", json!({}), 3).unwrap().is_some());
        assert!(bind(&reg, "1:control", json!({}), 7).unwrap().is_some());

        let open = registry(&[experiment(2, vec![], 0)]);
        for user in [1, 5, 9_999] {
            assert!(bind(&open, "2:control", json!({}), user).unwrap().is_some());
        }
    }

    #[test]
    fn check_order_reports_status_before_allow_list() {
        // A closed experiment reports closed even for a disallowed user, and
        // an allowed-list miss is reported before a missing correlation id.
        let mut exp = experiment(1, vec![3], 0);
        exp.status = ExperimentStatus::Closed;
        let reg = registry(&[exp]);
        assert_eq!(bind(&reg, "1", json!({}), 5).unwrap_err(), BindError::Closed(1));

        let reg = registry(&[experiment(1, vec![3], 0)]);
        let err = reg.bind(&headers("1"), &json!({}), None, 5, NOW).unwrap_err();
        assert_eq!(err, BindError::UserNotAllowed { experiment_id: 1, user_id: 5 });
    }

    #[test]
    fn reject_header_only_errors_when_present() {
        assert_eq!(reject_header(&HeaderMap::new()), Ok(()));
        assert_eq!(reject_header(&headers("1")), Err(BindError::NotSupportedHere));
        assert!(BindError::NotSupportedHere.to_string().contains(EXPERIMENT_HEADER));
    }

    #[test]
    fn bind_error_maps_to_invalid_request() {
        let api: crate::api::error::ApiError = BindError::UnknownExperiment(4).into();
        match api {
            crate::api::error::ApiError::InvalidRequest(msg) => {
                assert!(msg.contains("experiment 4 not found"))
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    async fn test_db() -> SqliteDb {
        let db = SqliteDb::connect(":memory:").await.unwrap();
        sqlx::migrate!("./migrations").run(&db.pool).await.unwrap();
        db
    }

    fn new_experiment(name: &str, expires_at: i64) -> NewExperiment {
        NewExperiment {
            name: name.to_string(),
            variants: variants(),
            allowed_user_ids: vec![],
            feed_learning: false,
            expires_at,
            retain_content: false,
            content_retention_days: 0,
        }
    }

    #[tokio::test]
    async fn load_from_then_close_expired_and_reload_rejects_the_same_header() {
        let db = test_db().await;
        let created = db.create(new_experiment("soon", NOW + 60)).await.unwrap();
        let reg = ExperimentRegistry::default();
        reg.load_from(&db).await.unwrap();
        assert_eq!(reg.len(), 1);

        let header = format!("{}:control", created.id);
        let b = bind(&reg, &header, json!({}), 1).unwrap().unwrap();
        assert_eq!(b.variant, "control");
        assert!(!b.retain_content);

        // The tick's sequence: close what has expired, then reload.
        let closed = db
            .close_expired(NOW + 60, "2026-09-02T00:00:00+00:00")
            .await
            .unwrap();
        assert_eq!(closed, vec![created.id]);
        reg.load_from(&db).await.unwrap();
        assert_eq!(bind(&reg, &header, json!({}), 1).unwrap_err(), BindError::Closed(created.id));
    }

    #[tokio::test]
    async fn row_written_directly_is_bindable_after_load_from() {
        let db = test_db().await;
        let variants_json = serde_json::to_string(&variants()).unwrap();
        sqlx::query(
            "INSERT INTO experiments (id, name, variants, allowed_user_ids, status, feed_learning, \
             expires_at, created_at, closed_at, retain_content, content_retention_days) \
             VALUES (42, 'raw', ?, '[9]', 'active', 0, 0, '2026-09-01T00:00:00+00:00', NULL, 1, 0)",
        )
        .bind(&variants_json)
        .execute(&db.pool)
        .await
        .unwrap();

        let reg = ExperimentRegistry::default();
        assert!(reg.is_empty());
        assert_eq!(
            bind(&reg, "42:candidate", json!({}), 9).unwrap_err(),
            BindError::UnknownExperiment(42)
        );
        reg.load_from(&db).await.unwrap();
        let b = bind(&reg, "42:candidate", json!({}), 9).unwrap().unwrap();
        assert_eq!(b.experiment_id, 42);
        assert_eq!(b.overlay["deep"], "anthropic/claude-opus");
        assert!(b.retain_content);
        assert_eq!(
            bind(&reg, "42:candidate", json!({}), 1).unwrap_err(),
            BindError::UserNotAllowed { experiment_id: 42, user_id: 1 }
        );
    }
}
