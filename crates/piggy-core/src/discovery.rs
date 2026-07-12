//! Saver discovery (M3): surface candidate token-savers from GitHub search.
//!
//! Piggy searches GitHub (unauthenticated, best-effort) for the
//! `token-optimization` / `claude-code` topics plus free text, merges and dedups
//! the hits, drops anything already curated in the catalog, and caches the
//! result at `~/.piggy/discovered.json`. The cache refreshes at most once a day
//! (a `--refresh` flag forces it). Catalog entries flagged `listed_only` — known
//! tools Piggy deliberately will not install — are always shown here with their
//! `exclusionReason`.
//!
//! The network layer is thin and isolated; the parse/merge logic is pure and
//! unit-tested against fixture JSON (tests never hit the network).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::registry::Catalog;

/// Catalog status marking a tool that is listed for transparency but never
/// installable.
const LISTED_ONLY: &str = "listed_only";

/// The GitHub topics we search, plus free-text queries. Results are merged.
const QUERIES: &[&str] = &[
    "topic:claude-code topic:token-optimization",
    "topic:claude-code token optimizer",
    "topic:token-optimization",
    "claude code token saver in:name,description,readme",
];

/// One discovered repository (or a listed-only catalog entry surfaced here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredRepo {
    pub full_name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub stars: u64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub topics: Vec<String>,
    /// True for a catalog `listed_only` entry (shown, never installable).
    #[serde(default)]
    pub listed_only: bool,
    /// Why a `listed_only` entry is excluded (verbatim from the catalog).
    #[serde(default)]
    pub exclusion_reason: Option<String>,
}

/// The on-disk discovery cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveryCache {
    /// RFC3339 timestamp of the last successful (or attempted) refresh.
    pub refreshed_at: String,
    /// Merged, deduped, catalog-filtered results, sorted by stars (desc).
    pub repos: Vec<DiscoveredRepo>,
    /// True when the results are served from a stale cache because the network
    /// refresh failed (e.g. rate-limited).
    #[serde(default)]
    pub stale: bool,
}

// ---------------------------------------------------------------------------
// Pure parse / merge (unit-tested, no network)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct GhSearchResponse {
    #[serde(default)]
    items: Vec<GhItem>,
}

#[derive(Deserialize)]
struct GhItem {
    full_name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    stargazers_count: u64,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    topics: Vec<String>,
}

/// Parse a GitHub `/search/repositories` JSON response into repos.
pub fn parse_search_response(json: &str) -> Result<Vec<DiscoveredRepo>> {
    let resp: GhSearchResponse =
        serde_json::from_str(json).context("parsing GitHub search response")?;
    Ok(resp
        .items
        .into_iter()
        .map(|it| DiscoveredRepo {
            full_name: it.full_name,
            description: it.description,
            stars: it.stargazers_count,
            url: it.html_url,
            topics: it.topics,
            listed_only: false,
            exclusion_reason: None,
        })
        .collect())
}

/// Merge several search result batches, dedup by `full_name` (keeping the
/// highest star count), drop repos already curated in the catalog, sort by stars
/// (desc, then name), and append the catalog's `listed_only` entries with their
/// exclusion reasons.
pub fn merge_and_filter(
    batches: Vec<Vec<DiscoveredRepo>>,
    catalog: &Catalog,
) -> Vec<DiscoveredRepo> {
    use std::collections::BTreeMap;

    // Repos already curated (installable) — never re-suggest these.
    let curated: std::collections::HashSet<String> = catalog
        .entries
        .iter()
        .filter(|e| e.status != LISTED_ONLY)
        .filter_map(|e| e.source.repo.clone())
        .collect();

    let mut by_name: BTreeMap<String, DiscoveredRepo> = BTreeMap::new();
    for batch in batches {
        for repo in batch {
            if curated.contains(&repo.full_name) {
                continue;
            }
            by_name
                .entry(repo.full_name.clone())
                .and_modify(|existing| {
                    if repo.stars > existing.stars {
                        existing.stars = repo.stars;
                    }
                    if existing.description.is_none() {
                        existing.description = repo.description.clone();
                    }
                    if existing.topics.is_empty() {
                        existing.topics = repo.topics.clone();
                    }
                })
                .or_insert(repo);
        }
    }

    let mut out: Vec<DiscoveredRepo> = by_name.into_values().collect();
    out.sort_by(|a, b| {
        b.stars
            .cmp(&a.stars)
            .then_with(|| a.full_name.cmp(&b.full_name))
    });

    // Append listed-only catalog entries (shown for transparency).
    for e in &catalog.entries {
        if e.status == LISTED_ONLY {
            out.push(DiscoveredRepo {
                full_name: e.source.repo.clone().unwrap_or_else(|| e.id.clone()),
                description: Some(e.description.clone()),
                stars: 0,
                url: e
                    .source
                    .repo
                    .as_ref()
                    .map(|r| format!("https://github.com/{r}"))
                    .unwrap_or_default(),
                topics: Vec::new(),
                listed_only: true,
                exclusion_reason: e.exclusion_reason.clone(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Cache + network
// ---------------------------------------------------------------------------

/// Whether `refreshed_at` (RFC3339) is within the last day of now.
fn is_fresh(refreshed_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(refreshed_at) {
        Ok(ts) => {
            let age = chrono::Utc::now().signed_duration_since(ts.with_timezone(&chrono::Utc));
            age < chrono::Duration::days(1)
        }
        Err(_) => false,
    }
}

/// Load the cache from disk, if present and parseable.
fn load_cache() -> Option<DiscoveryCache> {
    let bytes = std::fs::read(config::discovered_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Persist the cache atomically-ish (write + rename via a temp file).
fn save_cache(cache: &DiscoveryCache) -> Result<()> {
    let path = config::discovered_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Run all searches against the live GitHub API. Returns an error on network
/// failure or an explicit rate-limit so the caller can fall back to cache.
fn fetch_all() -> Result<Vec<Vec<DiscoveredRepo>>> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("piggy-discovery/0.1")
        .build()?;
    let mut batches = Vec::new();
    for q in QUERIES {
        let resp = client
            .get("https://api.github.com/search/repositories")
            .header("Accept", "application/vnd.github+json")
            .query(&[
                ("q", *q),
                ("sort", "stars"),
                ("order", "desc"),
                ("per_page", "30"),
            ])
            .send()
            .with_context(|| format!("GitHub search for {q:?}"))?;
        let status = resp.status();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            bail!("GitHub search rate-limited (HTTP {status}); using cache if available");
        }
        let resp = resp.error_for_status()?;
        let text = resp.text()?;
        batches.push(parse_search_response(&text)?);
    }
    Ok(batches)
}

/// Get discovery results, refreshing from GitHub at most once a day.
///
/// * If a fresh cache exists and `force` is false → return it.
/// * Otherwise attempt a live refresh; on success, cache and return it.
/// * On network failure / rate-limit → return the existing cache marked `stale`
///   if there is one, else a minimal result built from the catalog's
///   `listed_only` entries only (never an error — discovery is best-effort).
pub fn discover(force: bool) -> Result<DiscoveryCache> {
    let cached = load_cache();
    if !force {
        if let Some(c) = &cached {
            if is_fresh(&c.refreshed_at) {
                return Ok(c.clone());
            }
        }
    }

    let catalog = Catalog::embedded();
    match fetch_all() {
        Ok(batches) => {
            let cache = DiscoveryCache {
                refreshed_at: chrono::Utc::now().to_rfc3339(),
                repos: merge_and_filter(batches, &catalog),
                stale: false,
            };
            let _ = save_cache(&cache);
            Ok(cache)
        }
        Err(_) => {
            // Graceful degradation: serve stale cache, or a catalog-only view.
            if let Some(mut c) = cached {
                c.stale = true;
                Ok(c)
            } else {
                Ok(DiscoveryCache {
                    refreshed_at: chrono::Utc::now().to_rfc3339(),
                    repos: merge_and_filter(Vec::new(), &catalog),
                    stale: true,
                })
            }
        }
    }
}
