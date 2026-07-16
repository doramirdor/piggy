//! Saver discovery (M3): surface candidate token-savers from GitHub search.
//!
//! Piggy searches GitHub (unauthenticated, best-effort) for the
//! `token-optimization` / `claude-code` topics, merges and dedups the hits,
//! drops anything already curated in the catalog, and caches the result at
//! `~/.piggy/discovered.json`. The cache refreshes at most once a day
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

/// Piggy's own repo. It carries the `claude-code` and `token-optimization`
/// topics (so people looking for a token saver can find it), which means every
/// query below matches it. Piggy is not in its own catalog, so nothing else
/// filters it out, and without this it would list itself as a candidate saver.
const SELF_REPO: &str = "doramirdor/piggy";

/// The GitHub topic searches we run. Results are merged.
///
/// Every query is anchored to a topic. An unanchored free-text search (the old
/// `claude code token saver in:name,description,readme`) matched any repo whose
/// README merely mentioned those words, and since the merge sorts by stars the
/// junk led the feed. Keep new queries topic-scoped.
const QUERIES: &[&str] = &[
    "topic:claude-code topic:token-optimization",
    "topic:claude-code token optimizer",
    "topic:token-optimization",
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

/// Whether a discovered repo is the home of a curated package.
///
/// Catalog entries sourced from pip/npm often carry no `repo`, only a package
/// name (`headroom-ai`, `@ooples/token-optimizer-mcp`), so matching on `repo`
/// alone lets the package's own repo come back as a "discovery". Compare the
/// repo's name segment against the package name, ignoring case, any npm scope,
/// and a trailing `-ai` / `-cli` (so `headroom-ai` matches
/// `headroomlabs-ai/headroom`).
fn repo_is_package(full_name: &str, package: &str) -> bool {
    let repo_name = full_name.rsplit('/').next().unwrap_or(full_name);
    let pkg = package.rsplit('/').next().unwrap_or(package);
    let stem = pkg
        .strip_suffix("-ai")
        .or_else(|| pkg.strip_suffix("-cli"))
        .unwrap_or(pkg);
    repo_name.eq_ignore_ascii_case(pkg) || repo_name.eq_ignore_ascii_case(stem)
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

    // Every repo the catalog already knows about, curated or not. Curated ones
    // are installable, so re-suggesting them is just wrong. `listed_only` ones
    // are deliberately never installable and get appended below with their
    // exclusion reason, so letting the search surface them too would show the
    // same tool twice: once as a neutral candidate, once as "Piggy won't install
    // this, because…". Filter both here; the append is the only path that should
    // put a listed_only entry in the results.
    let known: std::collections::HashSet<String> = catalog
        .entries
        .iter()
        .filter_map(|e| e.source.repo.clone())
        .collect();

    // …and the packages they install, for the entries that name no repo.
    let known_packages: Vec<String> = catalog
        .entries
        .iter()
        .filter_map(|e| e.source.package.clone())
        .collect();

    let mut by_name: BTreeMap<String, DiscoveredRepo> = BTreeMap::new();
    for batch in batches {
        for repo in batch {
            if repo.full_name.eq_ignore_ascii_case(SELF_REPO)
                || known.contains(&repo.full_name)
                || known_packages
                    .iter()
                    .any(|p| repo_is_package(&repo.full_name, p))
            {
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

// ---------------------------------------------------------------------------
// Tests (fixture JSON only, never the network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A catalog fixture mirroring the real shapes that bite: entries sourced
    /// from pip/npm with no `repo` at all, one entry with a `repo`, and a
    /// `listed_only` entry (which must still be surfaced, not filtered).
    const CATALOG_JSON: &str = r#"{
      "registryVersion": 1,
      "entries": [
        { "id": "rtk", "name": "rtk", "status": "curated_v1",
          "source": { "type": "github_release", "repo": "rtk-ai/rtk" } },
        { "id": "headroom", "name": "Headroom", "status": "curated_v1",
          "source": { "type": "pip", "package": "headroom-ai" } },
        { "id": "cto", "name": "Claude Token Optimizer", "status": "curated_v1_1",
          "source": { "type": "npm", "package": "claude-token-optimizer" } },
        { "id": "token-optimizer-mcp", "name": "token-optimizer-mcp",
          "status": "listed_only", "description": "listed for transparency",
          "source": { "type": "npm", "package": "@ooples/token-optimizer-mcp" },
          "exclusionReason": "no clean uninstall" }
      ]
    }"#;

    fn repo(full_name: &str, stars: u64) -> DiscoveredRepo {
        DiscoveredRepo {
            full_name: full_name.to_string(),
            description: Some("a saver".into()),
            stars,
            url: format!("https://github.com/{full_name}"),
            topics: vec!["claude-code".into()],
            listed_only: false,
            exclusion_reason: None,
        }
    }

    fn feed_names(merged: &[DiscoveredRepo]) -> Vec<&str> {
        merged
            .iter()
            .filter(|r| !r.listed_only)
            .map(|r| r.full_name.as_str())
            .collect()
    }

    /// Every search query must stay anchored to a topic. A bare free-text query
    /// floods the feed with any high-star repo whose README says "token".
    #[test]
    fn every_query_is_topic_anchored() {
        for q in QUERIES {
            assert!(
                q.contains("topic:"),
                "query {q:?} is not topic-anchored; it will poison the feed"
            );
        }
    }

    /// Piggy tags its own repo `claude-code` + `token-optimization` so users can
    /// find it, which makes every query match it. It is not in its own catalog,
    /// so nothing else would filter it out of its own feed.
    #[test]
    fn piggy_does_not_discover_itself() {
        let catalog = Catalog::from_json(CATALOG_JSON).unwrap();
        let batch = vec![
            repo("doramirdor/piggy", 0),
            repo("DorAmirDor/Piggy", 3), // same repo, GitHub is case-insensitive here
            repo("stranger/lean-ctx", 120),
        ];
        assert_eq!(
            feed_names(&merge_and_filter(vec![batch], &catalog)),
            vec!["stranger/lean-ctx"],
            "Piggy must not list itself as a candidate saver"
        );
    }

    /// A `listed_only` tool must appear exactly once, in the appended
    /// transparency section with its exclusion reason. The live search finds
    /// `ooples/token-optimizer-mcp` (★438) on its own, so without filtering it
    /// out of the feed the same tool showed up twice: once as a neutral
    /// candidate, and once as "Piggy won't install this, because…".
    #[test]
    fn a_listed_only_tool_is_surfaced_once_not_also_in_the_feed() {
        let catalog = Catalog::from_json(CATALOG_JSON).unwrap();
        let batch = vec![
            repo("ooples/token-optimizer-mcp", 438),
            repo("stranger/lean-ctx", 120),
        ];
        let merged = merge_and_filter(vec![batch], &catalog);

        assert_eq!(
            feed_names(&merged),
            vec!["stranger/lean-ctx"],
            "the listed_only tool must not ride in on the search feed"
        );
        let listed: Vec<_> = merged.iter().filter(|r| r.listed_only).collect();
        assert_eq!(listed.len(), 1, "surfaced exactly once");
        assert_eq!(
            listed[0].exclusion_reason.as_deref(),
            Some("no clean uninstall"),
            "and it keeps the reason that is the whole point of listing it"
        );
    }

    /// The bug: `headroom` is curated (`defaultOn`) but sourced from pip with no
    /// `repo`, so its own repo came back as a fresh "discovery" at ★59,496.
    #[test]
    fn package_sourced_catalog_entries_filter_their_repo() {
        let catalog = Catalog::from_json(CATALOG_JSON).unwrap();
        let batch = vec![
            repo("headroomlabs-ai/headroom", 59_496),
            repo("ooples/claude-token-optimizer", 800),
            repo("rtk-ai/rtk", 4_200),
            repo("stranger/lean-ctx", 120),
        ];

        let merged = merge_and_filter(vec![batch], &catalog);

        assert_eq!(
            feed_names(&merged),
            vec!["stranger/lean-ctx"],
            "curated savers must never come back as discoveries, whether they \
             are sourced by repo (rtk) or by package (headroom, cto)"
        );
        // The listed_only entry is still surfaced, with its reason.
        let listed = merged
            .iter()
            .find(|r| r.listed_only)
            .expect("listed_only entry surfaced");
        assert_eq!(listed.full_name, "token-optimizer-mcp");
        assert_eq!(listed.exclusion_reason.as_deref(), Some("no clean uninstall"));
    }

    /// The name-segment match is case-insensitive and ignores an npm scope, but
    /// must not swallow unrelated repos that merely share a word.
    #[test]
    fn package_match_is_narrow() {
        assert!(repo_is_package("headroomlabs-ai/headroom", "headroom-ai"));
        assert!(repo_is_package("Someone/HeadRoom", "headroom-ai"));
        assert!(repo_is_package(
            "ooples/token-optimizer-mcp",
            "@ooples/token-optimizer-mcp"
        ));
        assert!(repo_is_package("x/nadirclaw", "nadirclaw"));

        // Different tools, not the curated package.
        assert!(!repo_is_package("other/headroom-monitor", "headroom-ai"));
        assert!(!repo_is_package("other/claude-tokens", "claude-token-optimizer"));
        // The owner segment never counts on its own.
        assert!(!repo_is_package("headroom/something-else", "headroom-ai"));
    }

    #[test]
    fn parse_and_dedup_keeps_the_highest_star_count() {
        let json = r#"{"items":[
          {"full_name":"a/one","description":"d","stargazers_count":10,
           "html_url":"https://github.com/a/one","topics":["claude-code"]},
          {"full_name":"a/two","stargazers_count":5,"html_url":"https://github.com/a/two"}
        ]}"#;
        let parsed = parse_search_response(json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].stars, 10);
        assert_eq!(parsed[1].description, None);

        let catalog = Catalog::from_json(CATALOG_JSON).unwrap();
        let merged = merge_and_filter(vec![parsed, vec![repo("a/two", 999)]], &catalog);
        // Sorted by stars desc: a/two (999, deduped up) before a/one (10).
        assert_eq!(feed_names(&merged), vec!["a/two", "a/one"]);
    }
}
