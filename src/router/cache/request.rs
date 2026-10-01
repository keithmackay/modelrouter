//! Per-request cache directives.
//!
//! A caller can steer the response cache for one request with headers. They
//! are parsed once at the handler boundary into [`CacheDirectives`]; a
//! malformed value is a 400, never silently ignored. The directives and the
//! operator's policy are then combined into a [`CachePlan`], which is all the
//! handler consults afterwards.

use axum::http::HeaderMap;

use super::store::EntryTtl;

/// Request header selecting the cache mode (`use`, `bypass`, `refresh`). The
/// response carries the outcome under the same name.
pub const MODE_HEADER: &str = "x-modelrouter-cache";

/// Request header setting the TTL of the entry this request stores, in whole
/// seconds; `0` asks for an entry that never expires. Capped by
/// `cache.max_ttl_seconds`.
pub const TTL_HEADER: &str = "x-modelrouter-cache-ttl";

/// Largest TTL a caller can write, ten years. Anything longer is a request
/// for "forever", which is spelled `0`; refusing it keeps absurd values away
/// from the store's expiry arithmetic.
const MAX_REQUESTED_TTL_SECS: u64 = 10 * 365 * 24 * 3600;

/// What the caller asked the cache to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CacheMode {
    /// No header: the operator's eligibility rules decide.
    #[default]
    Default,
    /// Serve from and store to the cache even when the request would not be
    /// eligible by default (e.g. a sampled temperature).
    Use,
    /// Neither read nor write the cache.
    Bypass,
    /// Skip the lookup, call the provider, and store the fresh answer.
    Refresh,
}

/// The caller's cache directives for one request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheDirectives {
    pub mode: CacheMode,
    /// TTL for an entry this request stores, before the operator's cap.
    /// `None` keeps the class default.
    pub ttl: Option<EntryTtl>,
}

impl CacheDirectives {
    /// Parse the directives from request headers. The error names the header
    /// and the accepted values, for the 400 body.
    pub fn from_headers(headers: &HeaderMap) -> Result<Self, String> {
        let mode = match header_str(headers, MODE_HEADER)? {
            None => CacheMode::Default,
            Some(v) if v.eq_ignore_ascii_case("use") => CacheMode::Use,
            Some(v) if v.eq_ignore_ascii_case("bypass") => CacheMode::Bypass,
            Some(v) if v.eq_ignore_ascii_case("refresh") => CacheMode::Refresh,
            Some(v) => {
                return Err(format!(
                    "{MODE_HEADER} must be one of use, bypass, refresh (got {v:?})"
                ))
            }
        };
        let ttl = match header_str(headers, TTL_HEADER)? {
            None => None,
            Some(v) => match v.parse::<u64>() {
                Ok(secs) if secs <= MAX_REQUESTED_TTL_SECS => Some(EntryTtl::from_secs(secs)),
                _ => {
                    return Err(format!(
                        "{TTL_HEADER} must be a whole number of seconds up to \
                         {MAX_REQUESTED_TTL_SECS}, or 0 for no expiry (got {v:?})"
                    ))
                }
            },
        };
        Ok(Self { mode, ttl })
    }
}

/// The trimmed value of `name`, `None` when absent or empty.
fn header_str<'h>(headers: &'h HeaderMap, name: &str) -> Result<Option<&'h str>, String> {
    let Some(value) = headers.get(name) else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| format!("{name} must be visible ASCII"))?
        .trim();
    Ok((!value.is_empty()).then_some(value))
}

/// What one request does with the cache, decided once from the operator's
/// policy and the caller's directives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePlan {
    /// The cache is not involved: off, disabled for this caller or class, or
    /// the request is not eligible.
    Skip,
    /// The caller asked to bypass the cache.
    Bypass,
    /// Look up, and store on a miss.
    LookupAndStore,
    /// The caller asked to refresh: no lookup, store the fresh answer.
    StoreOnly,
}

impl CachePlan {
    /// Decide the plan. `class_enabled` is the operator's switch for this
    /// cache class, `eligible` the default eligibility of the request, and
    /// `allow_opt_in` whether callers may widen eligibility (`use`,
    /// `refresh`). `bypass` only ever narrows, so it is always honoured.
    pub fn decide(
        mode: CacheMode,
        class_enabled: bool,
        eligible: bool,
        allow_opt_in: bool,
    ) -> Self {
        if !class_enabled {
            return CachePlan::Skip;
        }
        match mode {
            CacheMode::Bypass => CachePlan::Bypass,
            CacheMode::Use if allow_opt_in => CachePlan::LookupAndStore,
            CacheMode::Refresh if allow_opt_in => CachePlan::StoreOnly,
            _ if eligible => CachePlan::LookupAndStore,
            _ => CachePlan::Skip,
        }
    }

    pub fn lookup(self) -> bool {
        self == CachePlan::LookupAndStore
    }

    pub fn store(self) -> bool {
        matches!(self, CachePlan::LookupAndStore | CachePlan::StoreOnly)
    }

    /// The `x-modelrouter-cache` value for a response that did not come from
    /// the cache; `None` when the cache was not involved.
    pub fn miss_header(self) -> Option<&'static str> {
        match self {
            CachePlan::Skip => None,
            CachePlan::Bypass => Some("BYPASS"),
            CachePlan::LookupAndStore => Some("MISS"),
            CachePlan::StoreOnly => Some("REFRESH"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn directives(value: &str) -> Result<CacheDirectives, String> {
        with_header(MODE_HEADER, value)
    }

    fn with_header(name: &'static str, value: &str) -> Result<CacheDirectives, String> {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(value).unwrap());
        CacheDirectives::from_headers(&headers)
    }

    #[test]
    fn ttl_header_parses_seconds_and_zero_as_unlimited() {
        use std::time::Duration;
        assert_eq!(
            CacheDirectives::from_headers(&HeaderMap::new())
                .unwrap()
                .ttl,
            None
        );
        assert_eq!(
            with_header(TTL_HEADER, " 90 ").unwrap().ttl,
            Some(EntryTtl::Finite(Duration::from_secs(90)))
        );
        assert_eq!(
            with_header(TTL_HEADER, "0").unwrap().ttl,
            Some(EntryTtl::Unlimited)
        );
        assert_eq!(with_header(TTL_HEADER, "").unwrap().ttl, None);
        assert!(with_header(TTL_HEADER, &MAX_REQUESTED_TTL_SECS.to_string()).is_ok());
    }

    #[test]
    fn malformed_ttl_is_rejected() {
        let too_long = (MAX_REQUESTED_TTL_SECS + 1).to_string();
        for bad in ["-1", "1.5", "an hour", too_long.as_str()] {
            let err = with_header(TTL_HEADER, bad).unwrap_err();
            assert!(err.contains("0 for no expiry"), "{bad}: {err}");
        }
    }

    #[test]
    fn mode_header_parses_case_insensitively() {
        assert_eq!(
            CacheDirectives::from_headers(&HeaderMap::new())
                .unwrap()
                .mode,
            CacheMode::Default
        );
        assert_eq!(directives("use").unwrap().mode, CacheMode::Use);
        assert_eq!(directives(" Bypass ").unwrap().mode, CacheMode::Bypass);
        assert_eq!(directives("REFRESH").unwrap().mode, CacheMode::Refresh);
        assert_eq!(directives("").unwrap().mode, CacheMode::Default);
    }

    #[test]
    fn unknown_mode_is_rejected_with_the_accepted_values() {
        let err = directives("sometimes").unwrap_err();
        assert!(err.contains("use, bypass, refresh"), "{err}");
    }

    #[test]
    fn plan_table() {
        use CacheMode::*;
        use CachePlan as P;
        // (mode, class_enabled, eligible, allow_opt_in) -> plan
        let cases = [
            (Default, true, true, true, P::LookupAndStore),
            (Default, true, false, true, P::Skip),
            (Use, true, false, true, P::LookupAndStore),
            (Use, true, false, false, P::Skip),
            (Use, true, true, false, P::LookupAndStore),
            (Refresh, true, false, true, P::StoreOnly),
            (Refresh, true, true, false, P::LookupAndStore),
            (Refresh, true, false, false, P::Skip),
            (Bypass, true, true, true, P::Bypass),
            (Bypass, true, true, false, P::Bypass),
            (Use, false, true, true, P::Skip),
            (Bypass, false, true, true, P::Skip),
        ];
        for (mode, class_enabled, eligible, opt_in, want) in cases {
            assert_eq!(
                CachePlan::decide(mode, class_enabled, eligible, opt_in),
                want,
                "{mode:?} class_enabled={class_enabled} eligible={eligible} opt_in={opt_in}"
            );
        }
    }

    #[test]
    fn plan_flags_and_headers() {
        assert!(CachePlan::LookupAndStore.lookup() && CachePlan::LookupAndStore.store());
        assert!(!CachePlan::StoreOnly.lookup() && CachePlan::StoreOnly.store());
        assert!(!CachePlan::Bypass.lookup() && !CachePlan::Bypass.store());
        assert!(!CachePlan::Skip.lookup() && !CachePlan::Skip.store());
        assert_eq!(CachePlan::Skip.miss_header(), None);
        assert_eq!(CachePlan::Bypass.miss_header(), Some("BYPASS"));
        assert_eq!(CachePlan::LookupAndStore.miss_header(), Some("MISS"));
        assert_eq!(CachePlan::StoreOnly.miss_header(), Some("REFRESH"));
    }
}
