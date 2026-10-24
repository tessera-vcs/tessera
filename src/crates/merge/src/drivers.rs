//! Specialized 3-way structural merge drivers for TOML, JSON, `Cargo.lock`, and sequential migrations.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Outcome of a specialized manifest or lockfile merge driver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverMergeOutcome {
    /// Merged file content.
    pub content: String,
    /// `true` if conflicting leaf values could not be resolved by the driver.
    pub has_conflicts: bool,
    /// List of conflicting key paths (e.g., `dependencies.serde`).
    pub conflicting_keys: Vec<String>,
}

/// Attempts to merge `base`, `left`, and `right` using a specialized manifest/lockfile driver based on `path`.
///
/// Returns `Some(DriverMergeOutcome)` if `path` is handled by a specialized driver
/// (`Cargo.lock`, `*.toml`, `*.json`), or `None` if it should proceed to the Tree-sitter AST merger.
pub fn try_merge_with_driver(
    path: &str,
    base: &str,
    left: &str,
    right: &str,
) -> Option<DriverMergeOutcome> {
    let file_name = path.rsplit('/').next().unwrap_or(path);

    if file_name == "Cargo.lock" {
        return merge_cargo_lock_3way(base, left, right);
    }

    if std::path::Path::new(file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
    {
        return merge_toml_3way(base, left, right);
    }

    if std::path::Path::new(file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        return merge_json_3way(base, left, right);
    }

    None
}

/// Performs a structural 3-way merge on TOML files (`Cargo.toml`, `.git/tessera/config.toml`, etc.).
pub fn merge_toml_3way(base: &str, left: &str, right: &str) -> Option<DriverMergeOutcome> {
    let base_val: toml::Value = if base.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(base).ok()?
    };
    let left_val: toml::Value = toml::from_str(left).ok()?;
    let right_val: toml::Value = toml::from_str(right).ok()?;

    let mut conflicts = Vec::new();
    let merged_val = merge_toml_value("", &base_val, &left_val, &right_val, &mut conflicts);

    if !conflicts.is_empty() {
        return None;
    }

    let serialized = toml::to_string_pretty(&merged_val).ok()?;

    Some(DriverMergeOutcome {
        content: serialized,
        has_conflicts: false,
        conflicting_keys: Vec::new(),
    })
}

/// Recursively merges three [`toml::Value`] trees (`base`, `left`, `right`), recording any
/// conflicting dotted key paths into `conflicts`.
fn merge_toml_value(
    key_path: &str,
    base: &toml::Value,
    left: &toml::Value,
    right: &toml::Value,
    conflicts: &mut Vec<String>,
) -> toml::Value {
    if left == right || right == base {
        return left.clone();
    }

    if left == base {
        return right.clone();
    }

    match (base, left, right) {
        (_, toml::Value::Table(l_map), toml::Value::Table(r_map)) => {
            let empty_map = toml::map::Map::new();
            let b_map = match base {
                toml::Value::Table(m) => m,
                _ => &empty_map,
            };

            let mut all_keys = BTreeSet::new();
            for k in b_map.keys().chain(l_map.keys()).chain(r_map.keys()) {
                all_keys.insert(k.clone());
            }

            let mut out = toml::map::Map::new();

            for k in all_keys {
                let child_path = if key_path.is_empty() {
                    k.clone()
                } else {
                    format!("{key_path}.{k}")
                };

                match (b_map.get(&k), l_map.get(&k), r_map.get(&k)) {
                    (Some(b), Some(l), Some(r)) => {
                        out.insert(k, merge_toml_value(&child_path, b, l, r, conflicts));
                    }
                    (None, Some(l), Some(r)) => {
                        let empty_tbl = toml::Value::Table(toml::map::Map::new());
                        out.insert(
                            k,
                            merge_toml_value(&child_path, &empty_tbl, l, r, conflicts),
                        );
                    }
                    (None, Some(l), None) => {
                        out.insert(k, l.clone());
                    }
                    (None, None, Some(r)) => {
                        out.insert(k, r.clone());
                    }
                    (Some(b), Some(l), None) => {
                        if l != b {
                            conflicts.push(child_path);
                            out.insert(k, l.clone());
                        }
                    }
                    (Some(b), None, Some(r)) => {
                        if r != b {
                            conflicts.push(child_path);
                            out.insert(k, r.clone());
                        }
                    }
                    (Some(_), None, None) | (None, None, None) => {}
                }
            }

            toml::Value::Table(out)
        }
        (_, toml::Value::Array(l_arr), toml::Value::Array(r_arr)) => {
            let empty_arr = Vec::new();
            let b_arr = match base {
                toml::Value::Array(a) => a,
                _ => &empty_arr,
            };

            let removed_on_l: Vec<&toml::Value> =
                b_arr.iter().filter(|x| !l_arr.contains(x)).collect();
            let removed_on_r: Vec<&toml::Value> =
                b_arr.iter().filter(|x| !r_arr.contains(x)).collect();

            let mut merged = Vec::new();

            for item in l_arr.iter().chain(r_arr.iter()) {
                if removed_on_l.contains(&item) || removed_on_r.contains(&item) {
                    continue;
                }

                if !merged.contains(item) {
                    merged.push(item.clone());
                }
            }

            toml::Value::Array(merged)
        }
        _ => {
            conflicts.push(key_path.to_string());
            left.clone()
        }
    }
}

/// Performs a structural 3-way merge on JSON files (`package.json`, `tsconfig.json`, etc.).
pub fn merge_json_3way(base: &str, left: &str, right: &str) -> Option<DriverMergeOutcome> {
    let base_val: serde_json::Value = if base.trim().is_empty() {
        serde_json::Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(base).ok()?
    };
    let left_val: serde_json::Value = serde_json::from_str(left).ok()?;
    let right_val: serde_json::Value = serde_json::from_str(right).ok()?;

    let mut conflicts = Vec::new();
    let merged_val = merge_json_value("", &base_val, &left_val, &right_val, &mut conflicts);

    if !conflicts.is_empty() {
        return None;
    }

    let mut serialized = serde_json::to_string_pretty(&merged_val).ok()?;
    serialized.push('\n');

    Some(DriverMergeOutcome {
        content: serialized,
        has_conflicts: false,
        conflicting_keys: Vec::new(),
    })
}

/// Recursively merges three [`serde_json::Value`] trees (`base`, `left`, `right`), recording
/// any conflicting dotted key paths into `conflicts`.
fn merge_json_value(
    key_path: &str,
    base: &serde_json::Value,
    left: &serde_json::Value,
    right: &serde_json::Value,
    conflicts: &mut Vec<String>,
) -> serde_json::Value {
    if left == right || right == base {
        return left.clone();
    }

    if left == base {
        return right.clone();
    }

    match (base, left, right) {
        (_, serde_json::Value::Object(l_map), serde_json::Value::Object(r_map)) => {
            let empty_map = serde_json::Map::new();
            let b_map = match base {
                serde_json::Value::Object(m) => m,
                _ => &empty_map,
            };

            let mut all_keys = BTreeSet::new();
            for k in b_map.keys().chain(l_map.keys()).chain(r_map.keys()) {
                all_keys.insert(k.clone());
            }

            let mut out = serde_json::Map::new();

            for k in all_keys {
                let child_path = if key_path.is_empty() {
                    k.clone()
                } else {
                    format!("{key_path}.{k}")
                };

                match (b_map.get(&k), l_map.get(&k), r_map.get(&k)) {
                    (Some(b), Some(l), Some(r)) => {
                        out.insert(k, merge_json_value(&child_path, b, l, r, conflicts));
                    }
                    (None, Some(l), Some(r)) => {
                        let empty_obj = serde_json::Value::Object(serde_json::Map::new());
                        out.insert(
                            k,
                            merge_json_value(&child_path, &empty_obj, l, r, conflicts),
                        );
                    }
                    (None, Some(l), None) => {
                        out.insert(k, l.clone());
                    }
                    (None, None, Some(r)) => {
                        out.insert(k, r.clone());
                    }
                    (Some(b), Some(l), None) => {
                        if l != b {
                            conflicts.push(child_path);
                            out.insert(k, l.clone());
                        }
                    }
                    (Some(b), None, Some(r)) => {
                        if r != b {
                            conflicts.push(child_path);
                            out.insert(k, r.clone());
                        }
                    }
                    (Some(_), None, None) | (None, None, None) => {}
                }
            }

            serde_json::Value::Object(out)
        }
        (_, serde_json::Value::Array(l_arr), serde_json::Value::Array(r_arr)) => {
            let empty_arr = Vec::new();
            let b_arr = match base {
                serde_json::Value::Array(a) => a,
                _ => &empty_arr,
            };
            let removed_on_l: Vec<&serde_json::Value> =
                b_arr.iter().filter(|x| !l_arr.contains(x)).collect();
            let removed_on_r: Vec<&serde_json::Value> =
                b_arr.iter().filter(|x| !r_arr.contains(x)).collect();

            let mut merged = Vec::new();

            for item in l_arr.iter().chain(r_arr.iter()) {
                if removed_on_l.contains(&item) || removed_on_r.contains(&item) {
                    continue;
                }

                if !merged.contains(item) {
                    merged.push(item.clone());
                }
            }

            serde_json::Value::Array(merged)
        }
        _ => {
            conflicts.push(key_path.to_string());
            left.clone()
        }
    }
}

/// Performs a structural 3-way merge on `Cargo.lock` files by indexing `[[package]]` stanzas
/// by `(name, version)` and unioning added dependencies.
pub fn merge_cargo_lock_3way(base: &str, left: &str, right: &str) -> Option<DriverMergeOutcome> {
    let base_val: toml::Value = if base.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(base).ok()?
    };
    let left_val: toml::Value = toml::from_str(left).ok()?;
    let right_val: toml::Value = toml::from_str(right).ok()?;

    let base_pkgs = extract_lock_packages(&base_val);
    let left_pkgs = extract_lock_packages(&left_val);
    let right_pkgs = extract_lock_packages(&right_val);

    let mut all_keys = BTreeSet::new();
    for k in base_pkgs
        .keys()
        .chain(left_pkgs.keys())
        .chain(right_pkgs.keys())
    {
        all_keys.insert(k.clone());
    }

    let mut conflicts = Vec::new();
    let mut merged_pkgs = Vec::new();

    for key in all_keys {
        match (
            base_pkgs.get(&key),
            left_pkgs.get(&key),
            right_pkgs.get(&key),
        ) {
            (Some(b), Some(l), Some(r)) => {
                merged_pkgs.push(merge_toml_value(
                    &format!("package.{}.{}", key.0, key.1),
                    b,
                    l,
                    r,
                    &mut conflicts,
                ));
            }
            (None, Some(l), Some(r)) => {
                let empty_tbl = toml::Value::Table(toml::map::Map::new());
                merged_pkgs.push(merge_toml_value(
                    &format!("package.{}.{}", key.0, key.1),
                    &empty_tbl,
                    l,
                    r,
                    &mut conflicts,
                ));
            }
            (None, Some(l), None) => merged_pkgs.push(l.clone()),
            (None, None, Some(r)) => merged_pkgs.push(r.clone()),
            (Some(b), Some(l), None) => {
                if l != b {
                    merged_pkgs.push(l.clone());
                }
            }
            (Some(b), None, Some(r)) => {
                if r != b {
                    merged_pkgs.push(r.clone());
                }
            }
            (Some(_), None, None) | (None, None, None) => {}
        }
    }

    if !conflicts.is_empty() {
        return None;
    }

    let mut out_table = match left_val {
        toml::Value::Table(t) => t,
        _ => toml::map::Map::new(),
    };
    out_table.insert("package".to_string(), toml::Value::Array(merged_pkgs));
    let serialized = toml::to_string_pretty(&toml::Value::Table(out_table)).ok()?;

    Some(DriverMergeOutcome {
        content: serialized,
        has_conflicts: false,
        conflicting_keys: Vec::new(),
    })
}

/// Extracts `[[package]]` entries from a parsed `Cargo.lock` TOML document keyed by `(name, version)`.
fn extract_lock_packages(val: &toml::Value) -> BTreeMap<(String, String), toml::Value> {
    let mut map = BTreeMap::new();

    if let Some(pkgs) = val.get("package").and_then(|v| v.as_array()) {
        for pkg in pkgs {
            let name = pkg
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let version = pkg
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if !name.is_empty() {
                map.insert((name, version), pkg.clone());
            }
        }
    }

    map
}

/// Record of a sequential migration file automatically renumbered to resolve a prefix collision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationResequence {
    /// Original path created by the second agent (e.g., `migrations/0042_add_billing.sql`).
    pub original_path: String,
    /// Re-sequenced path in the merged tree (e.g., `migrations/0043_add_billing.sql`).
    pub resequenced_path: String,
}

/// Checks whether `dir_path` is a sequential migration directory (`migrations`, `migrate`, `db/migrations`).
pub fn is_migration_dir(dir_path: &str) -> bool {
    dir_path
        .split('/')
        .any(|seg| seg == "migrations" || seg == "migrate")
}

/// Splits a filename like `"0042_create_users.sql"` into `Some((42, 4, "_create_users.sql"))`.
pub fn parse_numeric_prefix(file_name: &str) -> Option<(u64, usize, &str)> {
    let digit_len = file_name.bytes().take_while(u8::is_ascii_digit).count();

    if digit_len == 0 {
        return None;
    }

    let rest = &file_name[digit_len..];

    if !rest.starts_with('_') && !rest.starts_with('-') {
        return None;
    }

    let num = file_name[..digit_len].parse::<u64>().ok()?;
    Some((num, digit_len, rest))
}

/// Formats a re-sequenced migration filename preserving zero-padding width (e.g., `43`, `width=4`, `"_billing.sql"` -> `"0043_billing.sql"`).
pub fn format_resequenced_filename(next_num: u64, width: usize, suffix: &str) -> String {
    format!("{next_num:0width$}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_toml_concurrent_dependencies_merge_cleanly() {
        let base = r#"[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = "1.0"
"#;
        let left = r#"[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = "1.0"
tokio = "1.40"
"#;
        let right = r#"[package]
name = "demo"
version = "0.1.0"

[dependencies]
serde = "1.0"
axum = "0.7"
"#;

        let res = merge_toml_3way(base, left, right).expect("toml merge");
        assert!(!res.has_conflicts);
        assert!(res.content.contains("tokio = \"1.40\""));
        assert!(res.content.contains("axum = \"0.7\""));
    }

    #[test]
    fn package_json_concurrent_deps_merge_cleanly() {
        let base = "{\n  \"name\": \"web\",\n  \"dependencies\": {\n    \"react\": \"^19.0.0\"\n  }\n}\n";
        let left = "{\n  \"name\": \"web\",\n  \"dependencies\": {\n    \"react\": \"^19.0.0\",\n    \"zod\": \"^3.24.0\"\n  }\n}\n";
        let right = "{\n  \"name\": \"web\",\n  \"dependencies\": {\n    \"react\": \"^19.0.0\",\n    \"lucide-react\": \"^0.460.0\"\n  }\n}\n";

        let res = merge_json_3way(base, left, right).expect("json merge");
        assert!(!res.has_conflicts);
        assert!(res.content.contains("\"zod\""));
        assert!(res.content.contains("\"lucide-react\""));
    }

    #[test]
    fn cargo_lock_concurrent_packages_merge_cleanly() {
        let base = r#"version = 3

[[package]]
name = "serde"
version = "1.0.217"
"#;
        let left = r#"version = 3

[[package]]
name = "serde"
version = "1.0.217"

[[package]]
name = "blake3"
version = "1.5.5"
"#;
        let right = r#"version = 3

[[package]]
name = "serde"
version = "1.0.217"

[[package]]
name = "rayon"
version = "1.10.0"
"#;

        let res = merge_cargo_lock_3way(base, left, right).expect("cargo.lock merge");
        assert!(!res.has_conflicts);
        assert!(res.content.contains("name = \"blake3\""));
        assert!(res.content.contains("name = \"rayon\""));
    }
}
