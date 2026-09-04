//! Plugin management for the DSH profile the app hosts.
//!
//! The app is a window shell around `dsh web`. DSH plugins are enabled /
//! disabled by editing the profile's `cordis.patch.yml` (the profile patch
//! layer): an id-targeted top-level block
//!
//! ```yaml
//! - id: some-plugin
//!   disabled: true
//! ```
//!
//! disables that plugin entry. There is no `dsh plugin disable` CLI command
//! (the `dsh plugin` subcommand just forwards to pnpm), so the management
//! panel writes this file directly.
//!
//! This module:
//!   * lists every third-party plugin entry (id + owning bundle + enabled
//!     state) by scanning the profile's `package.json` bundles and each
//!     bundle's `dsh.bundle.patch` file for `- insert:` entry ids,
//!   * toggles a plugin by appending / removing its `disabled: true` block
//!     in `cordis.patch.yml`, backing the file up first.
//!
//! The two built-in core bundles (`@deepseek-ai/dsh-base`,
//! `@deepseek-ai/dsh-web-app`) are skipped — they hold ~86 infrastructure
//! entries each and are not user plugins.

use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// One toggleable plugin entry, shown in the management window.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginEntry {
    /// Loader entry id (e.g. `better-sidebar`), used as the `- id:` key in
    /// `cordis.patch.yml`.
    pub id: String,
    /// The npm bundle that provides this entry (e.g. `dsh-better-sidebar`).
    pub bundle: String,
    /// `true` when the entry is currently active (no `disabled: true`
    /// override exists for it in `cordis.patch.yml`).
    pub enabled: bool,
}

/// Built-in core bundles shipped with the dsh CLI. They are not user
/// plugins; skipping them keeps the panel focused on third-party entries.
const CORE_BUNDLES: &[&str] = &["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"];

/// Resolve the web profile directory.
///
/// Priority: `$DSH_HOME` if set, else `<home>/.dsh`, always with the
/// `profiles/web` suffix. The home dir comes from `USERPROFILE` (Windows)
/// or `HOME` (POSIX).
fn profile_dir() -> Result<PathBuf, String> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| "无法定位用户主目录（USERPROFILE / HOME 均未设置）".to_string())?;
    let dsh_home = std::env::var("DSH_HOME").unwrap_or_else(|_| {
        let mut p = PathBuf::from(&home);
        p.push(".dsh");
        p.to_string_lossy().into_owned()
    });
    let mut dir = PathBuf::from(dsh_home);
    dir.push("profiles");
    dir.push("web");
    Ok(dir)
}

/// The `cordis.patch.yml` path of the web profile.
fn patch_path(profile: &Path) -> PathBuf {
    profile.join("cordis.patch.yml")
}

/// Parse `- insert:` blocks out of a bundle patch file and collect the
/// entry ids they mount. The patch format is:
///
/// ```yaml
/// - insert:
///     - id: better-sidebar
///       name: 'dsh-better-sidebar'
/// ```
///
/// Only `- id:` lines nested *below* an `- insert:` line are collected.
/// `!!js` expressions and unrelated top-level `- id:` config overrides are
/// ignored.
fn parse_patch_entry_ids(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut in_insert = false;
    let mut insert_indent = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("- insert:") {
            in_insert = true;
            insert_indent = line.len() - line.trim_start().len();
            continue;
        }
        if !in_insert {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent <= insert_indent {
            // Left the insert block (next top-level item or comment).
            in_insert = false;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- id:") {
            let id = rest.trim().trim_matches('\'').trim_matches('"').to_string();
            if !id.is_empty() {
                ids.push(id);
            }
        }
    }
    ids
}

/// Parse `cordis.patch.yml` top-level id-targeted blocks and return the set
/// of ids whose block contains `disabled: true`. Only exact
/// `disabled: true` counts; `!!js` expressions (e.g. platform guards) are
/// not treated as disabled.
fn parse_disabled_ids(text: &str) -> HashSet<String> {
    let mut disabled = HashSet::new();
    let mut cur_id: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            // Top-level item: a `- id:` starts a targetable block.
            if let Some(rest) = trimmed.strip_prefix("- id:") {
                cur_id = Some(
                    rest.trim().trim_matches('\'').trim_matches('"').to_string(),
                );
            } else {
                cur_id = None; // e.g. `- insert:` or a comment
            }
            continue;
        }
        if indent > 0 {
            if let Some(id) = &cur_id {
                if trimmed == "disabled: true" {
                    disabled.insert(id.clone());
                }
            }
        }
    }
    disabled
}

/// Remove the top-level `- id: <target>` block (and its indented children)
/// from `text`. Used to re-enable a plugin. Leaves everything else intact.
fn remove_disable_block(text: &str, target: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        let indent = line.len() - line.trim_start().len();
        let is_target = indent == 0
            && trimmed.starts_with("- id:")
            && trimmed["- id:".len()..]
                .trim()
                .trim_matches('\'')
                .trim_matches('"')
                == target;
        if is_target {
            // Skip this block: the line plus all following deeper lines.
            i += 1;
            while i < lines.len() {
                let l = lines[i];
                let li = l.len() - l.trim_start().len();
                if l.trim().is_empty() {
                    i += 1;
                    continue;
                }
                if li == 0 {
                    break;
                }
                i += 1;
            }
            continue;
        }
        out.push(line);
        i += 1;
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') && !joined.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Read the list of bundle package names from `dsh.profile.bundles`.
fn read_bundles(profile: &Path) -> Result<Vec<String>, String> {
    let pkg = fs::read_to_string(profile.join("package.json"))
        .map_err(|e| format!("读取 {} 失败: {e}", profile.join("package.json").display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&pkg).map_err(|e| format!("解析 package.json 失败: {e}"))?;
    let bundles = json["dsh"]["profile"]["bundles"]
        .as_array()
        .ok_or("package.json 缺少 dsh.profile.bundles 数组")?;
    Ok(bundles
        .iter()
        .filter_map(|b| b.as_str().map(String::from))
        .collect())
}

/// Resolve the entry ids mounted by a single bundle, by reading its
/// `dsh.bundle.patch` file inside `node_modules/<bundle>`.
fn bundle_entry_ids(profile: &Path, bundle: &str) -> Vec<String> {
    let pkg_path = profile.join("node_modules").join(bundle).join("package.json");
    let Ok(pkg) = fs::read_to_string(&pkg_path) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&pkg) else {
        return Vec::new();
    };
    let Some(patch_rel) = json["dsh"]["bundle"]["patch"].as_str() else {
        return Vec::new();
    };
    let patch_path = pkg_path.parent().unwrap_or(profile).join(patch_rel);
    let Ok(patch_text) = fs::read_to_string(&patch_path) else {
        return Vec::new();
    };
    parse_patch_entry_ids(&patch_text)
}

/// Write `content` to `path`, keeping a rolling backup of the previous
/// contents at `<path>.bak-plugin-panel` first (so a bad toggle is always
/// undoable by the user).
fn write_with_backup(path: &Path, content: &str) -> Result<(), String> {
    if path.exists() {
        let bak = PathBuf::from(format!("{}.bak-plugin-panel", path.display()));
        fs::copy(path, &bak).map_err(|e| format!("备份 {} 失败: {e}", bak.display()))?;
    }
    fs::write(path, content).map_err(|e| format!("写入 {} 失败: {e}", path.display()))
}

/// List all third-party plugin entries with their current enabled state.
fn list_plugins_in_dir(profile: &Path) -> Result<Vec<PluginEntry>, String> {
    let bundles = read_bundles(profile)?;
    let disabled = {
        let path = patch_path(profile);
        let text = fs::read_to_string(&path).unwrap_or_default();
        parse_disabled_ids(&text)
    };
    let mut entries = Vec::new();
    for bundle in bundles {
        if CORE_BUNDLES.contains(&bundle.as_str()) {
            continue;
        }
        for id in bundle_entry_ids(profile, &bundle) {
            entries.push(PluginEntry {
                enabled: !disabled.contains(&id),
                id,
                bundle: bundle.clone(),
            });
        }
    }
    // Deterministic order for the UI: bundle, then id.
    entries.sort_by(|a, b| a.bundle.cmp(&b.bundle).then(a.id.cmp(&b.id)));
    Ok(entries)
}

/// Toggle one plugin entry in `cordis.patch.yml`.
///
/// * disable → append a top-level `- id: <id>` / `disabled: true` block
///   (idempotent: no-op when already disabled),
/// * enable → remove that block if present (idempotent otherwise).
///
/// The change only takes effect after dsh is restarted (the boot flow
/// re-composes the bundle + patch layers); the caller decides when that
/// happens (the panel offers a manual "restart DSH" button).
fn set_plugin_in_dir(profile: &Path, id: &str, enabled: bool) -> Result<(), String> {
    if id.trim().is_empty() {
        return Err("插件 id 不能为空".to_string());
    }
    let path = patch_path(profile);
    let text = fs::read_to_string(&path).map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
    let disabled = parse_disabled_ids(&text);

    if enabled {
        if !disabled.contains(id) {
            return Ok(()); // already enabled
        }
        let new_text = remove_disable_block(&text, id);
        write_with_backup(&path, &new_text)
    } else {
        if disabled.contains(id) {
            return Ok(()); // already disabled
        }
        let mut new_text = text;
        if !new_text.ends_with('\n') {
            new_text.push('\n');
        }
        new_text.push_str(&format!("\n- id: {}\n  disabled: true\n", id));
        write_with_backup(&path, &new_text)
    }
}

// ── public API (used by the Tauri commands) ─────────────────────────────

/// List plugins for the web profile. See [`list_plugins_in_dir`].
pub fn list_plugins() -> Result<Vec<PluginEntry>, String> {
    list_plugins_in_dir(&profile_dir()?)
}

/// Toggle a plugin in the web profile. See [`set_plugin_in_dir`].
pub fn set_plugin(id: &str, enabled: bool) -> Result<(), String> {
    set_plugin_in_dir(&profile_dir()?, id, enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_insert_entry_ids() {
        let yaml = r#"
- insert:
    - id: better-sidebar
      name: 'dsh-better-sidebar'
    - id: dsh-at-file
      name: dsh-at-file
- id: some-config-block
  config:
    x: 1
"#;
        assert_eq!(parse_patch_entry_ids(yaml), vec!["better-sidebar", "dsh-at-file"]);
    }

    #[test]
    fn insert_ids_with_quotes_and_scopes() {
        let yaml = r#"
- insert:
    - id: "quoted-id"
    - id: scoped/thing
"#;
        assert_eq!(parse_patch_entry_ids(yaml), vec!["quoted-id", "scoped/thing"]);
    }

    #[test]
    fn ignores_js_disabled_expr_for_enabled_state() {
        // A `!!js` platform guard must NOT be read as "disabled".
        let yaml = "- id: gitbash-executor\n  disabled: !!js process.platform !== 'win32'\n";
        assert!(parse_disabled_ids(yaml).is_empty());
    }

    #[test]
    fn detects_disabled_true() {
        let yaml = "- id: better-sidebar\n  disabled: true\n- id: other\n  config: {}\n";
        let d = parse_disabled_ids(yaml);
        assert!(d.contains("better-sidebar"));
        assert!(!d.contains("other"));
    }

    #[test]
    fn disabled_detection_scoped_to_own_block() {
        // A `disabled: true` inside a later block must not leak into an
        // earlier one (nested children handled by top-level id tracking).
        let yaml = "- id: a\n  x: 1\n- id: b\n  disabled: true\n";
        let d = parse_disabled_ids(yaml);
        assert!(!d.contains("a"));
        assert!(d.contains("b"));
    }

    #[test]
    fn removes_disable_block_and_keeps_rest() {
        let yaml = "# comment\n- id: keep\n  config: {}\n- id: dropme\n  disabled: true\n  extra: 1\n- id: after\n";
        let out = remove_disable_block(yaml, "dropme");
        assert!(!out.contains("dropme"));
        assert!(out.contains("- id: keep"));
        assert!(out.contains("- id: after"));
        assert!(out.contains("# comment"));
    }

    #[test]
    fn remove_absent_block_is_noop() {
        let yaml = "- id: a\n  disabled: true\n";
        assert_eq!(remove_disable_block(yaml, "missing"), yaml);
    }

    #[test]
    fn set_plugin_toggle_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dsh-plugin-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cordis.patch.yml");
        fs::write(&path, "- id: x\n  enabled: true\n").unwrap();

        // disable → block appended
        set_plugin_in_dir(&dir, "better-sidebar", false).unwrap();
        let t = fs::read_to_string(&path).unwrap();
        assert!(t.contains("- id: better-sidebar\n  disabled: true"));
        assert!(parse_disabled_ids(&t).contains("better-sidebar"));

        // disable again → idempotent, no duplicate block
        set_plugin_in_dir(&dir, "better-sidebar", false).unwrap();
        let t2 = fs::read_to_string(&path).unwrap();
        assert_eq!(t, t2);

        // enable → block removed, original content intact
        set_plugin_in_dir(&dir, "better-sidebar", true).unwrap();
        let t3 = fs::read_to_string(&path).unwrap();
        assert!(!parse_disabled_ids(&t3).contains("better-sidebar"));
        assert!(t3.contains("- id: x"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_empty_id() {
        let dir = std::env::temp_dir().join(format!("dsh-plugin-test-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("cordis.patch.yml"), "[]").unwrap();
        assert!(set_plugin_in_dir(&dir, "  ", true).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
