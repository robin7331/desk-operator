//! Operations on a device, whatever its capabilities: `get`, `set`, `do`, and the readings `watch`
//! streams.
//!
//! A device's capabilities form one **catalog** of names: the well-known `source`
//! (voltage_source, names fixed by the core) and every declared capability. What a user types is
//! resolved and checked against it (names, units, ranges, limits) *before* anything is opened, which
//! gives an [`Op`]. Running an op then needs only the open drivers: in a session of its own, or in
//! the daemon's live session of a watched device.
//!
//! Power supplies keep their stricter rules: their part of an op runs through
//! [`crate::source::apply`] (setpoints before output, protection handling, limits on switch-on).

use std::time::Duration;

use indexmap::IndexMap;
use serde::Serialize;

use crate::config::{Device, SOURCE};
use crate::declared::{self, Action, Decl, Declarations, Driver, Readings, Type, Val};
use crate::error::{DeskError, Result};
use crate::model::Power;
use crate::source::{self, Limits, Mode, Reading, Request, SourceInfo, VoltageSource};

/// One capability in a device's catalog.
#[derive(Debug, Clone)]
pub struct Entry {
    pub key: String,
    pub kind: Kind,
    pub decls: Declarations,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// `voltage_source`, with the device's ratings.
    Source(SourceInfo),
    Declared,
}

/// A device's capabilities (not mains), in config order.
#[derive(Debug, Clone)]
pub struct Catalog {
    pub device: String,
    pub entries: Vec<Entry>,
}

/// Default timeout of a declared capability's commands.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

impl Catalog {
    pub fn of(device: &Device) -> Result<Catalog> {
        let mut entries = Vec::new();
        for (key, cap) in &device.caps {
            let (kind, decls) = if key == SOURCE {
                let ratings = match &cap.binding {
                    crate::config::Binding::Script { ratings, .. } if !ratings.is_empty() => {
                        source::parse_ratings(ratings).map_err(DeskError::ConfigInvalid)?
                    }
                    _ => SourceInfo::default(),
                };
                (
                    Kind::Source(ratings),
                    source_decls(ratings, cap.decls.channels.clone()),
                )
            } else {
                (Kind::Declared, cap.decls.clone())
            };
            let default = if key == SOURCE {
                source::DEFAULT_TIMEOUT
            } else {
                DEFAULT_TIMEOUT
            };
            entries.push(Entry {
                key: key.clone(),
                kind,
                decls,
                timeout: cap.binding.timeout().unwrap_or(default),
            });
        }
        Ok(Catalog {
            device: device.id.clone(),
            entries,
        })
    }

    pub fn keys(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.key.clone()).collect()
    }

    /// Every name with where it lives, for messages: `setpoint`, or `heater.setpoint` if it clashes.
    fn all_names(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in &self.entries {
            for n in e.decls.names() {
                let clashes = self
                    .entries
                    .iter()
                    .filter(|o| o.decls.names().any(|m| m == n))
                    .count()
                    > 1;
                out.push(if clashes {
                    format!("{}.{n}", e.key)
                } else {
                    n.clone()
                });
            }
        }
        out
    }

    /// Find a name (`setpoint` or `heater.setpoint`): the entry's index and the bare name.
    pub fn resolve(&self, query: &str) -> Result<(usize, String)> {
        let (cap, name) = match query.split_once('.') {
            Some((c, n)) => (Some(c), n),
            None => (None, query),
        };
        let hits: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| cap.is_none_or(|c| c == e.key) && e.decls.names().any(|n| n == name))
            .map(|(i, _)| i)
            .collect();
        match hits.as_slice() {
            [i] => Ok((*i, name.to_string())),
            [] => {
                // A power supply's name on a device that has none: say what's missing.
                let psu = source_decls(SourceInfo::default(), None);
                if cap.is_none_or(|c| c == SOURCE)
                    && psu.names().any(|n| n == name)
                    && !self.entries.iter().any(|e| e.key == SOURCE)
                {
                    return Err(DeskError::CapabilityMissing {
                        device: self.device.clone(),
                        missing: "has no `source` binding, so it can't do voltage_source (no \
                                  `voltage`, `current`, `output`)"
                            .into(),
                    });
                }
                let known = self.all_names();
                Err(DeskError::Usage(format!(
                    "device `{}` has no `{query}` ({})",
                    self.device,
                    if known.is_empty() {
                        "it has no values, settings or actions".to_string()
                    } else {
                        format!("it has: {}", known.join(", "))
                    }
                )))
            }
            many => Err(DeskError::Usage(format!(
                "`{query}` is ambiguous on device `{}`: say {}",
                self.device,
                many.iter()
                    .map(|i| format!("`{}.{name}`", self.entries[*i].key))
                    .collect::<Vec<_>>()
                    .join(" or ")
            ))),
        }
    }

    /// The declaration of a readable name (for display and `--changes`), any capability.
    pub fn readable_decl(&self, name: &str) -> Option<&Decl> {
        self.entries
            .iter()
            .find_map(|e| e.decls.readable_decl(name))
    }

    /// Find a setting: the entry's index, the bare name and its declaration.
    pub fn setting(&self, query: &str) -> Result<(usize, String, &Decl)> {
        let (i, name) = self.resolve(query)?;
        let e = &self.entries[i];
        match e.decls.settings.get(&name) {
            Some(decl) => Ok((i, name, decl)),
            None => Err(DeskError::Usage(if e.decls.actions.contains_key(&name) {
                format!("`{name}` is an action: `desk do {} {name}`", self.device)
            } else {
                format!("`{name}` is a value the device reports; it can't be set")
            })),
        }
    }

    /// Find an action: the entry's index, the bare name and its declaration.
    pub fn action(&self, query: &str) -> Result<(usize, String, &Action)> {
        let (i, name) = self.resolve(query)?;
        let e = &self.entries[i];
        match e.decls.actions.get(&name) {
            Some(action) => Ok((i, name, action)),
            None => Err(DeskError::Usage(format!(
                "`{name}` is not an action (actions: {}); values and settings use `desk get` / `desk set`",
                names_or_none(e.decls.actions.keys())
            ))),
        }
    }

    /// Whether any capability has this channel; if not, an error that says what's wrong.
    pub fn check_any_channel(&self, channel: &str) -> Result<()> {
        let any = self.entries.iter().any(|e| {
            e.decls
                .channels
                .as_ref()
                .is_some_and(|c| c.iter().any(|x| x == channel))
        });
        if any {
            return Ok(());
        }
        // Name the problem precisely (no channels at all, or not this one).
        for i in 0..self.entries.len() {
            self.check_channel(i, Some(channel), false)?;
        }
        Err(DeskError::Usage(format!(
            "device `{}` has no channels",
            self.device
        )))
    }

    pub fn check_channel(&self, entry: usize, channel: Option<&str>, need: bool) -> Result<()> {
        let e = &self.entries[entry];
        match (&e.decls.channels, channel) {
            (None, None) => Ok(()),
            (None, Some(ch)) => Err(DeskError::Usage(format!(
                "`{}` of device `{}` has no channels (asked for `{}:{ch}`)",
                e.key, self.device, self.device
            ))),
            (Some(chs), None) if need => Err(DeskError::Usage(format!(
                "`{}` of device `{}` has channels: address one, e.g. `{}:{}`",
                e.key, self.device, self.device, chs[0]
            ))),
            (Some(_), None) => Ok(()),
            (Some(chs), Some(ch)) if chs.iter().any(|c| c == ch) => Ok(()),
            (Some(chs), Some(ch)) => Err(DeskError::Usage(format!(
                "device `{}` has no channel `{ch}` (channels: {})",
                self.device,
                chs.join(", ")
            ))),
        }
    }
}

/// The names of `voltage_source`, fixed by the core so every power supply looks the same.
pub fn source_decls(ratings: SourceInfo, channels: Option<Vec<String>>) -> Declarations {
    let num = |unit: &str, max: Option<f64>, doc: &str, switches: bool| Decl {
        kind: Type::Number,
        unit: Some(unit.into()),
        units: Vec::new(),
        min: max.map(|_| 0.0),
        max,
        tolerance: None,
        choices: Vec::new(),
        doc: Some(doc.into()),
        switches,
    };
    let value = |unit: &str, doc: &str| num(unit, None, doc, false);
    let choice = |choices: &[&str], doc: &str| Decl {
        kind: Type::Choice,
        unit: None,
        units: Vec::new(),
        min: None,
        max: None,
        tolerance: None,
        choices: choices.iter().map(|c| c.to_string()).collect(),
        doc: Some(doc.into()),
        switches: false,
    };
    let mut settings = IndexMap::new();
    settings.insert(
        "voltage".into(),
        num("V", ratings.max_voltage, "voltage setpoint", true),
    );
    settings.insert(
        "current".into(),
        num("A", ratings.max_current, "current limit", true),
    );
    settings.insert(
        "output".into(),
        Decl {
            kind: Type::Boolean,
            doc: Some("output terminals on/off (not mains)".into()),
            switches: true,
            ..choice(&[], "")
        },
    );
    let mut values = IndexMap::new();
    values.insert(
        "measured_voltage".into(),
        value("V", "measured at the output"),
    );
    values.insert(
        "measured_current".into(),
        value("A", "measured at the output"),
    );
    values.insert(
        "measured_power".into(),
        Decl {
            max: ratings.max_power,
            ..value(
                "W",
                "measured at the output (max: the device's rating, informational)",
            )
        },
    );
    values.insert(
        "mode".into(),
        choice(
            &["cv", "cc"],
            "cv: holds the voltage; cc: the current limit is reached",
        ),
    );
    values.insert(
        "protection".into(),
        choice(
            &["none", "ovp", "ocp", "opp", "otp", "uvp", "other"],
            "a protection that tripped and switched the output off",
        ),
    );
    values.insert("temperature".into(), value("°C", "internal temperature"));
    values.insert("input_voltage".into(), value("V", "the supply's own input"));
    values.insert(
        "charge".into(),
        value("Ah", "delivered since the counter was reset"),
    );
    values.insert(
        "energy".into(),
        value("Wh", "delivered since the counter was reset"),
    );
    Declarations {
        values,
        settings,
        actions: IndexMap::new(),
        channels,
    }
}

/// A `voltage_source` reading under the generic names (only what the device reports).
fn source_readings(r: &Reading) -> Readings {
    let mut out = Readings::new();
    let mut num = |k: &str, v: Option<f64>| {
        if let Some(v) = v {
            out.insert(k.into(), Val::Number(v));
        }
    };
    num("voltage", r.voltage_set);
    num("current", r.current_set);
    if let Some(o) = r.output {
        out.insert("output".into(), Val::Bool(o));
    }
    let mut num = |k: &str, v: Option<f64>| {
        if let Some(v) = v {
            out.insert(k.into(), Val::Number(v));
        }
    };
    num("measured_voltage", r.voltage);
    num("measured_current", r.current);
    num("measured_power", r.power);
    if let Some(m) = r.mode {
        out.insert(
            "mode".into(),
            Val::Text(
                match m {
                    Mode::Cv => "cv",
                    Mode::Cc => "cc",
                }
                .into(),
            ),
        );
    }
    if let Some(p) = r.protection {
        let name = serde_json::to_value(p)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "other".into());
        out.insert("protection".into(), Val::Text(name));
    }
    let mut num = |k: &str, v: Option<f64>| {
        if let Some(v) = v {
            out.insert(k.into(), Val::Number(v));
        }
    };
    num("temperature", r.temperature);
    num("input_voltage", r.input_voltage);
    num("charge", r.charge);
    num("energy", r.energy);
    out
}

// ---------------------------------------------------------------------------------------------
// Ops: what to do, already resolved and checked

#[derive(Debug, Clone)]
pub enum Op {
    /// Read everything (the caller filters names for output).
    Get { channel: Option<String> },
    Set {
        channel: Option<String>,
        /// Per capability (index into the catalog), in catalog order.
        parts: Vec<SetPart>,
        dry_run: bool,
    },
    Do {
        channel: Option<String>,
        entry: usize,
        action: String,
        args: IndexMap<String, Val>,
        dry_run: bool,
    },
}

#[derive(Debug, Clone)]
pub struct SetPart {
    pub entry: usize,
    pub values: Readings,
    /// For `source`: its request (the core's PSU rules) with limits and ratings.
    pub source: Option<(Request, Limits, SourceInfo)>,
}

impl Op {
    /// The catalog entries this op needs open.
    pub fn entries(&self, cat: &Catalog) -> Vec<usize> {
        match self {
            Op::Get { .. } => (0..cat.entries.len()).collect(),
            Op::Set { parts, .. } => parts.iter().map(|p| p.entry).collect(),
            Op::Do { entry, .. } => vec![*entry],
        }
    }

    /// Whether it may change something (for mains checks it doesn't matter; for messages it does).
    pub fn changes(&self) -> bool {
        match self {
            Op::Get { .. } => false,
            Op::Set { dry_run, .. } | Op::Do { dry_run, .. } => !dry_run,
        }
    }
}

/// `desk get <device[:channel]> [<name>...]`: which names to show (`None` = all).
pub fn plan_get(
    cat: &Catalog,
    channel: Option<&str>,
    names: &[String],
) -> Result<(Op, Option<Vec<String>>)> {
    if let Some(ch) = channel {
        cat.check_any_channel(ch)?;
    }
    let filter = if names.is_empty() {
        None
    } else {
        let mut out = Vec::new();
        for n in names {
            let (i, name) = cat.resolve(n)?;
            if cat.entries[i].decls.actions.contains_key(&name) {
                return Err(DeskError::Usage(format!(
                    "`{name}` is an action (run it with `desk do {} {name}`), not a value",
                    cat.device
                )));
            }
            out.push(name);
        }
        Some(out)
    };
    Ok((
        Op::Get {
            channel: channel.map(str::to_string),
        },
        filter,
    ))
}

/// `desk set <device[:channel]> <name> <value> …`: parse, check ranges and limits, group.
pub fn plan_set(
    cat: &Catalog,
    device: &Device,
    channel: Option<&str>,
    pairs: &[String],
    clear_protection: bool,
    dry_run: bool,
) -> Result<Op> {
    if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
        return Err(DeskError::Usage(
            "give pairs of setting and value, e.g. `desk set psu voltage 5v current 1a`".into(),
        ));
    }
    let mut parts: Vec<SetPart> = Vec::new();
    for pair in pairs.chunks(2) {
        let (query, input) = (&pair[0], &pair[1]);
        let (i, name, decl) = cat.setting(query)?;
        let e = &cat.entries[i];
        cat.check_channel(i, channel, true)?;
        let val = declared::parse_input(&name, decl, input)?;
        if let Kind::Declared = e.kind {
            declared::check_range(&name, decl, &val, device.limits.get(&e.key, &name, channel))?;
        }
        let part = match parts.iter_mut().find(|p| p.entry == i) {
            Some(p) => p,
            None => {
                parts.push(SetPart {
                    entry: i,
                    values: Readings::new(),
                    source: None,
                });
                parts
                    .last_mut()
                    .ok_or_else(|| DeskError::Internal("no part".into()))?
            }
        };
        if part.values.insert(name.clone(), val).is_some() {
            return Err(DeskError::Usage(format!("`{name}` given twice")));
        }
    }
    parts.sort_by_key(|p| p.entry);
    let mut wants_on = false;
    for p in &mut parts {
        if let Kind::Source(ratings) = cat.entries[p.entry].kind {
            let req = Request {
                voltage: p.values.get("voltage").and_then(Val::as_f64),
                current: p.values.get("current").and_then(Val::as_f64),
                output: p.values.get("output").and_then(Val::as_bool),
                clear_protection,
            };
            wants_on |= req.output == Some(true);
            p.source = Some((req, device.limits.source(channel), ratings));
        }
    }
    if clear_protection && !wants_on {
        return Err(DeskError::Usage(
            "--clear-protection only applies when switching a power supply's output on \
             (`output on`)"
                .into(),
        ));
    }
    Ok(Op::Set {
        channel: channel.map(str::to_string),
        parts,
        dry_run,
    })
}

/// `desk do <device[:channel]> <action> [arg=value…]`.
pub fn plan_do(
    cat: &Catalog,
    channel: Option<&str>,
    query: &str,
    raw_args: &[String],
    dry_run: bool,
) -> Result<Op> {
    let (i, name, action) = cat.action(query)?;
    cat.check_channel(i, channel, true)?;
    let args = parse_args(&name, action, raw_args)?;
    Ok(Op::Do {
        channel: channel.map(str::to_string),
        entry: i,
        action: name,
        args,
        dry_run,
    })
}

fn names_or_none<'a>(names: impl Iterator<Item = &'a String>) -> String {
    let v: Vec<&str> = names.map(String::as_str).collect();
    if v.is_empty() {
        "none".into()
    } else {
        v.join(", ")
    }
}

fn parse_args(action: &str, decl: &Action, raw: &[String]) -> Result<IndexMap<String, Val>> {
    let mut given: IndexMap<String, Val> = IndexMap::new();
    for a in raw {
        let Some((k, v)) = a.split_once('=') else {
            return Err(DeskError::Usage(format!(
                "`{a}`: arguments look like name=value (e.g. seconds=10s)"
            )));
        };
        let Some(d) = decl.args.get(k) else {
            return Err(DeskError::Usage(format!(
                "`{action}` has no argument `{k}` (arguments: {})",
                names_or_none(decl.args.keys())
            )));
        };
        let val = declared::parse_input(k, d, v)?;
        declared::check_range(k, d, &val, None)?;
        if given.insert(k.to_string(), val).is_some() {
            return Err(DeskError::Usage(format!("`{k}` given twice")));
        }
    }
    let missing: Vec<&String> = decl
        .args
        .keys()
        .filter(|k| !given.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(DeskError::Usage(format!(
            "`{action}` needs {}",
            missing
                .iter()
                .map(|k| format!("`{k}=…`"))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    // Declaration order.
    Ok(decl
        .args
        .keys()
        .filter_map(|k| given.get(k).map(|v| (k.clone(), v.clone())))
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Running ops

/// An open driver of one capability.
pub enum CapDriver {
    Source(Box<dyn VoltageSource>),
    Declared(Box<dyn Driver>),
}

impl CapDriver {
    pub async fn open(&self) -> Result<()> {
        match self {
            CapDriver::Source(s) => s.open().await,
            CapDriver::Declared(d) => d.open().await,
        }
    }

    pub async fn close(&self) -> Result<()> {
        match self {
            CapDriver::Source(s) => s.close().await,
            CapDriver::Declared(d) => d.close().await,
        }
    }

    pub fn take_log(&self) -> Vec<String> {
        match self {
            CapDriver::Source(s) => s.take_log(),
            CapDriver::Declared(d) => d.take_log(),
        }
    }
}

/// Open drivers, aligned with the catalog (`None` = not needed or not opened).
pub type Drivers = Vec<Option<CapDriver>>;

/// Everything read from a device: capabilities without channels in `values`, the others per
/// channel (or in `values` too when one channel was addressed).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeviceReadings {
    pub values: Readings,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub channels: IndexMap<String, Readings>,
}

impl DeviceReadings {
    /// Only the named values (`watch iron temperature mode`), in the order named.
    pub fn filtered(&self, names: &[String]) -> DeviceReadings {
        let keep = |r: &Readings| -> Readings {
            names
                .iter()
                .filter_map(|n| r.get(n).map(|v| (n.clone(), v.clone())))
                .collect()
        };
        DeviceReadings {
            values: keep(&self.values),
            channels: self
                .channels
                .iter()
                .map(|(c, r)| (c.clone(), keep(r)))
                .collect(),
        }
    }

    /// Every (channel, name, value), for comparing two readings.
    pub fn flat(&self) -> Vec<(Option<&str>, &str, &Val)> {
        let mut out: Vec<(Option<&str>, &str, &Val)> = self
            .values
            .iter()
            .map(|(k, v)| (None, k.as_str(), v))
            .collect();
        for (c, r) in &self.channels {
            out.extend(r.iter().map(|(k, v)| (Some(c.as_str()), k.as_str(), v)));
        }
        out
    }
}

/// What an op did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Outcome {
    #[serde(flatten)]
    pub readings: DeviceReadings,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub verified: IndexMap<String, bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub applied: bool,
}

fn missing(cat: &Catalog, i: usize) -> DeskError {
    DeskError::Internal(format!(
        "`{}` of `{}` is not open",
        cat.entries[i].key, cat.device
    ))
}

/// Read one capability (all its channels, or the addressed one) into `into`.
async fn read_entry(
    cat: &Catalog,
    i: usize,
    drv: &CapDriver,
    channel: Option<&str>,
    into: &mut DeviceReadings,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let e = &cat.entries[i];
    let channels: Vec<Option<String>> = match (&e.decls.channels, channel) {
        (None, _) => vec![None],
        (Some(_), Some(ch)) => vec![Some(ch.to_string())],
        (Some(chs), None) => chs.iter().cloned().map(Some).collect(),
    };
    for ch in channels {
        let r = read_one(e, drv, ch.as_deref(), warnings).await?;
        match (&e.decls.channels, channel, ch) {
            (Some(_), None, Some(ch)) => into.channels.entry(ch).or_default().extend(r),
            _ => into.values.extend(r),
        }
    }
    Ok(())
}

async fn read_one(
    e: &Entry,
    drv: &CapDriver,
    channel: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<Readings> {
    match (drv, e.kind) {
        (CapDriver::Source(s), Kind::Source(_)) => {
            s.select_channel(channel)?;
            Ok(source_readings(&source::read(s.as_ref(), warnings).await?))
        }
        (CapDriver::Declared(d), Kind::Declared) => {
            declared::read(d.as_ref(), &e.decls, channel).await
        }
        _ => Err(DeskError::Internal(format!(
            "`{}`: driver kind mismatch",
            e.key
        ))),
    }
}

/// Run an op on open drivers. Reads go through every open capability; changes only through the
/// ones the op names.
pub async fn run(cat: &Catalog, drivers: &Drivers, op: &Op) -> Result<Outcome> {
    let mut out = Outcome::default();
    match op {
        Op::Get { channel } => {
            for (i, d) in drivers.iter().enumerate() {
                let Some(d) = d else { continue };
                let e = &cat.entries[i];
                // With a channel: only capabilities that have it, plus those without channels.
                if channel.is_some() && e.decls.channels.is_none() {
                    read_entry(cat, i, d, None, &mut out.readings, &mut out.warnings).await?;
                    continue;
                }
                read_entry(
                    cat,
                    i,
                    d,
                    channel.as_deref(),
                    &mut out.readings,
                    &mut out.warnings,
                )
                .await?;
            }
        }
        Op::Set {
            channel,
            parts,
            dry_run,
        } => {
            let ch = channel.as_deref();
            for p in parts {
                let d = drivers
                    .get(p.entry)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| missing(cat, p.entry))?;
                let e = &cat.entries[p.entry];
                match (d, &p.source) {
                    (CapDriver::Source(s), Some((req, limits, ratings))) => {
                        s.select_channel(ch)?;
                        let r =
                            source::apply(s.as_ref(), *req, *limits, *ratings, *dry_run).await?;
                        let names = [
                            ("voltage", r.verified.voltage),
                            ("current", r.verified.current),
                            ("output", r.verified.output),
                        ];
                        for (n, v) in names {
                            if v == Some(true) {
                                out.verified.insert(n.into(), true);
                            }
                        }
                        out.warnings.extend(r.warnings);
                        out.applied |= r.applied;
                        out.readings.values.extend(source_readings(&r.reading));
                    }
                    (CapDriver::Declared(drv), None) => {
                        let o =
                            declared::set(drv.as_ref(), &e.decls, ch, &p.values, *dry_run).await?;
                        out.verified.extend(o.verified);
                        out.applied |= o.applied;
                        out.readings.values.extend(o.values);
                    }
                    _ => return Err(DeskError::Internal(format!("`{}`: kind mismatch", e.key))),
                }
            }
        }
        Op::Do {
            channel,
            entry,
            action,
            args,
            dry_run,
        } => {
            let d = drivers
                .get(*entry)
                .and_then(Option::as_ref)
                .ok_or_else(|| missing(cat, *entry))?;
            let CapDriver::Declared(drv) = d else {
                return Err(DeskError::Internal(
                    "actions need a declared capability".into(),
                ));
            };
            let e = &cat.entries[*entry];
            let o = declared::act(
                drv.as_ref(),
                &e.decls,
                channel.as_deref(),
                action,
                args,
                *dry_run,
            )
            .await?;
            out.applied = o.applied;
            out.readings.values.extend(o.values);
        }
    }
    Ok(out)
}

/// The report of a `get`, `set` or `do`.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub device: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Mains, when the device has a power path that can be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mains: Option<Power>,
    /// `set`: what was asked for; `do`: the action.
    #[serde(skip_serializing_if = "Readings::is_empty")]
    pub requested: Readings,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "IndexMap::is_empty")]
    pub args: IndexMap<String, Val>,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Report {
    pub fn new(cat: &Catalog, op: &Op, mains: Option<Power>, outcome: Outcome) -> Report {
        let (channel, requested, action, args) = match op {
            Op::Get { channel } => (channel.clone(), Readings::new(), None, IndexMap::new()),
            Op::Set { channel, parts, .. } => (
                channel.clone(),
                parts.iter().flat_map(|p| p.values.clone()).collect(),
                None,
                IndexMap::new(),
            ),
            Op::Do {
                channel,
                action,
                args,
                ..
            } => (
                channel.clone(),
                Readings::new(),
                Some(action.clone()),
                args.clone(),
            ),
        };
        Report {
            device: cat.device.clone(),
            channel,
            mains,
            requested,
            action,
            args,
            outcome,
        }
    }
}

/// `psu:2` → (`psu`, `Some("2")`).
pub fn split_channel(target: &str) -> (&str, Option<&str>) {
    match target.split_once(':') {
        Some((d, c)) if !c.is_empty() => (d, Some(c)),
        _ => (target, None),
    }
}

/// Whether `new` differs from `old` in any watched value (`watch --changes`).
pub fn changed(cat: &Catalog, old: &DeviceReadings, new: &DeviceReadings) -> bool {
    let a = old.flat();
    let b = new.flat();
    if a.len() != b.len() {
        return true;
    }
    b.iter().any(|(ch, name, v)| {
        let before = a
            .iter()
            .find(|(c, n, _)| c == ch && n == name)
            .map(|(_, _, v)| *v);
        declared::changed(cat.readable_decl(name), before, Some(v))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Protection;

    #[test]
    fn source_names_are_fixed() {
        let d = source_decls(
            SourceInfo {
                max_voltage: Some(30.0),
                max_current: Some(5.0),
                max_power: None,
            },
            None,
        );
        assert_eq!(
            d.settings.keys().collect::<Vec<_>>(),
            ["voltage", "current", "output"]
        );
        assert_eq!(d.settings["voltage"].max, Some(30.0));
        assert!(d.values.contains_key("measured_power"));
        assert!(!d.values.contains_key("power"), "`power` is mains");
        let r = source_readings(&Reading {
            output: Some(true),
            voltage_set: Some(5.0),
            voltage: Some(4.99),
            protection: Some(Protection::Ocp),
            ..Reading::default()
        });
        assert_eq!(r["voltage"], Val::Number(5.0));
        assert_eq!(r["measured_voltage"], Val::Number(4.99));
        assert_eq!(r["protection"], Val::Text("ocp".into()));
        assert!(!r.contains_key("current"), "unreported stays absent");
    }

    #[test]
    fn channels_in_targets() {
        assert_eq!(split_channel("psu:2"), ("psu", Some("2")));
        assert_eq!(split_channel("psu"), ("psu", None));
        assert_eq!(split_channel("psu:"), ("psu:", None));
    }
}
