//! Property definitions and the indexed attribute mirror
//! (`docs/DESIGN.md` §6.3).
//!
//! Values live as JSON in each row's `attributes` for round-trip fidelity and
//! are mirrored into `signal_property` so a query such as "every signal with
//! `symbol_rate_hz` above 1 M" is an index lookup, not a scan.

use rusqlite::{params, Connection, Row};
use serde_json::Value;
use sp_core::{Attributes, PropKind, PropScope, PropertyDef, PropertyValue, SignalId};

use crate::error::{Result, StoreError};

/// A predicate over one property key.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyQuery {
    /// Numeric value within `[min, max]`; either bound may be open.
    Between { min: Option<f64>, max: Option<f64> },
    /// Text value equal to the string.
    Equals(String),
    /// The key is present with any value.
    Exists,
}

// ---------------------------------------------------------------------------
// Definitions
// ---------------------------------------------------------------------------

/// Inserts a definition. `(scope, key)` must be unique.
pub fn insert_property_def(conn: &Connection, def: &PropertyDef) -> Result<i64> {
    validate_key(&def.key)?;
    conn.execute(
        "INSERT INTO property_def
             (key, scope, label, kind_json, unit, default_json, required, section, ordinal)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            def.key,
            def.scope.as_str(),
            def.label,
            serde_json::to_string(&def.kind)?,
            def.unit,
            def.default
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            i64::from(def.required),
            def.section,
            def.ordinal,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Replaces every editable field of an existing definition, matched on
/// `(scope, key)`.
pub fn update_property_def(conn: &Connection, def: &PropertyDef) -> Result<()> {
    let changed = conn.execute(
        "UPDATE property_def
         SET label = ?3, kind_json = ?4, unit = ?5, default_json = ?6, required = ?7,
             section = ?8, ordinal = ?9
         WHERE scope = ?2 AND key = ?1",
        params![
            def.key,
            def.scope.as_str(),
            def.label,
            serde_json::to_string(&def.kind)?,
            def.unit,
            def.default
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            i64::from(def.required),
            def.section,
            def.ordinal,
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::invalid(format!(
            "no {} property '{}' to update",
            def.scope, def.key
        )));
    }
    Ok(())
}

/// Removes a definition. Values already stored under the key stay in
/// `attributes` and become "unrecognised" (§6.5).
pub fn delete_property_def(conn: &Connection, scope: PropScope, key: &str) -> Result<bool> {
    let changed = conn.execute(
        "DELETE FROM property_def WHERE scope = ?1 AND key = ?2",
        params![scope.as_str(), key],
    )?;
    Ok(changed > 0)
}

const DEF_COLUMNS: &str =
    "key, scope, label, kind_json, unit, default_json, required, section, ordinal";

fn def_from_row(row: &Row<'_>) -> Result<PropertyDef> {
    let default: Option<String> = row.get(5)?;
    Ok(PropertyDef {
        key: row.get(0)?,
        scope: row.get::<_, String>(1)?.parse()?,
        label: row.get(2)?,
        kind: serde_json::from_str::<PropKind>(&row.get::<_, String>(3)?)?,
        unit: row.get(4)?,
        default: default
            .map(|s| serde_json::from_str::<PropertyValue>(&s))
            .transpose()?,
        required: row.get::<_, i64>(6)? != 0,
        section: row.get(7)?,
        ordinal: row.get(8)?,
    })
}

/// Definitions for one scope, or every scope, in editor order.
pub fn list_property_defs(conn: &Connection, scope: Option<PropScope>) -> Result<Vec<PropertyDef>> {
    match scope {
        Some(scope) => {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {DEF_COLUMNS} FROM property_def WHERE scope = ?1
                 ORDER BY section, ordinal, key"
            ))?;
            let rows = stmt.query_and_then([scope.as_str()], def_from_row)?;
            rows.collect()
        }
        None => {
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {DEF_COLUMNS} FROM property_def ORDER BY scope, section, ordinal, key"
            ))?;
            let rows = stmt.query_and_then([], def_from_row)?;
            rows.collect()
        }
    }
}

pub fn get_property_def(
    conn: &Connection,
    scope: PropScope,
    key: &str,
) -> Result<Option<PropertyDef>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {DEF_COLUMNS} FROM property_def WHERE scope = ?1 AND key = ?2"
    ))?;
    let mut rows = stmt.query_and_then(params![scope.as_str(), key], def_from_row)?;
    rows.next().transpose()
}

fn validate_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !key.starts_with(|c: char| c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(StoreError::invalid(format!(
            "property key '{key}' must be snake_case ASCII"
        )))
    }
}

// ---------------------------------------------------------------------------
// Mirror
// ---------------------------------------------------------------------------

/// Rewrites a signal's rows in `signal_property` from its attributes.
pub fn mirror_signal_attributes(
    conn: &Connection,
    id: SignalId,
    attributes: &Attributes,
) -> Result<()> {
    conn.execute(
        "DELETE FROM signal_property WHERE signal_id = ?1",
        [id.get()],
    )?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO signal_property (signal_id, key, num_value, txt_value) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for (key, value) in attributes.iter() {
        let (num, txt) = mirror_columns(value);
        if num.is_none() && txt.is_none() {
            continue;
        }
        stmt.execute(params![id.get(), key, num, txt])?;
    }
    Ok(())
}

/// How one JSON value lands in the index: numbers and booleans in
/// `num_value`, strings in `txt_value`, structures as their JSON text.
fn mirror_columns(value: &Value) -> (Option<f64>, Option<String>) {
    match value {
        Value::Null => (None, None),
        Value::Bool(b) => (Some(f64::from(u8::from(*b))), None),
        Value::Number(n) => (n.as_f64(), None),
        Value::String(s) => (None, Some(s.clone())),
        other => (None, Some(other.to_string())),
    }
}

/// Signals whose property `key` satisfies `query`, by id.
pub fn find_signals(conn: &Connection, key: &str, query: &PropertyQuery) -> Result<Vec<SignalId>> {
    let ids: Vec<i64> = match query {
        PropertyQuery::Between { min, max } => {
            let mut stmt = conn.prepare_cached(
                "SELECT signal_id FROM signal_property
                 WHERE key = ?1 AND num_value IS NOT NULL
                   AND (?2 IS NULL OR num_value >= ?2)
                   AND (?3 IS NULL OR num_value <= ?3)
                 ORDER BY signal_id",
            )?;
            let rows = stmt.query_map(params![key, min, max], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        }
        PropertyQuery::Equals(text) => {
            let mut stmt = conn.prepare_cached(
                "SELECT signal_id FROM signal_property
                 WHERE key = ?1 AND txt_value = ?2 ORDER BY signal_id",
            )?;
            let rows = stmt.query_map(params![key, text], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        }
        PropertyQuery::Exists => {
            let mut stmt = conn.prepare_cached(
                "SELECT signal_id FROM signal_property WHERE key = ?1 ORDER BY signal_id",
            )?;
            let rows = stmt.query_map([key], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        }
    };
    Ok(ids.into_iter().map(SignalId::new).collect())
}

/// Distinct keys present in the index, with how many signals carry each.
pub fn attribute_key_counts(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let mut stmt =
        conn.prepare_cached("SELECT key, COUNT(*) FROM signal_property GROUP BY key ORDER BY key")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    Ok(rows
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(k, n)| (k, n.max(0) as u64))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn json_values_land_in_the_right_column() {
        assert_eq!(mirror_columns(&json!(1.5)), (Some(1.5), None));
        assert_eq!(mirror_columns(&json!(true)), (Some(1.0), None));
        assert_eq!(mirror_columns(&json!("nrz")), (None, Some("nrz".into())));
        assert_eq!(mirror_columns(&json!(null)), (None, None));
        assert_eq!(mirror_columns(&json!([1, 2])), (None, Some("[1,2]".into())));
    }

    #[test]
    fn keys_must_be_snake_case() {
        assert!(validate_key("prf_hz").is_ok());
        assert!(validate_key("PRF").is_err());
        assert!(validate_key("1st").is_err());
        assert!(validate_key("").is_err());
        assert!(validate_key("pulse width").is_err());
    }
}
