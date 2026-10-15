//! Editing the risk limits in `turbojet-hub.toml`, as the API does: one table, `[risk.defaults]` or
//! `[risk.client.COMPID]`, changed with `toml_edit`, so the comments and layout elsewhere in the
//! file stay as they were. [`Service::edit_limits`](crate::service::Service::edit_limits) checks,
//! writes and reloads the result.

use std::fmt;

use toml_edit::{DocumentMut, Item, Table};

use crate::config::{RiskTable, RiskTables};

/// The keys a risk table takes, as `config` reads them.
const AMOUNTS: [&str; 4] = ["max_order_qty", "max_notional", "price_band", "credit"];

/// A change to the limits.
#[derive(Debug, Clone)]
pub enum Edit {
    /// Each key set to its value, or removed for `None`, in `table`, which is created if absent.
    Set { table: Limits, values: Vec<(&'static str, Option<toml_edit::Value>)> },
    /// The client's table removed: its sessions get the defaults.
    Delete { comp_id: String },
}

/// Which table an edit is to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Limits {
    Defaults,
    Client(String),
}

/// Why an edit wasn't made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    /// The gateway is stopping.
    Stopping,
    /// The client has no table to delete.
    NoTable(String),
    /// The file, edited, doesn't load, or wouldn't edit: why.
    Refused(String),
    /// The table edited is wrong on its own (a key it doesn't take, a value the loader refuses):
    /// why, naming the key. Unlike [`Refused`](Self::Refused), it says nothing of other tables.
    Invalid(String),
    /// Reading or writing the file failed.
    Io(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopping => f.write_str("the gateway is stopping"),
            Self::NoTable(comp_id) => write!(f, "no [risk.client.{comp_id}]"),
            Self::Refused(why) | Self::Invalid(why) | Self::Io(why) => f.write_str(why),
        }
    }
}

/// For the log: the table, and the keys set or removed.
impl fmt::Display for Edit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Set { table, values } => {
                let (set, removed): (Vec<_>, Vec<_>) = values.iter().partition(|(_, value)| value.is_some());
                let names = |keys: Vec<&(&str, _)>| keys.iter().map(|(key, _)| *key).collect::<Vec<_>>().join(", ");
                write!(f, "{table}: set [{}], removed [{}]", names(set), names(removed))
            }
            Self::Delete { comp_id } => write!(f, "{}: deleted", Limits::Client(comp_id.clone())),
        }
    }
}

impl fmt::Display for Limits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Defaults => f.write_str("risk.defaults"),
            Self::Client(comp_id) => write!(f, "risk.client.{comp_id}"),
        }
    }
}

impl Edit {
    /// The values of a request's `set`, `{key: value | null}`: a JSON number or string for an
    /// amount, an array of strings for `symbols`, a boolean for `self_trade`. Whether a value is
    /// allowed (positive, say) is the loader's to say once the file is edited.
    ///
    /// # Errors
    ///
    /// `set` is empty, or names a key a risk table doesn't take or gives one a value of the wrong
    /// type.
    pub fn set(table: Limits, set: &serde_json::Map<String, serde_json::Value>) -> Result<Self, String> {
        if set.is_empty() {
            return Err("set names no key".to_string());
        }
        let values = set.iter().map(|(key, value)| value_for(key, value)).collect::<Result<_, _>>()?;
        Ok(Self::Set { table, values })
    }

    /// The table this edit is to.
    pub fn limits(&self) -> Limits {
        match self {
            Self::Set { table, .. } => table.clone(),
            Self::Delete { comp_id } => Limits::Client(comp_id.clone()),
        }
    }

    /// The table this edit leaves in `risk`, the file's as edited; `None` if it has none.
    pub fn table_in(&self, risk: &RiskTables) -> Option<RiskTable> {
        match self {
            Self::Set { table: Limits::Defaults, .. } => Some(risk.defaults.clone()),
            Self::Set { table: Limits::Client(comp_id), .. } | Self::Delete { comp_id } => {
                risk.client.get(comp_id).cloned()
            }
        }
    }
}

/// A CompID as a client table names it: printable ASCII, with no `.` or quote, which would make the
/// TOML key one an operator editing the file by hand could misread.
///
/// # Errors
///
/// Empty, or a character outside those.
pub fn check_comp_id(comp_id: &str) -> Result<(), String> {
    let allowed = |c: char| c.is_ascii_graphic() && !matches!(c, '.' | '"' | '\'' | '\\');
    if comp_id.is_empty() || !comp_id.chars().all(allowed) {
        return Err(format!("CompID {comp_id:?}: printable ASCII, without '.', quotes or '\\'"));
    }
    Ok(())
}

/// `key`'s TOML value for `value`, `None` for `null`.
fn value_for(key: &str, value: &serde_json::Value) -> Result<(&'static str, Option<toml_edit::Value>), String> {
    use serde_json::Value as Json;
    let known = AMOUNTS.iter().chain(&["symbols", "self_trade"]).find(|known| **known == key);
    let Some(&key) = known else { return Err(format!("unknown key {key:?}")) };
    let toml = match (key, value) {
        (_, Json::Null) => None,
        ("symbols", Json::Array(symbols)) => {
            let symbols: Option<toml_edit::Array> = symbols.iter().map(Json::as_str).collect();
            Some(symbols.ok_or("symbols: expected an array of strings")?.into())
        }
        ("self_trade", Json::Bool(self_trade)) => Some((*self_trade).into()),
        // An integer past a TOML integer's range is written as a decimal string, every digit kept.
        (_, Json::Number(number)) if AMOUNTS.contains(&key) => Some(match number.as_i64() {
            Some(integer) => integer.into(),
            None if number.is_u64() => number.to_string().into(),
            None => number.as_f64().expect("without arbitrary_precision every JSON number is an f64").into(),
        }),
        (_, Json::String(text)) if AMOUNTS.contains(&key) => Some(text.as_str().into()),
        ("symbols", _) => return Err("symbols: expected an array of strings".to_string()),
        ("self_trade", _) => return Err("self_trade: expected true or false".to_string()),
        _ => return Err(format!("{key}: expected a number, or a decimal as a string")),
    };
    Ok((key, toml))
}

/// `text`, the configuration file, with `edit` made: only the table it's to changes.
///
/// # Errors
///
/// `text` isn't TOML, `[risk]` or the table is written in a form an edit can't change (an inline
/// table, say), or (`NoTable`) a table to delete isn't there.
pub fn apply(text: &str, edit: &Edit) -> Result<String, EditError> {
    let mut document: DocumentMut = text.parse().map_err(|e| EditError::Refused(format!("not TOML: {e}")))?;
    let risk = table(document.as_table_mut(), "risk")?;
    match edit {
        Edit::Set { table: limits, values } => {
            let edited = match limits {
                Limits::Defaults => table(risk, "defaults")?,
                Limits::Client(comp_id) => table(table(risk, "client")?, comp_id)?,
            };
            for (key, value) in values {
                set(edited, key, value.clone());
            }
        }
        Edit::Delete { comp_id } => {
            if table(risk, "client")?.remove(comp_id).is_none() {
                return Err(EditError::NoTable(comp_id.clone()));
            }
        }
    }
    Ok(document.to_string())
}

/// `parent`'s table `key`, created if absent: implicit, so it shows only once it has keys.
fn table<'a>(parent: &'a mut Table, key: &str) -> Result<&'a mut Table, EditError> {
    let item = parent.entry(key).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    item.as_table_mut().ok_or_else(|| EditError::Refused(format!("{key} isn't written as a [table]: edit it by hand")))
}

/// Sets `key` to `value` in `table`, keeping a comment after an old value; removes it for `None`.
fn set(table: &mut Table, key: &str, value: Option<toml_edit::Value>) {
    let Some(mut value) = value else {
        table.remove(key);
        return;
    };
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    }
    table.insert(key, Item::Value(value));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_edit(table: Limits, set: serde_json::Value) -> Result<Edit, String> {
        Edit::set(table, set.as_object().unwrap())
    }

    /// Each key takes its own type; an unknown key, a wrong type or nothing to set is refused.
    #[test]
    fn values_are_checked_for_their_keys_type() {
        let edit = set_edit(
            Limits::Defaults,
            serde_json::json!({
                "credit": 5, "max_notional": 2.5, "max_order_qty": "10.25", "price_band": null,
                "self_trade": true, "symbols": ["AAPL"],
            }),
        )
        .unwrap();
        let Edit::Set { values, .. } = edit else { panic!() };
        let keys: Vec<_> = values.iter().map(|(key, _)| *key).collect();
        assert_eq!(keys, ["credit", "max_notional", "max_order_qty", "price_band", "self_trade", "symbols"]);
        assert_eq!(values[0].1.as_ref().and_then(toml_edit::Value::as_integer), Some(5));
        assert_eq!(values[2].1.as_ref().and_then(toml_edit::Value::as_str), Some("10.25"));
        assert!(values[3].1.is_none());
        let edit = set_edit(Limits::Client("FUNDA".into()), serde_json::json!({ "credit": u64::MAX, "symbols": null }));
        let edit = edit.unwrap();
        let Edit::Set { values, .. } = &edit else { panic!() };
        assert_eq!(values[0].1.as_ref().and_then(toml_edit::Value::as_str), Some("18446744073709551615"));
        assert_eq!(edit.to_string(), "risk.client.FUNDA: set [credit], removed [symbols]");
        for (set, error) in [
            (serde_json::json!({}), "set names no key"),
            (serde_json::json!({ "max_qty": 5 }), "unknown key \"max_qty\""),
            (serde_json::json!({ "credit": true }), "credit: expected a number"),
            (serde_json::json!({ "symbols": "AAPL" }), "symbols: expected an array"),
            (serde_json::json!({ "symbols": [1] }), "symbols: expected an array"),
            (serde_json::json!({ "self_trade": "yes" }), "self_trade: expected true or false"),
        ] {
            let refused = set_edit(Limits::Defaults, set).unwrap_err();
            assert!(refused.starts_with(error), "{refused}");
        }
    }

    #[test]
    fn a_comp_id_is_plain() {
        assert!(check_comp_id("FUND-A_1").is_ok());
        for bad in ["", "FUND.A", "FUND\"A", "FUND A", "FUNDÄ"] {
            assert!(check_comp_id(bad).is_err(), "{bad}");
        }
    }

    /// Only the table edited changes: comments elsewhere, and one after a value changed, stay.
    #[test]
    fn an_edit_changes_only_its_table() {
        let text = "# the gateway\nsessions = \"s.toml\"  # here\n\n[risk.defaults]\ncredit = 5 # tight\n\
                    max_order_qty = 9\n\n# FUNDA's\n[risk.client.FUNDA]\ncredit = 1\n";
        let edit = set_edit(Limits::Defaults, serde_json::json!({ "credit": "7.5", "max_order_qty": null })).unwrap();
        let edited = apply(text, &edit).unwrap();
        assert_eq!(edited, text.replace("credit = 5 # tight\nmax_order_qty = 9\n", "credit = \"7.5\" # tight\n"));

        let edit = set_edit(Limits::Client("FUNDB".into()), serde_json::json!({ "symbols": ["AAPL"] })).unwrap();
        let edited = apply(text, &edit).unwrap();
        assert_eq!(edited, format!("{text}\n[risk.client.FUNDB]\nsymbols = [\"AAPL\"]\n"));

        let delete = Edit::Delete { comp_id: "FUNDA".into() };
        assert_eq!(apply(text, &delete).unwrap(), text.replace("\n# FUNDA's\n[risk.client.FUNDA]\ncredit = 1\n", ""));
        assert_eq!(apply(text, &Edit::Delete { comp_id: "FUNDB".into() }), Err(EditError::NoTable("FUNDB".into())));
    }

    /// A file with no `[risk]` gets the table; one that isn't TOML, or writes the table inline,
    /// is refused.
    #[test]
    fn tables_are_created_or_refused() {
        let edit = set_edit(Limits::Defaults, serde_json::json!({ "credit": 5 })).unwrap();
        assert_eq!(apply("sessions = \"s\"\n", &edit).unwrap(), "sessions = \"s\"\n\n[risk.defaults]\ncredit = 5\n");
        assert!(matches!(apply("sessions = ", &edit), Err(EditError::Refused(e)) if e.starts_with("not TOML")));
        let inline = "sessions = \"s\"\n[risk]\ndefaults = { credit = 1 }\n";
        assert!(matches!(apply(inline, &edit), Err(EditError::Refused(e)) if e.contains("edit it by hand")));
    }
}
