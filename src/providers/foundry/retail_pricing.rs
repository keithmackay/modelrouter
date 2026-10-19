//! Foundry deployment prices from the Azure Retail Prices API.
//!
//! `https://prices.azure.com/api/retail/prices` is Azure's public, unauthenticated
//! list-price feed. Foundry model meters live under `serviceName eq 'Foundry
//! Models'`, but their names are abbreviated and irregular ("5.4 mini cd Inp Gl
//! 1M Tokens", "gpt 4.1 Inp glbl Tokens", "Phi-4-Input Tokens"), and the vendor
//! is often only in the product name ("Azure Grok Models" + "4.3 Inp Glbl"). So
//! each meter is split into model tokens, a rate kind (input, output, cache
//! read, cache write) and qualifiers, and matched to the configured deployment
//! names.
//!
//! Only standard pay-as-you-go rates count: batch, priority processing, flex,
//! long-context, fine-tuned, data-zone and regional meters are skipped. Global
//! meters are preferred; a model whose meters carry no deployment-type
//! qualifier at all (Phi) falls back to those. Matching runs in tiers, from
//! exact to progressively looser, and a tier counts only when it yields both an
//! input and an output rate (input alone for embedding models). Deployments no
//! tier prices are returned as unpriced, for the operator to price with
//! `[[pricing]]`.
use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;

use crate::config::schema::PricingEntry;

/// The public endpoint; `retail_pricing_url` overrides it (tests, proxies).
pub const DEFAULT_RETAIL_PRICES_URL: &str = "https://prices.azure.com/api/retail/prices";
/// Default refresh interval. Azure list prices change rarely; daily is plenty.
pub const DEFAULT_REFRESH_HOURS: u64 = 24;
/// Pagination guard: one region's Foundry meters are a couple of thousand rows
/// at 1,000 per page, so a runaway `NextPageLink` chain stops here.
const MAX_PAGES: usize = 50;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetailItem {
    pub meter_name: String,
    pub product_name: String,
    pub unit_of_measure: String,
    pub retail_price: f64,
    #[serde(default)]
    pub effective_start_date: String,
}

#[derive(Debug, Deserialize)]
struct RetailPage {
    #[serde(rename = "Items", default)]
    items: Vec<RetailItem>,
    #[serde(rename = "NextPageLink", default)]
    next_page_link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Input,
    Output,
    CacheRead,
    CacheWrite,
}

#[derive(Debug, Clone)]
struct Meter {
    model: HashSet<String>,
    family: HashSet<String>,
    kind: Kind,
    global: bool,
    per_million: f64,
    start: String,
}

/// Qualifiers that make a meter a non-standard rate for the same model.
const SKIP: &[&str] = &[
    "batch", "pp", "flex", "fl", "longco", "loco", "l", "dz", "dzone", "datazone", "regnl",
    "regional", "ft", "dev", "training", "rft", "hosting", "aud", "audio", "img", "image", "rt",
    "realtime", "txt", "fw", "grader", "mm", "new",
];
/// Words that carry no model identity.
const NOISE: &[&str] = &["std", "shortco", "shco", "tokens", "1m", "1k"];
const GLOBAL: &[&str] = &["gl", "glbl", "global", "glb"];
const CACHED: &[&str] = &["cd", "cchd", "ccchd", "cached", "cache", "ch"];
const WRITE: &[&str] = &["wr", "write"];
/// Deployment- or meter-side tokens the loosest tier ignores.
const IGNORABLE: &[&str] = &["instruct", "fp8", "thinking"];
/// Product-name words that are not a model family.
const PRODUCT_NOISE: &[&str] =
    &["azure", "models", "model", "openai", "reasoning", "media", "oss", "embedding"];

fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

fn kind_word(t: &str) -> Option<Kind> {
    match t {
        "inp" | "inpt" | "input" | "in" => Some(Kind::Input),
        "opt" | "outp" | "outpt" | "out" | "output" => Some(Kind::Output),
        _ => None,
    }
}

/// A date-like version token: `0731`, `202605`, `05052026`, or a zero-led month.
fn is_date(t: &str) -> bool {
    t.chars().all(|c| c.is_ascii_digit())
        && (matches!(t.len(), 4 | 6 | 8) || (t.len() == 2 && t.starts_with('0')))
}

/// An expert-count token such as `128e`.
fn is_expert_count(t: &str) -> bool {
    t.len() > 1 && t.ends_with('e') && t[..t.len() - 1].chars().all(|c| c.is_ascii_digit())
}

fn family(product: &str) -> HashSet<String> {
    tokens(product)
        .into_iter()
        .filter(|t| !PRODUCT_NOISE.contains(&t.as_str()))
        .map(|t| t.trim_end_matches(|c: char| c.is_ascii_digit()).to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn parse(item: &RetailItem) -> Option<Meter> {
    let scale = match item.unit_of_measure.as_str() {
        "1K" => 1000.0,
        "1M" => 1.0,
        _ => return None,
    };
    if !item.meter_name.ends_with("Tokens") {
        return None;
    }
    let (mut kind, mut cached, mut write, mut global) = (None, false, false, false);
    let mut model = HashSet::new();
    for t in tokens(&item.meter_name) {
        let t = t.as_str();
        if SKIP.contains(&t) {
            return None;
        }
        if let Some(k) = kind_word(t) {
            kind = Some(k);
        } else if CACHED.contains(&t) {
            cached = true;
        } else if WRITE.contains(&t) {
            write = true;
        } else if GLOBAL.contains(&t) {
            global = true;
        } else if !NOISE.contains(&t) {
            model.insert(t.to_string());
        }
    }
    let embedding = model.iter().any(|t| t.contains("embedding"))
        || item.product_name.to_lowercase().contains("embedding");
    if kind.is_none() && (cached || embedding) {
        kind = Some(Kind::Input);
    }
    let kind = match kind? {
        Kind::Input if write => Kind::CacheWrite,
        Kind::Input if cached => Kind::CacheRead,
        k => k,
    };
    Some(Meter {
        model,
        family: family(&item.product_name),
        kind,
        global,
        per_million: item.retail_price * scale,
        start: item.effective_start_date.clone(),
    })
}

fn loose(set: &HashSet<String>, family: &HashSet<String>) -> HashSet<String> {
    set.iter()
        .filter(|t| {
            !is_date(t) && !is_expert_count(t) && !IGNORABLE.contains(&t.as_str()) && !family.contains(*t)
        })
        .cloned()
        .collect()
}

fn with_family(m: &Meter) -> HashSet<String> {
    m.model.union(&m.family).cloned().collect()
}

/// Matching tiers, exact first.
fn tier_matches(tier: usize, m: &Meter, want: &HashSet<String>) -> bool {
    match tier {
        0 => with_family(m) == *want,
        // The meter carries a version date the deployment name omits.
        1 => {
            !want.iter().any(|t| is_date(t))
                && with_family(m).into_iter().filter(|t| !is_date(t)).collect::<HashSet<_>>() == *want
        }
        // Family, dates, expert counts and decorative words ignored on both sides.
        _ => {
            let w = loose(want, &m.family);
            !w.is_empty() && loose(&m.model, &m.family) == w
        }
    }
}

/// The meters of one dated version: when matches span several (gpt-4o's 0513
/// and 0806), the version with the most recent effective date wins.
fn newest_version(matched: Vec<&Meter>) -> Vec<&Meter> {
    let mut groups: BTreeMap<Vec<String>, Vec<&Meter>> = BTreeMap::new();
    for m in matched {
        let mut dates: Vec<String> = m.model.iter().filter(|t| is_date(t)).cloned().collect();
        dates.sort();
        groups.entry(dates).or_default().push(m);
    }
    groups
        .into_iter()
        .max_by(|(ka, a), (kb, b)| {
            let sa = a.iter().map(|m| m.start.as_str()).max();
            let sb = b.iter().map(|m| m.start.as_str()).max();
            sa.cmp(&sb).then_with(|| ka.cmp(kb))
        })
        .map(|(_, g)| g)
        .unwrap_or_default()
}

fn rates(deployment: &str, matched: Vec<&Meter>) -> Option<PricingEntry> {
    let mut by_kind: Vec<(Kind, &Meter)> = Vec::new();
    for m in newest_version(matched) {
        match by_kind.iter_mut().find(|(k, _)| *k == m.kind) {
            Some(slot) if slot.1.start < m.start => slot.1 = m,
            Some(_) => {}
            None => by_kind.push((m.kind, m)),
        }
    }
    let get = |k: Kind| by_kind.iter().find(|(kk, _)| *kk == k).map(|(_, m)| m.per_million);
    let input = get(Kind::Input)?;
    let output = match get(Kind::Output) {
        Some(o) => o,
        None if deployment.to_lowercase().contains("embedding") => 0.0,
        None => return None,
    };
    Some(PricingEntry {
        model: deployment.to_string(),
        input_per_million: input,
        output_per_million: output,
        cache_read_per_million: get(Kind::CacheRead),
        cache_write_per_million: get(Kind::CacheWrite),
    })
}

/// Price `deployments` from one region's retail items. Returns the priced
/// entries (keyed by deployment name) and the deployments left unpriced.
pub fn price_deployments(items: &[RetailItem], deployments: &[String]) -> (Vec<PricingEntry>, Vec<String>) {
    let meters: Vec<Meter> = items.iter().filter_map(parse).collect();
    let mut priced = Vec::new();
    let mut unpriced = Vec::new();
    'deployments: for d in deployments {
        let want: HashSet<String> = tokens(d).into_iter().collect();
        for tier in 0..3 {
            for global in [true, false] {
                let matched: Vec<&Meter> = meters
                    .iter()
                    .filter(|m| m.global == global && tier_matches(tier, m, &want))
                    .collect();
                if matched.is_empty() {
                    continue;
                }
                if let Some(entry) = rates(d, matched) {
                    priced.push(entry);
                    continue 'deployments;
                }
            }
        }
        unpriced.push(d.clone());
    }
    (priced, unpriced)
}

/// The pseudo-model a Grounding with Bing search is priced under.
pub const BING_GROUNDING_PRICING_KEY: &str = "search/bing_grounding";

/// Dollars per Grounding with Bing search from the retail feed's
/// `MS Bing Services` / `Grounding with Bing` meters: "Search Transactions",
/// or "Custom Search Transactions" for the domain-restricted variant. The
/// meters are priced per 1,000 transactions. None when the feed lacks it.
pub fn bing_grounding_rate(items: &[RetailItem], custom_search: bool) -> Option<f64> {
    let meter = if custom_search { "Custom Search Transactions" } else { "Search Transactions" };
    items
        .iter()
        .filter(|i| i.product_name == "Grounding with Bing" && i.meter_name == meter && i.retail_price > 0.0)
        .filter_map(|i| match i.unit_of_measure.as_str() {
            "1K" => Some(i.retail_price / 1000.0),
            "1" => Some(i.retail_price),
            _ => None,
        })
        .next()
}

/// The Grounding with Bing meters (they are not regional).
pub async fn fetch_bing_items(client: &reqwest::Client, base_url: &str) -> Result<Vec<RetailItem>, String> {
    let filter = "serviceName eq 'MS Bing Services' and productName eq 'Grounding with Bing' and priceType eq 'Consumption'";
    let resp = client
        .get(base_url)
        .query(&[("$filter", filter)])
        .send()
        .await
        .map_err(|e| format!("retail prices request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("retail prices API answered {} for Grounding with Bing", resp.status()));
    }
    let page: RetailPage = resp.json().await.map_err(|e| format!("retail prices response unreadable: {e}"))?;
    Ok(page.items)
}

/// Every consumption meter for Foundry Models in `region`, following pagination.
pub async fn fetch_region(client: &reqwest::Client, base_url: &str, region: &str) -> Result<Vec<RetailItem>, String> {
    let filter = format!(
        "serviceName eq 'Foundry Models' and armRegionName eq '{region}' and priceType eq 'Consumption'"
    );
    let mut request = client.get(base_url).query(&[("$filter", filter.as_str())]);
    let mut items = Vec::new();
    for _ in 0..MAX_PAGES {
        let resp = request.send().await.map_err(|e| format!("retail prices request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("retail prices API answered {status} for region {region}"));
        }
        let page: RetailPage = resp.json().await.map_err(|e| format!("retail prices response unreadable: {e}"))?;
        items.extend(page.items);
        match page.next_page_link.filter(|l| !l.is_empty()) {
            Some(next) => request = client.get(next),
            None => return Ok(items),
        }
    }
    Err(format!("retail prices for region {region} exceeded {MAX_PAGES} pages"))
}

/// Price `deployments` from `regions` in order: each region prices what the
/// earlier ones left unpriced, so a model missing from the first region's
/// feed takes the next region's rate rather than none.
pub async fn fetch_prices(
    client: &reqwest::Client,
    base_url: &str,
    regions: &[String],
    deployments: &[String],
) -> Result<(Vec<PricingEntry>, Vec<String>), String> {
    let mut priced = Vec::new();
    let mut remaining = deployments.to_vec();
    for region in regions {
        if remaining.is_empty() {
            break;
        }
        let items = fetch_region(client, base_url, region).await?;
        let (found, left) = price_deployments(&items, &remaining);
        tracing::debug!(region = %region, priced = found.len(), "retail prices matched");
        priced.extend(found);
        remaining = left;
    }
    Ok((priced, remaining))
}

/// Refresh `cost_calc`'s live Foundry prices now and then every
/// `refresh_hours`. A failed refresh keeps the previous prices and retries
/// within the hour; it never takes the router down.
///
/// `bing_custom_search`: Some when a `bing_grounding` search provider is
/// configured (its `custom_search` flag); its per-query price is refreshed
/// with the models'. A feed without it leaves searches unpriced, with a warning.
pub fn spawn_refresh(
    cost_calc: std::sync::Arc<crate::router::cost::CostCalculator>,
    base_url: String,
    regions: Vec<String>,
    deployments: Vec<String>,
    refresh_hours: u64,
    bing_custom_search: Option<bool>,
) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "foundry retail pricing disabled: HTTP client unavailable");
                return;
            }
        };
        let every = std::time::Duration::from_secs(refresh_hours.max(1) * 3600);
        loop {
            let wait = match fetch_prices(&client, &base_url, &regions, &deployments).await {
                Ok((mut priced, unpriced)) => {
                    if let Some(custom) = bing_custom_search {
                        match fetch_bing_items(&client, &base_url).await.map(|items| bing_grounding_rate(&items, custom)) {
                            Ok(Some(rate)) => priced.push(PricingEntry {
                                model: BING_GROUNDING_PRICING_KEY.to_string(),
                                input_per_million: rate,
                                output_per_million: 0.0,
                                cache_read_per_million: None,
                                cache_write_per_million: None,
                            }),
                            Ok(None) => tracing::warn!("the retail price feed has no Grounding with Bing price; searches are recorded unpriced"),
                            Err(e) => tracing::warn!(error = %e, "Grounding with Bing price unavailable; searches are recorded unpriced"),
                        }
                    }
                    let count = cost_calc.replace_live_pricing(&priced);
                    tracing::info!(
                        priced = count,
                        unpriced = ?unpriced,
                        regions = ?regions,
                        "foundry deployment prices refreshed from the Azure Retail Prices API"
                    );
                    every
                }
                Err(e) => {
                    tracing::warn!(error = %e, "foundry retail price refresh failed; keeping the previous prices");
                    every.min(std::time::Duration::from_secs(3600))
                }
            };
            tokio::time::sleep(wait).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(product: &str, meter: &str, unit: &str, price: f64) -> RetailItem {
        dated(product, meter, unit, price, "2026-01-01T00:00:00Z")
    }

    fn dated(product: &str, meter: &str, unit: &str, price: f64, start: &str) -> RetailItem {
        RetailItem {
            meter_name: meter.into(),
            product_name: product.into(),
            unit_of_measure: unit.into(),
            retail_price: price,
            effective_start_date: start.into(),
        }
    }

    fn price(items: &[RetailItem], name: &str) -> Option<PricingEntry> {
        let (priced, _) = price_deployments(items, &[name.to_string()]);
        priced.into_iter().next()
    }

    #[test]
    fn prices_a_gpt_deployment_with_cache_rates_and_skips_non_standard_meters() {
        let items = vec![
            item("Azure OpenAI GPT5", "5.6 sol ShortCo Inp Std Gl 1M Tokens", "1M", 4.0),
            item("Azure OpenAI GPT5", "5.6 sol ShortCo Opt Std Gl 1M Tokens", "1M", 20.0),
            item("Azure OpenAI GPT5", "5.6 sol ShortCo Cd Inp Std Gl 1M Tokens", "1M", 0.4),
            item("Azure OpenAI GPT5", "5.6 sol ShortCo Cd Wr Std Gl 1M Tokens", "1M", 5.0),
            item("Azure OpenAI GPT5", "5.6 sol LongCo Inp Std Gl 1M Tokens", "1M", 8.0),
            item("Azure OpenAI GPT5", "5.6 sol ShortCo Inp PP Gl 1M Tokens", "1M", 8.0),
            item("Azure OpenAI GPT5", "5.6 sol ShortCo Inp Std DZ 1M Tokens", "1M", 4.4),
            item("Azure OpenAI GPT5", "56sol ShCo Inp Fl Gl 1M Tokens", "1M", 2.0),
        ];
        let p = price(&items, "gpt-5.6-sol").expect("priced");
        assert_eq!((p.input_per_million, p.output_per_million), (4.0, 20.0));
        assert_eq!(p.cache_read_per_million, Some(0.4));
        assert_eq!(p.cache_write_per_million, Some(5.0));
    }

    #[test]
    fn converts_per_thousand_meters_and_tells_mini_from_the_base_model() {
        let items = vec![
            item("Azure OpenAI", "gpt 4.1 Inp glbl Tokens", "1K", 0.002),
            item("Azure OpenAI", "gpt 4.1 Outp glbl Tokens", "1K", 0.008),
            item("Azure OpenAI", "gpt 4.1 mini Inp glbl Tokens", "1K", 0.0004),
            item("Azure OpenAI", "gpt 4.1 mini Outp glbl Tokens", "1K", 0.0016),
            item("Azure OpenAI", "gpt 4.1 mini Batch Inp glbl Tokens", "1K", 0.0002),
        ];
        let base = price(&items, "gpt-4.1").unwrap();
        assert!((base.input_per_million - 2.0).abs() < 1e-9 && (base.output_per_million - 8.0).abs() < 1e-9);
        let mini = price(&items, "gpt-4.1-mini").unwrap();
        assert!((mini.input_per_million - 0.4).abs() < 1e-9 && (mini.output_per_million - 1.6).abs() < 1e-9);
    }

    #[test]
    fn takes_the_vendor_from_the_product_name() {
        let items = vec![
            item("Azure Grok Models", "4.3 Inp Glbl Tokens", "1K", 0.00125),
            item("Azure Grok Models", "4.3 Outp Glbl Tokens", "1K", 0.0025),
            item("Azure Grok Models", "4.3 Inp Glbl L Tokens", "1K", 0.0025),
        ];
        let p = price(&items, "grok-4.3").unwrap();
        assert!((p.input_per_million - 1.25).abs() < 1e-9 && (p.output_per_million - 2.5).abs() < 1e-9);
    }

    #[test]
    fn prefers_global_meters_and_falls_back_to_unqualified_ones() {
        let items = vec![
            item("Azure Phi Models", "Phi-4-Input Tokens", "1K", 0.000125),
            item("Azure Phi Models", "Phi-4-Output Tokens", "1K", 0.0005),
            item("Azure Deepseek Models", "V3.2 Inp glbl Tokens", "1K", 0.00058),
            item("Azure Deepseek Models", "V3.2 Outp glbl Tokens", "1K", 0.00168),
            item("Azure Fireworks Models", "FW DeepSeek V3.2 Inp Tokens", "1K", 0.00062),
        ];
        let phi = price(&items, "Phi-4").unwrap();
        assert!((phi.input_per_million - 0.125).abs() < 1e-9);
        let ds = price(&items, "DeepSeek-V3.2").unwrap();
        assert!((ds.input_per_million - 0.58).abs() < 1e-9);
    }

    #[test]
    fn loose_tier_ignores_family_dates_expert_counts_and_decorative_words() {
        let items = vec![
            item("Azure Mistral Models", "Codestral Inp glbl Tokens", "1K", 0.0003),
            item("Azure Mistral Models", "Codestral Outp glbl Tokens", "1K", 0.0009),
            item("Azure Llama Models", "Llama 4 Maverick 17B Inp glbl Tokens", "1K", 0.00025),
            item("Azure Llama Models", "Llama 4 Maverick 17B Outp glbl Tokens", "1K", 0.001),
        ];
        assert!(price(&items, "Codestral-2501").is_some());
        assert!(price(&items, "Llama-4-Maverick-17B-128E-Instruct-FP8").is_some());
        assert!(price(&items, "Llama-4-Scout-17B-16E-Instruct").is_none());
    }

    #[test]
    fn a_dated_family_takes_its_newest_version() {
        let items = vec![
            dated("Azure OpenAI", "gpt 4o 0513 Input global Tokens", "1K", 0.005, "2024-05-13T00:00:00Z"),
            dated("Azure OpenAI", "gpt 4o 0513 Output global Tokens", "1K", 0.015, "2024-05-13T00:00:00Z"),
            dated("Azure OpenAI", "gpt 4o 1120 Inp glbl Tokens", "1K", 0.0025, "2024-11-20T00:00:00Z"),
            dated("Azure OpenAI", "gpt 4o 1120 Outp glbl Tokens", "1K", 0.01, "2024-11-20T00:00:00Z"),
        ];
        let p = price(&items, "gpt-4o").unwrap();
        assert!((p.input_per_million - 2.5).abs() < 1e-9 && (p.output_per_million - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_tier_without_input_and_output_falls_through_to_a_looser_one() {
        let items = vec![
            item("Azure Kimi", "K2.5 cached glbl Tokens", "1K", 0.0001),
            item("Azure Kimi", "K2.5 Thinking Inp glbl Tokens", "1K", 0.0006),
            item("Azure Kimi", "K2.5 Thinking Outp glbl Tokens", "1K", 0.003),
        ];
        let p = price(&items, "Kimi-K2.5").unwrap();
        assert!((p.output_per_million - 3.0).abs() < 1e-9);
        assert_eq!(p.cache_read_per_million.map(|c| (c * 1e6).round()), Some(100000.0));
    }

    #[test]
    fn embeddings_are_priced_on_input_alone() {
        let items = vec![item("Azure OpenAI", "text-embedding-3-large-glbl Tokens", "1K", 0.00013)];
        let p = price(&items, "text-embedding-3-large").unwrap();
        assert!((p.input_per_million - 0.13).abs() < 1e-9);
        assert_eq!(p.output_per_million, 0.0);
    }

    #[test]
    fn reports_what_it_cannot_price_and_ignores_non_token_meters() {
        let items = vec![
            item("Azure Mistral Models", "OCR glbl Pages", "1K", 1.0),
            item("Azure Llama Models", "3.3 70b FT DZ Deployment Hosting Unit", "1/Hour", 0.33),
        ];
        let (priced, unpriced) = price_deployments(&items, &["model-router".to_string()]);
        assert!(priced.is_empty());
        assert_eq!(unpriced, vec!["model-router".to_string()]);
    }

    #[test]
    fn bing_grounding_is_priced_per_search_from_the_per_thousand_meter() {
        let items = vec![
            item("Grounding with Bing", "Search Transactions", "1K", 14.0),
            item("Grounding with Bing", "Custom Search Transactions", "1K", 18.0),
            item("MS Bing Custom Search", "S1 Transactions", "1K", 18.0),
        ];
        assert_eq!(bing_grounding_rate(&items, false), Some(0.014));
        assert_eq!(bing_grounding_rate(&items, true), Some(0.018));
        assert_eq!(bing_grounding_rate(&items[2..], false), None);
    }

    #[tokio::test]
    async fn fetch_follows_pages_and_falls_back_to_later_regions() {
        use axum::{extract::Query, routing::get, Json, Router};
        use std::collections::HashMap;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/prices", listener.local_addr().unwrap());
        let next = format!("{base}?page=2&region=first");
        let app = Router::new().route(
            "/prices",
            get(move |Query(q): Query<HashMap<String, String>>| {
                let next = next.clone();
                async move {
                    let filter = q.get("$filter").cloned().unwrap_or_default();
                    let body = if q.get("page").map(String::as_str) == Some("2") {
                        serde_json::json!({"Items": [
                            {"meterName": "gpt 4.1 Outp glbl Tokens", "productName": "Azure OpenAI", "unitOfMeasure": "1K", "retailPrice": 0.008}
                        ], "NextPageLink": null})
                    } else if filter.contains("'first'") {
                        serde_json::json!({"Items": [
                            {"meterName": "gpt 4.1 Inp glbl Tokens", "productName": "Azure OpenAI", "unitOfMeasure": "1K", "retailPrice": 0.002}
                        ], "NextPageLink": next})
                    } else {
                        serde_json::json!({"Items": [
                            {"meterName": "Phi-4-Input Tokens", "productName": "Azure Phi Models", "unitOfMeasure": "1K", "retailPrice": 0.000125},
                            {"meterName": "Phi-4-Output Tokens", "productName": "Azure Phi Models", "unitOfMeasure": "1K", "retailPrice": 0.0005}
                        ]})
                    };
                    Json(body)
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let regions = vec!["first".to_string(), "second".to_string()];
        let deployments = vec!["gpt-4.1".to_string(), "Phi-4".to_string(), "unknown-model".to_string()];
        let (priced, unpriced) = fetch_prices(&client, &base, &regions, &deployments).await.unwrap();
        let names: Vec<&str> = priced.iter().map(|p| p.model.as_str()).collect();
        assert_eq!(names, vec!["gpt-4.1", "Phi-4"]);
        assert_eq!(unpriced, vec!["unknown-model".to_string()]);
    }

    #[tokio::test]
    async fn fetch_reports_an_http_error() {
        use axum::{http::StatusCode, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/prices", listener.local_addr().unwrap());
        let app = Router::new().route("/prices", get(|| async { StatusCode::SERVICE_UNAVAILABLE }));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let err = fetch_region(&reqwest::Client::new(), &base, "x").await.unwrap_err();
        assert!(err.contains("503"), "{err}");
    }

    /// Against the real public API: `cargo test --lib retail_pricing -- --ignored --nocapture`.
    /// `RETAIL_PRICING_REGIONS` (comma-separated) and `RETAIL_PRICING_DEPLOYMENTS`
    /// override the defaults.
    #[tokio::test]
    #[ignore = "calls the public Azure Retail Prices API"]
    async fn live_retail_prices_api() {
        let split = |v: String| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let regions = split(std::env::var("RETAIL_PRICING_REGIONS").unwrap_or_else(|_| "eastus2".into()));
        let deployments = split(
            std::env::var("RETAIL_PRICING_DEPLOYMENTS").unwrap_or_else(|_| "gpt-4.1,gpt-4.1-mini,text-embedding-3-large".into()),
        );
        let (priced, unpriced) = fetch_prices(&reqwest::Client::new(), DEFAULT_RETAIL_PRICES_URL, &regions, &deployments)
            .await
            .expect("retail prices API reachable");
        for p in &priced {
            println!(
                "{:45} in {:>9.4} out {:>9.4} cache_read {:?} cache_write {:?}",
                p.model, p.input_per_million, p.output_per_million, p.cache_read_per_million, p.cache_write_per_million
            );
        }
        println!("unpriced: {unpriced:?}");
        assert!(!priced.is_empty());
    }
}
