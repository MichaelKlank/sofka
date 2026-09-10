//! Server column definitions and cells, kept separate from resource objects.

use std::collections::HashMap;

use anyhow::{Context, Result, ensure};
use k8s_openapi::jiff::Timestamp;
use kube::core::{DynamicObject, ObjectMeta};
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub priority: i32,
}

impl Column {
    pub fn numeric(&self) -> bool {
        matches!(self.kind.as_str(), "number" | "integer")
    }

    pub fn sort_value(&self, value: Option<&Value>) -> crate::views::SortValue {
        use crate::views::SortValue;
        if self.numeric() {
            SortValue::Num(value.and_then(Value::as_f64).unwrap_or(f64::MAX))
        } else if self.kind == "date" || (self.kind == "string" && self.format == "date-time") {
            SortValue::Num(
                value
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<Timestamp>().ok())
                    .map(|t| t.as_second() as f64)
                    .unwrap_or(f64::MAX),
            )
        } else {
            SortValue::Text(render(value).to_lowercase())
        }
    }
}

pub fn render(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "<none>".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    uid: Option<String>,
    resource_version: Option<String>,
    pub cells: Vec<Value>,
}

impl Row {
    fn matches(&self, obj: &DynamicObject) -> bool {
        self.uid == obj.metadata.uid && self.resource_version == obj.metadata.resource_version
    }
}

#[derive(Debug)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
    pub resource_version: String,
    pub continue_token: Option<String>,
}

impl Table {
    /// Later watch events can omit definitions. Their cells use the last schema.
    pub fn decode(value: Value, previous: &[Column]) -> Result<Self> {
        ensure!(
            value["kind"] == "Table" && value["apiVersion"] == "meta.k8s.io/v1",
            "expected a meta.k8s.io/v1 Table"
        );
        let columns: Vec<Column> = match value.get("columnDefinitions") {
            None | Some(Value::Null) => previous.to_vec(),
            Some(Value::Array(columns)) if columns.is_empty() => previous.to_vec(),
            Some(columns) => serde_json::from_value(columns.clone())
                .context("invalid Table column definitions")?,
        };
        let mut names = std::collections::HashSet::new();
        for col in &columns {
            ensure!(
                !col.name.trim().is_empty() && names.insert(col.name.to_uppercase()),
                "Table column names must be nonempty and unique"
            );
        }
        let raw_rows = match value.get("rows") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(rows)) => rows.as_slice(),
            _ => anyhow::bail!("invalid Table rows"),
        };
        let mut rows = Vec::with_capacity(raw_rows.len());
        let mut keys = std::collections::HashSet::new();
        for row in raw_rows {
            let cells = row["cells"].as_array().context("invalid Table cells")?;
            ensure!(
                cells.len() == columns.len(),
                "Table cell count does not match its columns"
            );
            let metadata: ObjectMeta = serde_json::from_value(row["object"]["metadata"].clone())
                .context("Table row has no valid object metadata")?;
            let name = metadata
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .context("Table row has no object name")?;
            let key = match metadata.namespace.as_deref() {
                Some(ns) => format!("{ns}/{name}"),
                None => name.to_string(),
            };
            ensure!(
                keys.insert(key.clone()),
                "Table contains duplicate resource rows"
            );
            rows.push(Row {
                key,
                uid: metadata.uid,
                resource_version: metadata.resource_version,
                cells: cells.clone(),
            });
        }
        Ok(Self {
            columns,
            rows,
            resource_version: value["metadata"]["resourceVersion"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            continue_token: value["metadata"]["continue"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        })
    }
}

#[derive(Debug)]
pub enum Update {
    Replace(Table),
    Apply(Table),
    Delete(Table),
    Unavailable,
}

#[derive(Default)]
pub struct State {
    pub columns: Vec<Column>,
    rows: HashMap<String, Row>,
}

impl State {
    pub fn cells(&self, obj: &DynamicObject) -> Option<&[Value]> {
        if self.rows.is_empty() {
            return None;
        }
        self.rows
            .get(&crate::store::row_key(obj))
            .filter(|row| row.matches(obj))
            .map(|row| row.cells.as_slice())
    }

    /// Return whether the layout changed and which rows need a new render.
    pub fn apply(&mut self, update: Update) -> (bool, Vec<String>) {
        let (table, replace, delete) = match update {
            Update::Replace(table) => (table, true, false),
            Update::Apply(table) => (table, false, false),
            Update::Delete(table) => (table, false, true),
            Update::Unavailable => {
                let changed = !self.columns.is_empty();
                self.columns.clear();
                return (changed, self.clear_cells());
            }
        };
        let layout_changed = self.columns != table.columns;
        let mut changed = Vec::new();
        if layout_changed {
            changed.extend(self.rows.keys().cloned());
            self.rows.clear();
            self.columns = table.columns;
        }
        if replace {
            let fresh: HashMap<_, _> = table.rows.into_iter().map(|r| (r.key.clone(), r)).collect();
            changed.extend(
                self.rows
                    .keys()
                    .filter(|k| !fresh.contains_key(*k))
                    .cloned(),
            );
            changed.extend(
                fresh
                    .iter()
                    .filter(|(k, r)| self.rows.get(*k) != Some(*r))
                    .map(|(k, _)| k.clone()),
            );
            self.rows = fresh;
        } else {
            for row in table.rows {
                if delete {
                    if self
                        .rows
                        .get(&row.key)
                        .is_some_and(|old| old.uid == row.uid)
                    {
                        self.rows.remove(&row.key);
                        changed.push(row.key);
                    }
                } else if self.rows.get(&row.key) != Some(&row) {
                    changed.push(row.key.clone());
                    self.rows.insert(row.key.clone(), row);
                }
            }
        }
        (layout_changed, changed)
    }

    pub fn clear_cells(&mut self) -> Vec<String> {
        self.rows.drain().map(|(key, _)| key).collect()
    }

    pub fn remove(&mut self, key: &str, uid: Option<&str>) {
        if self
            .rows
            .get(key)
            .is_some_and(|row| row.uid.as_deref() == uid)
        {
            self.rows.remove(key);
        }
    }
}
