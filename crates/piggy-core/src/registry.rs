//! The saver registry: a declarative catalog of what each saver is and the
//! ordered steps to install / uninstall / health-check it.
//!
//! The catalog is **data, not code** (`registry/catalog.json`), embedded at
//! build time via [`include_str!`]. The engine ([`crate::engine`]) interprets an
//! entry's steps; this module only parses and validates the catalog and answers
//! "is this saver installable by this build?".
//!
//! Steps are kept as raw JSON objects rather than a closed enum so the catalog
//! can carry step-specific fields the engine reads ad hoc, and so an *unknown*
//! step kind produces a clean, actionable error ("catalog newer than app")
//! instead of a serde failure. Unknown top-level fields are ignored — the
//! catalog is expected to grow.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

const EMBEDDED: &str = include_str!("../../../registry/catalog.json");

/// Step kinds the v1 engine knows how to execute. Anything else in a catalog
/// entry's `install`/`uninstall` list means the catalog is newer than this
/// binary, and installing that saver is refused (never guessed).
pub const KNOWN_STEP_KINDS: &[&str] = &[
    "download_release_asset",
    "extract_binary",
    "merge_hooks",
    "claude_cli",
    "require_binary",
    "run_plugin_script",
    "verify_no_setting",
    "remove_hooks",
    "delete_file",
    "builtin_enable",
    "builtin_disable",
    "ensure_dir_on_path",
    "remove_dir_from_path",
    "require_python",
    "create_venv",
    "pip_install",
    "write_launcher",
    "delete_dir",
];

/// The whole parsed catalog.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    #[serde(default)]
    pub registry_version: u32,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub min_app_version: String,
    pub entries: Vec<Entry>,
}

/// One saver.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub plain_label: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub layer: String,
    #[serde(default)]
    pub install_type: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub default_on: bool,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub source: Source,
    #[serde(default)]
    pub install: StepSet,
    #[serde(default)]
    pub uninstall: StepSet,
    #[serde(default)]
    pub health_check: HealthCheck,
    #[serde(default)]
    pub conflicts_with: Vec<String>,
    #[serde(default)]
    pub ordering: i64,
    #[serde(default)]
    pub behavior_changing: bool,
    #[serde(default)]
    pub claimed_savings: Option<String>,
    #[serde(default)]
    pub risk: Option<String>,
    #[serde(default)]
    pub warning: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub license_note: Option<String>,
    #[serde(default)]
    pub exclusion_reason: Option<String>,
    /// User-tunable options this saver exposes in the app (empty for most).
    #[serde(default)]
    pub config_options: Vec<ConfigOption>,
}

/// One user-tunable option a saver exposes (e.g. Caveman's intensity level).
/// `apply` declares how the engine writes the chosen value; like install
/// steps, an unknown apply kind refuses the action rather than guessing.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    /// Stable option key (unique within the saver).
    pub key: String,
    /// Short UI label, e.g. "Intensity".
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// The allowed values, in display order.
    pub choices: Vec<ConfigChoice>,
    /// The value in effect when the user never chose one.
    pub default: String,
    /// How to apply a chosen value (raw object with a `kind` discriminator,
    /// interpreted by [`crate::saver_config`]).
    pub apply: Value,
}

/// One selectable value of a [`ConfigOption`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigChoice {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// Where a saver's artifacts come from. Polymorphic on `type`; only the fields
/// the engine uses are named, the rest are permissive.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Source {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub pinned_version: Option<String>,
    /// arch-key (`darwin-aarch64` / `darwin-x86_64` / …) → asset filename.
    #[serde(default)]
    pub assets: BTreeMap<String, String>,
    #[serde(default)]
    pub checksum_file: Option<String>,
    #[serde(default)]
    pub marketplace: Option<String>,
    #[serde(default)]
    pub plugin: Option<String>,
    #[serde(default)]
    pub package: Option<String>,
}

/// An ordered list of steps (each an opaque JSON object with a `step` key).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StepSet {
    #[serde(default)]
    pub steps: Vec<Value>,
}

impl StepSet {
    /// The `step` kind of each entry, in order (missing `step` → `""`).
    pub fn kinds(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|s| step_kind(s).to_string())
            .collect()
    }
}

/// Health-check declaration.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HealthCheck {
    #[serde(default)]
    pub checks: Vec<Value>,
}

/// The `step`/`check` discriminator of a raw step object (`""` if absent).
pub fn step_kind(v: &Value) -> &str {
    v.get("step").and_then(Value::as_str).unwrap_or("")
}

/// The `check` discriminator of a raw health-check object (`""` if absent).
pub fn check_kind(v: &Value) -> &str {
    v.get("check").and_then(Value::as_str).unwrap_or("")
}

impl Catalog {
    /// Parse the embedded catalog. Panics only if the embedded JSON is malformed
    /// — a build-time invariant covered by [`crate::registry`] tests.
    pub fn embedded() -> Self {
        serde_json::from_str(EMBEDDED).expect("embedded catalog.json must be valid")
    }

    /// Parse a catalog from a JSON string (used by the refresh-from-GitHub path,
    /// currently a stub, and by tests).
    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        Ok(serde_json::from_str(s)?)
    }

    /// Look up an entry by id.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Entries sorted by their chain `ordering` (then id), the order Piggy shows
    /// and installs them in.
    pub fn ordered(&self) -> Vec<&Entry> {
        let mut v: Vec<&Entry> = self.entries.iter().collect();
        v.sort_by(|a, b| a.ordering.cmp(&b.ordering).then_with(|| a.id.cmp(&b.id)));
        v
    }
}

impl Entry {
    /// Whether this build knows every step kind in the entry's install and
    /// uninstall lists. Deferred/listed-only entries (whose steps use
    /// placeholder kinds like `todo_v1_1`) return `false` here.
    pub fn installable(&self) -> Result<(), String> {
        for kind in self
            .install
            .kinds()
            .iter()
            .chain(self.uninstall.kinds().iter())
        {
            if kind.is_empty() {
                return Err("catalog step is missing its `step` kind".to_string());
            }
            if !KNOWN_STEP_KINDS.contains(&kind.as_str()) {
                return Err(format!(
                    "unknown step kind `{kind}` — this catalog is newer than Piggy; update the app"
                ));
            }
        }
        Ok(())
    }

    /// True when the saver has real install steps (some listed-only entries have
    /// an empty step list and are display-only).
    pub fn has_install_steps(&self) -> bool {
        !self.install.steps.is_empty()
    }

    /// The launch command a wrapper-model saver installs (the `name` of its
    /// `write_launcher` install step, e.g. Headroom's `piggy-claude`). `None`
    /// for savers that apply to every session without a special launcher.
    pub fn launch_command(&self) -> Option<String> {
        self.install
            .steps
            .iter()
            .filter(|s| step_kind(s) == "write_launcher")
            .find_map(|s| s.get("name").and_then(Value::as_str))
            .map(String::from)
    }

    /// Each `require_binary` install step as `(binary, reason)`. A *soft* require
    /// installs anyway, so if the binary later goes missing the saver runs but
    /// degrades or silently no-ops (Token Optimizer's Python hooks, RTK's Node
    /// lifecycle hooks). Lets the UI say which binary and why, in the author's
    /// own words.
    pub fn required_binaries(&self) -> Vec<(&str, &str)> {
        self.install
            .steps
            .iter()
            .filter(|s| step_kind(s) == "require_binary")
            .filter_map(|s| {
                let bin = s.get("binary").and_then(Value::as_str)?;
                let reason = s
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("required by this saver");
                Some((bin, reason))
            })
            .collect()
    }
}
