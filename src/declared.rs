//! Open capabilities: what a model declares a device can do (values, settings, actions), and
//! the rules the core applies to every declared capability, whatever kind of device it is.
//!
//! * **value**: something the device reports (`temperature`)
//! * **setting**: something it reports and you can change (`setpoint`)
//! * **action**: something it does on request, optionally with arguments (`standby`)
//! * **stream**: live data it delivers until stopped (`samples`), see [`crate::stream`]
//!
//! The core parses what you type with the unit table, checks the device range and your `limits`
//! before anything is sent, applies several settings in declaration order (switching off first),
//! reads back and verifies, and supports dry runs. Drivers only implement `read`, `set` and
//! `action`.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde::Serialize;

use crate::capture::{Output, Produced};
use crate::error::{DeskError, Result};
use crate::stream::StreamOutput;
use crate::units;

/// Read-back tolerance when a setting declares none (like `voltage_source`).
const TOLERANCE_REL: f64 = 0.005;
const TOLERANCE_ABS: f64 = 0.011;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Type {
    Number,
    Boolean,
    Choice,
    String,
}

/// One declared value, setting or action argument.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decl {
    #[serde(rename = "type")]
    pub kind: Type,
    /// The unit of a number (a unit-table symbol or one the table doesn't know).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// A value whose unit changes (a multimeter's reading): the possible units.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub units: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Changing it has a physical effect (heats, outputs, moves). Always false for values.
    pub switches: bool,
    /// Action arguments: the value used when the argument is left out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Val>,
    /// Action arguments: may be left out (the driver then doesn't get it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub optional: bool,
}

impl Decl {
    /// An action argument that must be given (no default, not optional).
    pub fn required(&self) -> bool {
        self.default.is_none() && !self.optional
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Action {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub switches: bool,
    /// What the action produces besides doing it: `file` (a capture).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub produces: Option<Produces>,
    /// Required unless they have a `default` or are `optional`.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub args: IndexMap<String, Decl>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Produces {
    /// A file the core owns (`ctx.output`).
    File,
}

impl Action {
    pub fn produces_file(&self) -> bool {
        self.produces == Some(Produces::File)
    }

    /// Whether any argument must be given.
    pub fn needs_args(&self) -> bool {
        self.args.values().any(Decl::required)
    }
}

/// A stream: live data the device delivers until it is stopped (`desk stream`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stream {
    /// What the payload is: `logic` (well-known, see [`crate::stream`]) or a declared name whose
    /// bytes are opaque to the core.
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    /// Required unless they have a `default` or are `optional`, like an action's.
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub args: IndexMap<String, Decl>,
}

impl Stream {
    /// Whether any argument must be given.
    pub fn needs_args(&self) -> bool {
        self.args.values().any(Decl::required)
    }
}

/// Everything a capability declares.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Declarations {
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub values: IndexMap<String, Decl>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub settings: IndexMap<String, Decl>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub actions: IndexMap<String, Action>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub streams: IndexMap<String, Stream>,
    /// Channel names (`channels = 2` gives `1`, `2`); `None` = no channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Value,
    Setting,
    Arg,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !n.starts_with(|c: char| c.is_ascii_digit())
}

impl Declarations {
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
            && self.settings.is_empty()
            && self.actions.is_empty()
            && self.streams.is_empty()
    }

    pub fn readable(&self) -> bool {
        !self.values.is_empty() || !self.settings.is_empty()
    }

    /// Take the declaration keys out of a capability table and check them.
    pub fn take(table: &mut toml::Table) -> std::result::Result<Declarations, String> {
        let mut d = Declarations::default();
        let section =
            |table: &mut toml::Table, key: &str| -> std::result::Result<toml::Table, String> {
                match table.remove(key) {
                    None => Ok(toml::Table::new()),
                    Some(toml::Value::Table(t)) => Ok(t),
                    Some(_) => Err(format!("`{key}` must be a table of names")),
                }
            };
        for (name, v) in section(table, "values")? {
            d.values.insert(name.clone(), decl(&name, &v, Role::Value)?);
        }
        for (name, v) in section(table, "settings")? {
            d.settings
                .insert(name.clone(), decl(&name, &v, Role::Setting)?);
        }
        for (name, v) in section(table, "actions")? {
            d.actions.insert(name.clone(), action(&name, &v)?);
        }
        for (name, v) in section(table, "streams")? {
            d.streams.insert(name.clone(), stream(&name, &v)?);
        }
        d.channels = match table.remove("channels") {
            None => None,
            Some(toml::Value::Integer(n)) if (1..=64).contains(&n) => {
                Some((1..=n).map(|i| i.to_string()).collect())
            }
            Some(toml::Value::Array(names)) => {
                let mut out: Vec<String> = Vec::new();
                for n in names {
                    match n.as_str() {
                        Some(s) if valid_channel(s) && !out.iter().any(|o| o == s) => {
                            out.push(s.to_string())
                        }
                        _ => {
                            return Err(
                                "`channels` must be distinct names like \"ch1\" (letters, digits, `-`, `_`)"
                                    .into(),
                            );
                        }
                    }
                }
                if out.is_empty() {
                    return Err("`channels` must not be empty".into());
                }
                Some(out)
            }
            Some(_) => {
                return Err("`channels` must be a count (1..64) or a list of names".into());
            }
        };
        // One namespace per capability.
        let mut seen: Vec<&str> = Vec::new();
        for n in d
            .values
            .keys()
            .chain(d.settings.keys())
            .chain(d.actions.keys())
            .chain(d.streams.keys())
        {
            if seen.contains(&n.as_str()) {
                return Err(format!(
                    "`{n}` is declared twice (names are unique per device)"
                ));
            }
            seen.push(n);
        }
        Ok(d)
    }

    /// Settings, values, actions: the order shown everywhere.
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.settings
            .keys()
            .chain(self.values.keys())
            .chain(self.actions.keys())
    }

    /// A value or setting by name (what `read` may report).
    pub fn readable_decl(&self, name: &str) -> Option<&Decl> {
        self.settings.get(name).or_else(|| self.values.get(name))
    }
}

pub fn valid_channel(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

const DECL_KEYS: &[&str] = &[
    "type",
    "unit",
    "units",
    "min",
    "max",
    "tolerance",
    "choices",
    "doc",
    "switches",
    "default",
    "optional",
];

fn decl(name: &str, v: &toml::Value, role: Role) -> std::result::Result<Decl, String> {
    let at = |m: &str| format!("`{name}`: {m}");
    if !valid_name(name) {
        return Err(at("names use lowercase letters, digits and `_`"));
    }
    let toml::Value::Table(t) = v else {
        return Err(at(
            "must be a table like { type = \"number\", unit = \"V\" }",
        ));
    };
    if let Some(k) = t.keys().find(|k| !DECL_KEYS.contains(&k.as_str())) {
        return Err(at(&format!(
            "unknown key `{k}` (allowed: {})",
            DECL_KEYS.join(", ")
        )));
    }
    let kind = match t.get("type").and_then(toml::Value::as_str) {
        Some("number") => Type::Number,
        Some("boolean") => Type::Boolean,
        Some("choice") => Type::Choice,
        Some("string") => Type::String,
        _ => {
            return Err(at(
                "needs `type = \"number\" | \"boolean\" | \"choice\" | \"string\"`",
            ));
        }
    };
    let string = |k: &str| -> std::result::Result<Option<String>, String> {
        match t.get(k) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(at(&format!("`{k}` must be a string"))),
        }
    };
    let number = |k: &str| -> std::result::Result<Option<f64>, String> {
        match t.get(k) {
            None => Ok(None),
            Some(toml::Value::Float(f)) if f.is_finite() => Ok(Some(*f)),
            Some(toml::Value::Integer(i)) => Ok(Some(*i as f64)),
            Some(_) => Err(at(&format!("`{k}` must be a number"))),
        }
    };
    let strings = |k: &str| -> std::result::Result<Vec<String>, String> {
        match t.get(k) {
            None => Ok(Vec::new()),
            Some(toml::Value::Array(a)) => a
                .iter()
                .map(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| at(&format!("`{k}` must be a list of strings")))
                })
                .collect(),
            Some(_) => Err(at(&format!("`{k}` must be a list of strings"))),
        }
    };
    let unit = string("unit")?;
    let units = strings("units")?;
    let choices = strings("choices")?;
    let (min, max, tolerance) = (number("min")?, number("max")?, number("tolerance")?);
    for u in unit.iter().chain(&units) {
        if let Some(symbol) = units::misspelled(u) {
            return Err(at(&format!("write the unit as \"{symbol}\", not \"{u}\"")));
        }
        if u.trim().is_empty() || u.chars().any(|c| c.is_ascii_digit()) {
            return Err(at(&format!("\"{u}\" is not a unit")));
        }
    }
    match kind {
        Type::Number => {
            if unit.is_some() && !units.is_empty() {
                return Err(at("give either `unit` or `units`, not both"));
            }
            if !units.is_empty() && role != Role::Value {
                return Err(at(
                    "only values may have changing `units`; a setting needs one fixed `unit`",
                ));
            }
            if let (Some(lo), Some(hi)) = (min, max)
                && lo > hi
            {
                return Err(at("`min` is above `max`"));
            }
            if tolerance.is_some_and(|t| t < 0.0) {
                return Err(at("`tolerance` must be >= 0"));
            }
            if !choices.is_empty() {
                return Err(at("`choices` belong to type \"choice\""));
            }
        }
        Type::Choice => {
            if choices.is_empty() {
                return Err(at("type \"choice\" needs `choices = [\"…\", …]`"));
            }
            if unit.is_some() || !units.is_empty() || min.is_some() || max.is_some() {
                return Err(at("a choice has no unit, min or max"));
            }
        }
        Type::Boolean | Type::String => {
            if unit.is_some()
                || !units.is_empty()
                || min.is_some()
                || max.is_some()
                || !choices.is_empty()
            {
                return Err(at(
                    "only numbers have units and ranges, only choices have choices",
                ));
            }
        }
    }
    if role != Role::Arg {
        for k in ["default", "optional"] {
            if t.contains_key(k) {
                return Err(at(&format!("only action arguments have `{k}`")));
            }
        }
    }
    let optional = match t.get("optional") {
        None => false,
        Some(toml::Value::Boolean(b)) => *b,
        Some(_) => return Err(at("`optional` must be true or false")),
    };
    let switches = match (role, t.get("switches")) {
        (Role::Value, Some(_)) => return Err(at("values can't switch anything (no `switches`)")),
        (Role::Arg, Some(_)) => {
            return Err(at("`switches` belongs to the action, not its arguments"));
        }
        (_, Some(toml::Value::Boolean(b))) => *b,
        (_, Some(_)) => return Err(at("`switches` must be true or false")),
        (Role::Setting, None) => true,
        (_, None) => false,
    };
    let mut d = Decl {
        kind,
        unit,
        units,
        min,
        max,
        tolerance,
        choices,
        doc: string("doc")?,
        switches,
        default: None,
        optional,
    };
    if let Some(v) = t.get("default") {
        if optional {
            return Err(at("give `default` or `optional = true`, not both"));
        }
        let val = default_value(&d, v).map_err(|m| at(&format!("`default`: {m}")))?;
        check_range(name, &d, &val, None).map_err(|e| at(&format!("`default`: {e}")))?;
        d.default = Some(val);
    }
    Ok(d)
}

/// An argument's `default`, written in TOML: a number in the declared unit, `true`/`false`, or a
/// string (one of the choices).
fn default_value(d: &Decl, v: &toml::Value) -> std::result::Result<Val, String> {
    match (d.kind, v) {
        (Type::Number, toml::Value::Float(f)) if f.is_finite() => Ok(Val::Number(*f)),
        (Type::Number, toml::Value::Integer(i)) => Ok(Val::Number(*i as f64)),
        (Type::Number, _) => Err(format!(
            "must be a number{}",
            d.unit
                .as_ref()
                .map(|u| format!(" in {u}"))
                .unwrap_or_default()
        )),
        (Type::Boolean, toml::Value::Boolean(b)) => Ok(Val::Bool(*b)),
        (Type::Boolean, _) => Err("must be true or false".into()),
        (Type::Choice, toml::Value::String(s)) if d.choices.contains(s) => Ok(Val::Text(s.clone())),
        (Type::Choice, _) => Err(format!("must be one of: {}", d.choices.join(", "))),
        (Type::String, toml::Value::String(s)) => Ok(Val::Text(s.clone())),
        (Type::String, _) => Err("must be a string".into()),
    }
}

fn action(name: &str, v: &toml::Value) -> std::result::Result<Action, String> {
    let at = |m: &str| format!("action `{name}`: {m}");
    if !valid_name(name) {
        return Err(at("names use lowercase letters, digits and `_`"));
    }
    let toml::Value::Table(t) = v else {
        return Err(at("must be a table like { switches = true, doc = \"…\" }"));
    };
    if let Some(k) = t
        .keys()
        .find(|k| !["switches", "doc", "args", "produces"].contains(&k.as_str()))
    {
        return Err(at(&format!(
            "unknown key `{k}` (allowed: switches, doc, args, produces)"
        )));
    }
    let produces = match t.get("produces") {
        None => None,
        Some(toml::Value::String(s)) if s == "file" => Some(Produces::File),
        Some(_) => return Err(at("`produces` can only be \"file\"")),
    };
    let switches = match t.get("switches") {
        Some(toml::Value::Boolean(b)) => *b,
        _ => {
            return Err(at(
                "needs `switches = true` (it has a physical effect) or `switches = false`",
            ));
        }
    };
    let doc = match t.get("doc") {
        None => None,
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(at("`doc` must be a string")),
    };
    let mut args = IndexMap::new();
    match t.get("args") {
        None => {}
        Some(toml::Value::Table(a)) => {
            for (n, v) in a {
                args.insert(n.clone(), decl(n, v, Role::Arg).map_err(|e| at(&e))?);
            }
        }
        Some(_) => return Err(at("`args` must be a table of names")),
    }
    Ok(Action {
        doc,
        switches,
        produces,
        args,
    })
}

/// A stream's `content`: lowercase letters, digits and `_`.
pub fn valid_content(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 32
        && c.chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn stream(name: &str, v: &toml::Value) -> std::result::Result<Stream, String> {
    let at = |m: &str| format!("stream `{name}`: {m}");
    if !valid_name(name) {
        return Err(at("names use lowercase letters, digits and `_`"));
    }
    let toml::Value::Table(t) = v else {
        return Err(at(
            "must be a table like { content = \"logic\", doc = \"…\" }",
        ));
    };
    if let Some(k) = t
        .keys()
        .find(|k| !["content", "doc", "args"].contains(&k.as_str()))
    {
        return Err(at(&format!(
            "unknown key `{k}` (allowed: content, doc, args)"
        )));
    }
    let content = match t.get("content") {
        Some(toml::Value::String(c)) if valid_content(c) => c.clone(),
        Some(toml::Value::String(c)) => {
            return Err(at(&format!(
                "`content` \"{c}\" must be lowercase letters, digits and `_` (e.g. \"logic\")"
            )));
        }
        _ => {
            return Err(at(
                "needs `content = \"logic\"` (or another name for what its bytes are)",
            ));
        }
    };
    let doc = match t.get("doc") {
        None => None,
        Some(toml::Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(at("`doc` must be a string")),
    };
    let mut args = IndexMap::new();
    match t.get("args") {
        None => {}
        Some(toml::Value::Table(a)) => {
            for (n, v) in a {
                args.insert(n.clone(), decl(n, v, Role::Arg).map_err(|e| at(&e))?);
            }
        }
        Some(_) => return Err(at("`args` must be a table of names")),
    }
    Ok(Stream { content, doc, args })
}

// ---------------------------------------------------------------------------------------------
// Values

/// A value as it travels: from what you typed (checked), from the driver (decoded), to JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Val {
    Number(f64),
    /// A number whose unit changes (declared with `units`).
    Measured {
        value: f64,
        unit: String,
    },
    Bool(bool),
    Text(String),
}

impl Val {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Val::Number(n) | Val::Measured { value: n, .. } => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// For humans, in the declared unit.
    pub fn display(&self, decl: Option<&Decl>) -> String {
        match self {
            Val::Number(n) => units::format(*n, decl.and_then(|d| d.unit.as_deref())),
            Val::Measured { value, unit } => units::format(*value, Some(unit)),
            Val::Bool(b) => if *b { "on" } else { "off" }.to_string(),
            Val::Text(s) => s.clone(),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Values in declaration order.
pub type Readings = IndexMap<String, Val>;

/// Parse what was typed for a setting or argument.
pub fn parse_input(name: &str, decl: &Decl, input: &str) -> Result<Val> {
    let usage = |m: String| DeskError::Usage(format!("{name}: {m}"));
    match decl.kind {
        Type::Number => units::parse(input, decl.unit.as_deref())
            .map(Val::Number)
            .map_err(usage),
        Type::Boolean => match input.to_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Ok(Val::Bool(true)),
            "off" | "false" | "no" | "0" => Ok(Val::Bool(false)),
            _ => Err(usage(format!("`{input}` is not on/off (or true/false)"))),
        },
        Type::Choice => decl
            .choices
            .iter()
            .find(|c| c.eq_ignore_ascii_case(input))
            .map(|c| Val::Text(c.clone()))
            .ok_or_else(|| {
                usage(format!(
                    "`{input}` is not one of: {}",
                    decl.choices.join(", ")
                ))
            }),
        Type::String => Ok(Val::Text(input.to_string())),
    }
}

/// Bench limits of one setting: a number is the maximum, or `{ min, max }`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Limit {
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// Shown the way it's written: a number for a maximum, else `{ min, max }`.
impl Serialize for Limit {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match (self.min, self.max) {
            (None, Some(max)) => s.serialize_f64(max),
            _ => {
                let mut m = s.serialize_map(None)?;
                if let Some(v) = self.min {
                    m.serialize_entry("min", &v)?;
                }
                if let Some(v) = self.max {
                    m.serialize_entry("max", &v)?;
                }
                m.end()
            }
        }
    }
}

impl Limit {
    pub fn parse(v: &toml::Value) -> std::result::Result<Limit, String> {
        let num = |v: &toml::Value| match v {
            toml::Value::Float(f) if f.is_finite() => Some(*f),
            toml::Value::Integer(i) => Some(*i as f64),
            _ => None,
        };
        if let Some(max) = num(v) {
            return Ok(Limit {
                min: None,
                max: Some(max),
            });
        }
        let toml::Value::Table(t) = v else {
            return Err("a limit is a number (the maximum) or { min = …, max = … }".into());
        };
        let mut l = Limit::default();
        for (k, v) in t {
            let n = num(v).ok_or_else(|| format!("`{k}` must be a number"))?;
            match k.as_str() {
                "min" => l.min = Some(n),
                "max" => l.max = Some(n),
                other => return Err(format!("unknown key `{other}` (allowed: min, max)")),
            }
        }
        Ok(l)
    }
}

/// Check a typed number against the device range and the bench limit, before anything is sent.
pub fn check_range(name: &str, decl: &Decl, val: &Val, limit: Option<Limit>) -> Result<()> {
    let Some(v) = val.as_f64() else {
        return Ok(());
    };
    if !v.is_finite() {
        return Err(DeskError::Usage(format!(
            "{name} must be a finite number, got {v}"
        )));
    }
    let unit = decl.unit.as_deref();
    let fail = |limit: f64, kind: &str, below: bool| DeskError::LimitExceeded {
        what: name.to_string(),
        requested: units::format(v, unit),
        limit: units::format(limit, unit),
        kind: kind.to_string(),
        below,
    };
    let limit = limit.unwrap_or_default();
    for (bound, kind) in [(decl.max, "device maximum"), (limit.max, "bench limit")] {
        if let Some(b) = bound
            && v > b + 1e-9
        {
            return Err(fail(b, kind, false));
        }
    }
    for (bound, kind) in [(decl.min, "device minimum"), (limit.min, "bench limit")] {
        if let Some(b) = bound
            && v < b - 1e-9
        {
            return Err(fail(b, kind, true));
        }
    }
    Ok(())
}

/// Check what a driver's `read` returned against the declarations. Unreported names are absent.
pub fn decode(decls: &Declarations, raw: serde_json::Value, driver: &str) -> Result<Readings> {
    let bad = |m: String| DeskError::ScriptFailed(format!("{driver}: read(): {m}"));
    let obj = match raw {
        serde_json::Value::Object(o) => o,
        // An empty Lua table arrives as an empty array.
        serde_json::Value::Array(a) if a.is_empty() => serde_json::Map::new(),
        other => return Err(bad(format!("must return a table, got {other}"))),
    };
    for k in obj.keys() {
        if decls.readable_decl(k).is_none() {
            let known: Vec<&String> = decls.values.keys().chain(decls.settings.keys()).collect();
            return Err(bad(format!(
                "`{k}` is not a declared value or setting (declared: {})",
                known
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut out = Readings::new();
    // Settings first, then values: the declaration order shown everywhere.
    for (name, decl) in decls.settings.iter().chain(&decls.values) {
        let Some(v) = obj.get(name) else { continue };
        if v.is_null() {
            continue;
        }
        let val = match (decl.kind, v) {
            (Type::Number, serde_json::Value::Number(n)) if decl.units.is_empty() => {
                Val::Number(n.as_f64().unwrap_or(f64::NAN))
            }
            (Type::Number, serde_json::Value::Object(o)) if !decl.units.is_empty() => {
                let value = o.get("value").and_then(serde_json::Value::as_f64);
                let unit = o.get("unit").and_then(serde_json::Value::as_str);
                match (value, unit) {
                    (Some(value), Some(unit)) if decl.units.iter().any(|u| u == unit) => {
                        Val::Measured {
                            value,
                            unit: unit.to_string(),
                        }
                    }
                    (Some(_), Some(unit)) => {
                        return Err(bad(format!(
                            "`{name}`: unit \"{unit}\" is not one of the declared {}",
                            decl.units.join(", ")
                        )));
                    }
                    _ => {
                        return Err(bad(format!(
                            "`{name}` must be {{ value = <number>, unit = \"{}\" }}",
                            decl.units[0]
                        )));
                    }
                }
            }
            (Type::Number, _) if !decl.units.is_empty() => {
                return Err(bad(format!(
                    "`{name}` must be {{ value = <number>, unit = \"{}\" }}",
                    decl.units[0]
                )));
            }
            (Type::Boolean, serde_json::Value::Bool(b)) => Val::Bool(*b),
            (Type::Choice, serde_json::Value::String(s)) if decl.choices.contains(s) => {
                Val::Text(s.clone())
            }
            (Type::Choice, serde_json::Value::String(s)) => {
                return Err(bad(format!(
                    "`{name}` = \"{s}\" is not one of {}",
                    decl.choices.join(", ")
                )));
            }
            (Type::String, serde_json::Value::String(s)) => Val::Text(s.clone()),
            (kind, other) => {
                return Err(bad(format!(
                    "`{name}` must be a {} (got {other})",
                    serde_json::to_value(kind)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default()
                )));
            }
        };
        if val.as_f64().is_some_and(|n| !n.is_finite()) {
            return Err(bad(format!("`{name}` is not a finite number")));
        }
        out.insert(name.clone(), val);
    }
    Ok(out)
}

/// Whether a read-back confirms what was set.
pub fn confirms(decl: &Decl, want: &Val, got: &Val) -> bool {
    match (want.as_f64(), got.as_f64()) {
        (Some(w), Some(g)) => {
            let tol = decl
                .tolerance
                .unwrap_or_else(|| (w.abs() * TOLERANCE_REL).max(TOLERANCE_ABS));
            (w - g).abs() <= tol + 1e-12
        }
        _ => want == got,
    }
}

/// Whether a watched value changed enough to report (`watch --changes`).
pub fn changed(decl: Option<&Decl>, old: Option<&Val>, new: Option<&Val>) -> bool {
    match (old, new) {
        (Some(o), Some(n)) => match (decl, o.as_f64(), n.as_f64()) {
            (Some(d), Some(_), Some(_)) => !confirms(d, o, n) || unit_of(o) != unit_of(n),
            _ => o != n,
        },
        (None, None) => false,
        _ => true,
    }
}

fn unit_of(v: &Val) -> Option<&str> {
    match v {
        Val::Measured { unit, .. } => Some(unit),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// Drivers and semantics

/// What a driver of a declared capability implements. Values arrive checked and in the declared
/// unit; `read` reports raw JSON that the core checks against the declarations.
#[async_trait]
pub trait Driver: Send + Sync {
    async fn open(&self) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    async fn read(&self, channel: Option<&str>) -> Result<serde_json::Value>;
    async fn set(&self, channel: Option<&str>, name: &str, value: &Val) -> Result<()>;
    /// Run an action. One that produces a file gets `output` to fill and reports what it
    /// produced.
    async fn action(
        &self,
        channel: Option<&str>,
        name: &str,
        args: &IndexMap<String, Val>,
        output: Option<&Output>,
    ) -> Result<Option<Produced>>;
    /// Deliver a stream: describe it on `output` once, fill it, and return when the source ends.
    /// It is stopped by cancelling `output` (or by dropping the call).
    async fn stream(
        &self,
        channel: Option<&str>,
        name: &str,
        args: &IndexMap<String, Val>,
        output: &StreamOutput,
    ) -> Result<()> {
        let _ = (channel, args, output);
        Err(DeskError::Internal(format!(
            "{}: this driver has no streams (asked for `{name}`)",
            self.label()
        )))
    }
    fn take_log(&self) -> Vec<String> {
        Vec::new()
    }
    /// For messages: `iron.lua`, `sim (endpoint=iron)`.
    fn label(&self) -> String;
}

/// The outcome of one operation on one declared capability (or channel).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Outcome {
    pub values: Readings,
    /// Per setting that was set: `true` = read back and confirmed; absent = the device doesn't
    /// report it.
    pub verified: IndexMap<String, bool>,
    pub warnings: Vec<String>,
    /// Something was sent (`false` for reads and dry runs).
    pub applied: bool,
}

pub async fn read(
    drv: &dyn Driver,
    decls: &Declarations,
    channel: Option<&str>,
) -> Result<Readings> {
    if !decls.readable() {
        return Ok(Readings::new());
    }
    let raw = drv.read(channel).await?;
    decode(decls, raw, &drv.label())
}

/// Apply settings, already parsed and range-checked, in declaration order (a boolean `switches`
/// setting set to false goes first); then read back and verify. With `dry_run`, only read.
pub async fn set(
    drv: &dyn Driver,
    decls: &Declarations,
    channel: Option<&str>,
    requested: &Readings,
    dry_run: bool,
) -> Result<Outcome> {
    if dry_run {
        return Ok(Outcome {
            values: read(drv, decls, channel).await?,
            ..Outcome::default()
        });
    }
    // Switching something off comes first, so a new setpoint never reaches a live device on its
    // way off. Everything else in declaration order (models declare the safe order for "on").
    let off_first = |name: &str| {
        decls
            .settings
            .get(name)
            .is_some_and(|d| d.kind == Type::Boolean && d.switches)
            && requested.get(name).and_then(Val::as_bool) == Some(false)
    };
    for pass_off in [true, false] {
        for name in decls.settings.keys() {
            if off_first(name) == pass_off
                && let Some(v) = requested.get(name)
            {
                drv.set(channel, name, v).await?;
            }
        }
    }
    let values = read(drv, decls, channel).await?;
    let mut verified = IndexMap::new();
    for (name, want) in requested {
        let Some(decl) = decls.settings.get(name) else {
            continue;
        };
        if let Some(got) = values.get(name) {
            if !confirms(decl, want, got) {
                return Err(DeskError::VerifyFailed {
                    expected: format!("{name} {}", want.display(Some(decl))),
                    actual: got.display(Some(decl)),
                });
            }
            verified.insert(name.clone(), true);
        }
    }
    Ok(Outcome {
        values,
        verified,
        warnings: Vec::new(),
        applied: !requested.is_empty(),
    })
}

/// Run an action (arguments already parsed and checked), then read the state after. An action
/// that produces a file fills `output`; it must report a format and leave something in it.
pub async fn act(
    drv: &dyn Driver,
    decls: &Declarations,
    channel: Option<&str>,
    name: &str,
    args: &IndexMap<String, Val>,
    dry_run: bool,
    output: Option<&Output>,
) -> Result<(Outcome, Option<Produced>)> {
    let mut produced = None;
    if !dry_run {
        produced = drv.action(channel, name, args, output).await?;
        if let Some(out) = output {
            out.close();
            let label = drv.label();
            if produced.is_none() {
                return Err(DeskError::Internal(format!(
                    "{label}: `{name}` produces a file, but the driver reported no format"
                )));
            }
            if out.size() == 0 {
                return Err(DeskError::EndpointUnavailable {
                    endpoint: label,
                    message: format!("`{name}` produced no data (the capture is empty)"),
                });
            }
        }
    }
    // The action is done: a read that fails now must not undo that. Its result (a file, something
    // that switched) is reported, with the read's error as a warning. A dry run did nothing, so
    // there the read is all there is.
    let mut outcome = Outcome {
        applied: !dry_run,
        ..Outcome::default()
    };
    match read(drv, decls, channel).await {
        Ok(values) => outcome.values = values,
        Err(e) if dry_run => return Err(e),
        Err(e) => outcome.warnings.push(format!(
            "`{name}` is done, but reading the device afterwards failed: {e}"
        )),
    }
    Ok((outcome, produced))
}

/// A value as JSON (for a script's `params` and logs).
pub fn to_json(v: &Val) -> serde_json::Value {
    v.to_json()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decls(text: &str) -> std::result::Result<Declarations, String> {
        let mut t: toml::Table = toml::from_str(text).unwrap();
        Declarations::take(&mut t)
    }

    const IRON: &str = r#"
        [values]
        temperature = { type = "number", unit = "°C" }
        reading = { type = "number", units = ["V", "Ω"] }
        [settings]
        setpoint = { type = "number", unit = "°C", min = 50, max = 450 }
        mode = { type = "choice", choices = ["normal", "boost"] }
        [actions]
        standby = { switches = true }
        boost = { switches = true, args = { seconds = { type = "number", unit = "s", min = 1, max = 60 } } }
    "#;

    #[test]
    fn parses_declarations() {
        let d = decls(IRON).unwrap();
        assert_eq!(d.values.len(), 2);
        assert!(
            d.settings["setpoint"].switches,
            "settings switch by default"
        );
        assert!(!d.values["temperature"].switches);
        assert_eq!(d.actions["boost"].args["seconds"].max, Some(60.0));
        let d = decls("channels = 2\n[values]\nx = { type = \"boolean\" }").unwrap();
        assert_eq!(d.channels, Some(vec!["1".into(), "2".into()]));
    }

    #[test]
    fn rejects_bad_declarations() {
        let err = |t: &str| decls(t).unwrap_err();
        assert!(err("[values]\nt = { type = \"number\", unit = \"C\" }").contains("\"°C\""));
        assert!(
            err("[settings]\nt = { type = \"number\", units = [\"V\", \"A\"] }").contains("fixed")
        );
        assert!(
            err("[values]\nt = { type = \"number\", switches = true }").contains("can't switch")
        );
        assert!(err("[actions]\ngo = { doc = \"x\" }").contains("switches"));
        assert!(err("[values]\nt = { type = \"choice\" }").contains("choices"));
        assert!(err("[values]\nt = { type = \"number\", min = 5, max = 1 }").contains("above"));
        assert!(err("[values]\nt = { type = \"number\", unti = \"V\" }").contains("unti"));
        assert!(err("[values]\nT = { type = \"boolean\" }").contains("lowercase"));
        assert!(
            err("[values]\nx = { type = \"boolean\" }\n[actions]\nx = { switches = false }")
                .contains("twice")
        );
        assert!(err("channels = 0").contains("count"));
    }

    #[test]
    fn action_arguments_have_defaults_or_are_optional() {
        let d = decls(
            r#"
            [actions.capture]
            switches = false
            produces = "file"
            args = { duration = { type = "number", unit = "s", min = 0.001, max = 600 }, samplerate = { type = "number", unit = "Hz", default = 10e6 }, trigger = { type = "string", optional = true } }
        "#,
        )
        .unwrap();
        let a = &d.actions["capture"];
        assert!(a.produces_file());
        assert!(a.needs_args());
        assert!(a.args["duration"].required());
        assert_eq!(a.args["samplerate"].default, Some(Val::Number(10e6)));
        assert!(!a.args["samplerate"].required());
        assert!(a.args["trigger"].optional);
        let err = |t: &str| decls(t).unwrap_err();
        assert!(
            err("[actions.go]\nswitches = false\nargs = { a = { type = \"number\", max = 5, default = 6 } }")
                .contains("device maximum")
        );
        assert!(
            err("[actions.go]\nswitches = false\nargs = { a = { type = \"boolean\", default = true, optional = true } }")
                .contains("not both")
        );
        assert!(
            err("[actions.go]\nswitches = false\nargs = { a = { type = \"boolean\", optional = 1 } }")
                .contains("true or false")
        );
        assert!(
            err("[settings]\nx = { type = \"number\", optional = true }").contains("only action")
        );
        assert!(err("[actions.go]\nswitches = false\nproduces = \"files\"").contains("produces"));
    }

    #[test]
    fn streams_are_declared_like_actions() {
        let d = decls(
            r#"
            [streams.samples]
            content = "logic"
            doc = "live samples"
            args = { samplerate = { type = "number", unit = "Hz", default = 1e6 }, channels = { type = "string" } }
        "#,
        )
        .unwrap();
        assert!(!d.is_empty(), "a capability with only streams is valid");
        assert!(!d.readable());
        let s = &d.streams["samples"];
        assert_eq!(s.content, "logic");
        assert!(s.needs_args());
        assert_eq!(s.args["samplerate"].default, Some(Val::Number(1e6)));
        let err = |t: &str| decls(t).unwrap_err();
        assert!(err("[streams.s]\ndoc = \"x\"").contains("content"));
        assert!(err("[streams.s]\ncontent = \"Logic\"").contains("lowercase"));
        assert!(err("[streams.s]\ncontent = \"x\"\nrate = 1").contains("unknown key"));
        assert!(
            err("[actions]\ns = { switches = false }\n[streams.s]\ncontent = \"x\"")
                .contains("twice")
        );
    }

    #[test]
    fn parses_input_and_checks_ranges() {
        let d = decls(IRON).unwrap();
        let sp = &d.settings["setpoint"];
        assert_eq!(
            parse_input("setpoint", sp, "330c").unwrap(),
            Val::Number(330.0)
        );
        assert_eq!(
            parse_input("setpoint", sp, "330").unwrap_err().code(),
            "usage"
        );
        let mode = &d.settings["mode"];
        assert_eq!(
            parse_input("mode", mode, "BOOST").unwrap(),
            Val::Text("boost".into())
        );
        assert!(parse_input("mode", mode, "turbo").is_err());

        let check = |v: f64, l: Option<Limit>| check_range("setpoint", sp, &Val::Number(v), l);
        assert!(check(330.0, None).is_ok());
        let e = check(500.0, None).unwrap_err();
        assert_eq!(e.code(), "limit_exceeded");
        assert!(e.to_string().contains("device maximum"), "{e}");
        let e = check(20.0, None).unwrap_err();
        assert!(e.to_string().contains("below the device minimum"), "{e}");
        let bench = Some(Limit {
            min: None,
            max: Some(380.0),
        });
        assert!(
            check(400.0, bench)
                .unwrap_err()
                .to_string()
                .contains("bench limit")
        );
        assert!(check(380.0, bench).is_ok());

        let free = decls("[settings]\nlevel = { type = \"number\" }").unwrap();
        let e = check_range(
            "level",
            &free.settings["level"],
            &Val::Number(f64::INFINITY),
            None,
        )
        .unwrap_err();
        assert_eq!(e.code(), "usage");
    }

    #[test]
    fn decodes_driver_readings() {
        let d = decls(IRON).unwrap();
        let r = decode(
            &d,
            serde_json::json!({ "temperature": 312.5, "setpoint": 330, "reading": { "value": 4.98, "unit": "V" } }),
            "iron.lua",
        )
        .unwrap();
        assert_eq!(r["temperature"], Val::Number(312.5));
        assert_eq!(
            r["reading"],
            Val::Measured {
                value: 4.98,
                unit: "V".into()
            }
        );
        assert_eq!(r.keys().next().unwrap(), "setpoint", "settings first");

        let e = |v| decode(&d, v, "iron.lua").unwrap_err().to_string();
        assert!(e(serde_json::json!({ "tmp": 1 })).contains("not a declared"));
        assert!(
            e(serde_json::json!({ "reading": { "value": 1, "unit": "A" } })).contains("not one of")
        );
        assert!(e(serde_json::json!({ "reading": 1 })).contains("value ="));
        assert!(e(serde_json::json!({ "mode": "turbo" })).contains("not one of"));
        assert!(e(serde_json::json!({ "temperature": "hot" })).contains("must be a number"));
    }

    #[test]
    fn tolerance_and_changes() {
        let d = decls(IRON).unwrap();
        let sp = &d.settings["setpoint"];
        assert!(confirms(sp, &Val::Number(330.0), &Val::Number(331.0)));
        assert!(!confirms(sp, &Val::Number(330.0), &Val::Number(340.0)));
        let t = Some(&d.values["temperature"]);
        assert!(!changed(
            t,
            Some(&Val::Number(300.0)),
            Some(&Val::Number(300.001))
        ));
        assert!(changed(
            t,
            Some(&Val::Number(300.0)),
            Some(&Val::Number(310.0))
        ));
        assert!(changed(t, None, Some(&Val::Number(300.0))));
    }
}
