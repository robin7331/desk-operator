//! The `voltage_source` capability (bench power supplies) and the semantics shared by every
//! driver: limit checks, setpoints before switching on (switching off before setpoints),
//! read-back verification, sessions.
//!
//! Drivers only implement the primitive operations. Everything a user relies on for safety lives
//! here, so it holds for every driver, native or Lua.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{DeskError, Result};

/// Used when a source binding doesn't set `timeout` (covers the whole session of one command).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// How far a read-back setpoint may differ from the requested one and still count as confirmed:
/// devices round to their display resolution.
const TOLERANCE_REL: f64 = 0.005;
pub const TOLERANCE_ABS: f64 = 0.011;

/// What the device itself can do: its ratings (from the binding) combined with what the driver
/// reports. Also the format of a `source` binding's `ratings = { … }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInfo {
    pub max_voltage: Option<f64>,
    pub max_current: Option<f64>,
    /// Informational (shown, not enforced yet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_power: Option<f64>,
    /// How far a read-back voltage setpoint may differ and still count as confirmed, V. Without
    /// it the default applies (0.5 % or 0.011, whichever is larger).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voltage_tolerance: Option<f64>,
    /// The same for the current setpoint, A.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_tolerance: Option<f64>,
}

impl SourceInfo {
    /// The stricter of both, field by field.
    pub fn stricter(self, other: SourceInfo) -> SourceInfo {
        let min = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        SourceInfo {
            max_voltage: min(self.max_voltage, other.max_voltage),
            max_current: min(self.max_current, other.max_current),
            max_power: min(self.max_power, other.max_power),
            voltage_tolerance: min(self.voltage_tolerance, other.voltage_tolerance),
            current_tolerance: min(self.current_tolerance, other.current_tolerance),
        }
    }

    /// The first number that isn't finite, if any (a driver bug).
    pub fn non_finite(&self) -> Option<&'static str> {
        [
            ("max_voltage", self.max_voltage),
            ("max_current", self.max_current),
            ("max_power", self.max_power),
            ("voltage_tolerance", self.voltage_tolerance),
            ("current_tolerance", self.current_tolerance),
        ]
        .into_iter()
        .find(|(_, v)| v.is_some_and(|v| !v.is_finite()))
        .map(|(name, _)| name)
    }
}

/// Parse and check a `source` binding's `ratings = { max_voltage, max_current, max_power }`.
pub fn parse_ratings(t: &toml::Table) -> std::result::Result<SourceInfo, String> {
    let r: SourceInfo = toml::Value::Table(t.clone())
        .try_into()
        .map_err(|e: toml::de::Error| format!("ratings: {}", e.message()))?;
    let maximum = "the device's maximum";
    let tolerance = "how far a read-back may differ";
    for (what, v, means) in [
        ("max_voltage", r.max_voltage, maximum),
        ("max_current", r.max_current, maximum),
        ("max_power", r.max_power, maximum),
        ("voltage_tolerance", r.voltage_tolerance, tolerance),
        ("current_tolerance", r.current_tolerance, tolerance),
    ] {
        if let Some(v) = v
            && !(v.is_finite() && v > 0.0)
        {
            return Err(format!("ratings.{what} must be > 0 ({means}), got {v}"));
        }
    }
    Ok(r)
}

/// Everything a driver can report. Every field is optional: drivers report what they can read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub output: Option<bool>,
    pub voltage_set: Option<f64>,
    pub current_set: Option<f64>,
    pub voltage: Option<f64>,
    pub current: Option<f64>,
    pub power: Option<f64>,
    /// Regulating voltage (CV) or limiting current (CC).
    pub mode: Option<Mode>,
    /// A protection that tripped and switched the output off, or `none`.
    pub protection: Option<Protection>,
    /// Internal temperature, °C.
    pub temperature: Option<f64>,
    /// The supply's own input voltage (e.g. its internal bus), V.
    pub input_voltage: Option<f64>,
    /// Charge delivered since the device's counter was reset, Ah.
    pub charge: Option<f64>,
    /// Energy delivered since the device's counter was reset, Wh.
    pub energy: Option<f64>,
}

impl Reading {
    /// The first number that isn't finite, if any (a driver bug, e.g. a badly decoded frame).
    pub fn non_finite(&self) -> Option<&'static str> {
        [
            ("voltage_set", self.voltage_set),
            ("current_set", self.current_set),
            ("voltage", self.voltage),
            ("current", self.current),
            ("power", self.power),
            ("temperature", self.temperature),
            ("input_voltage", self.input_voltage),
            ("charge", self.charge),
            ("energy", self.energy),
        ]
        .into_iter()
        .find(|(_, v)| v.is_some_and(|v| !v.is_finite()))
        .map(|(name, _)| name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Constant voltage: the output holds the voltage setpoint.
    Cv,
    /// Constant current: the current limit is reached, the voltage drops.
    Cc,
}

/// Protection state as the device reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protection {
    /// Nothing tripped.
    None,
    /// Over-voltage.
    Ovp,
    /// Over-current.
    Ocp,
    /// Over-power.
    Opp,
    /// Over-temperature.
    Otp,
    /// Input under-voltage.
    Uvp,
    /// Another protection the capability has no name for.
    Other,
}

impl Protection {
    pub fn tripped(self) -> bool {
        self != Protection::None
    }

    pub fn name(self) -> &'static str {
        match self {
            Protection::None => "none",
            Protection::Ovp => "OVP (over-voltage)",
            Protection::Ocp => "OCP (over-current)",
            Protection::Opp => "OPP (over-power)",
            Protection::Otp => "OTP (over-temperature)",
            Protection::Uvp => "UVP (input under-voltage)",
            Protection::Other => "a device protection",
        }
    }
}

/// Bench safety limits for a voltage source, from the device's `limits = { source = { ... } }`,
/// independent of the driver.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub voltage: Option<f64>,
    pub current: Option<f64>,
}

#[async_trait]
pub trait VoltageSource: Send + Sync {
    /// Start a session (connect, handshake). Called once per command, before anything else.
    async fn open(&self) -> Result<()> {
        Ok(())
    }
    /// End the session. Always called, also after errors and timeouts.
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    async fn info(&self) -> Result<SourceInfo> {
        Ok(SourceInfo::default())
    }
    async fn set_voltage(&self, volts: f64) -> Result<()>;
    async fn set_current(&self, amps: f64) -> Result<()>;
    async fn set_output(&self, on: bool) -> Result<()>;
    async fn read(&self) -> Result<Reading>;
    /// Address a channel for the next calls (multi-channel supplies). Drivers without channels
    /// only accept `None`.
    fn select_channel(&self, channel: Option<&str>) -> Result<()> {
        match channel {
            None => Ok(()),
            Some(ch) => Err(DeskError::ConfigInvalid(format!(
                "this driver has no channels (asked for channel `{ch}`)"
            ))),
        }
    }
    /// Log lines since the last call (Lua drivers); most have none.
    fn take_log(&self) -> Vec<String> {
        Vec::new()
    }
}

/// What a command asks for. Empty means "just read".
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Request {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voltage: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<bool>,
    /// Switch the output on although a protection tripped (on many devices that also clears it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub clear_protection: bool,
}

impl Request {
    pub fn is_empty(&self) -> bool {
        self.voltage.is_none() && self.current.is_none() && self.output.is_none()
    }
}

/// Per requested value: was it confirmed by reading it back?
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Verified {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voltage: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<bool>,
}

impl Verified {
    /// `true` when every requested value was confirmed.
    #[cfg(test)]
    pub fn all(&self) -> bool {
        [self.voltage, self.current, self.output]
            .iter()
            .all(|v| v.unwrap_or(true))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub requested: Request,
    pub reading: Reading,
    pub verified: Verified,
    pub info: SourceInfo,
    /// Things the user should know although nothing failed (e.g. limits that couldn't be checked).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// `true` when something was sent to the device (`false` for reads and `--dry-run`).
    pub applied: bool,
}

/// Apply a request: check limits, set voltage, set current, switch output on (switching off comes
/// first), then read back and verify. With `dry_run`, only reads and checks.
pub async fn apply(
    src: &dyn VoltageSource,
    req: Request,
    limits: Limits,
    ratings: SourceInfo,
    dry_run: bool,
) -> Result<Report> {
    validate_values(&req)?;
    let info = src.info().await?.stricter(ratings);
    check_limits(&req, &info, &limits)?;

    let mut warnings = Vec::new();
    if req.output == Some(true) {
        let now = src.read().await?;
        // A tripped protection means something went wrong (a short, an overload). Switching on
        // would bring it back, and on many devices "output on" also clears the trip silently.
        if let Some(p) = now.protection.filter(|p| p.tripped()) {
            if !req.clear_protection {
                return Err(DeskError::ProtectionTripped(p.name().to_string()));
            }
            warnings.push(format!("{} was tripped; switching on clears it", p.name()));
        }
        // Switching on with setpoints we didn't set: make sure those are within limits too.
        if limits.voltage.is_some() || limits.current.is_some() {
            let effective = Request {
                voltage: req.voltage.or(now.voltage_set),
                current: req.current.or(now.current_set),
                ..Request::default()
            };
            check_limits(&effective, &info, &limits)?;
            if effective.voltage.is_none() || effective.current.is_none() {
                warnings.push(
                    "the device doesn't report its setpoints, so the bench limits could only be \
                     checked for values set in this command"
                        .into(),
                );
            }
        }
    }

    if dry_run {
        let reading = src.read().await?;
        protection_warning(&reading, &mut warnings);
        return Ok(Report {
            requested: req,
            reading,
            verified: Verified::default(),
            info,
            warnings,
            applied: false,
        });
    }

    // Switching off comes first: a new setpoint must never reach a live output on its way off.
    if req.output == Some(false) {
        src.set_output(false).await?;
    }
    if let Some(v) = req.voltage {
        src.set_voltage(v).await?;
    }
    if let Some(a) = req.current {
        src.set_current(a).await?;
    }
    if req.output == Some(true) {
        // New setpoints go live with the output, so the device must have taken them first: a
        // write it dropped would switch on at the setpoint it still holds, which nothing checked.
        if req.voltage.is_some() || req.current.is_some() {
            let held = src.read().await?;
            let setpoints = Request {
                output: None,
                ..req
            };
            verify(&setpoints, &held, &info).map_err(not_switched_on)?;
        }
        src.set_output(true).await?;
    }

    let reading = src.read().await?;
    // Switched on, and a protection tripped right away (the cause is still there).
    if req.output == Some(true)
        && let Some(p) = reading.protection.filter(|p| p.tripped())
    {
        return Err(DeskError::ProtectionTripped(p.name().to_string()));
    }
    let verified = verify(&req, &reading, &info)?;
    protection_warning(&reading, &mut warnings);
    Ok(Report {
        requested: req,
        reading,
        verified,
        info,
        warnings,
        applied: !req.is_empty(),
    })
}

/// Compare a request with what the device reports. The first value it reports differently is
/// `verify_failed`; what it doesn't report stays unconfirmed (`None`).
fn verify(req: &Request, reading: &Reading, info: &SourceInfo) -> Result<Verified> {
    let verified = Verified {
        voltage: req.voltage.and_then(|v| {
            reading
                .voltage_set
                .map(|r| close(v, r, info.voltage_tolerance))
        }),
        current: req.current.and_then(|a| {
            reading
                .current_set
                .map(|r| close(a, r, info.current_tolerance))
        }),
        output: req.output.and_then(|o| reading.output.map(|r| r == o)),
    };
    for (what, ok, want, got) in [
        (
            "voltage",
            verified.voltage,
            req.voltage.map(fmt_v),
            reading.voltage_set.map(fmt_v),
        ),
        (
            "current",
            verified.current,
            req.current.map(fmt_a),
            reading.current_set.map(fmt_a),
        ),
        (
            "output",
            verified.output,
            req.output.map(on_off),
            reading.output.map(on_off),
        ),
    ] {
        if ok == Some(false) {
            return Err(DeskError::VerifyFailed {
                expected: format!("{what} {}", want.unwrap_or_default()),
                actual: got.unwrap_or_default(),
            });
        }
    }
    Ok(verified)
}

/// A setpoint the device didn't take, found before switching on: say that the output is still off.
fn not_switched_on(e: DeskError) -> DeskError {
    match e {
        DeskError::VerifyFailed { expected, actual } => DeskError::VerifyFailed {
            expected,
            actual: format!("{actual} (the output was not switched on)"),
        },
        other => other,
    }
}

/// Read the device (one query: no `info()`), with the warning for a tripped protection.
pub async fn read(src: &dyn VoltageSource, warnings: &mut Vec<String>) -> Result<Reading> {
    let reading = src.read().await?;
    protection_warning(&reading, warnings);
    Ok(reading)
}

fn protection_warning(reading: &Reading, warnings: &mut Vec<String>) {
    if let Some(p) = reading.protection.filter(|p| p.tripped()) {
        warnings.push(format!(
            "{} tripped: the device switched its output off. Check the circuit; switching on \
             again needs --clear-protection",
            p.name()
        ));
    }
}

fn validate_values(req: &Request) -> Result<()> {
    for (what, v) in [("voltage", req.voltage), ("current", req.current)] {
        if let Some(v) = v
            && !(v.is_finite() && v >= 0.0)
        {
            return Err(DeskError::Usage(format!(
                "{what} must be a non-negative number, got {v}"
            )));
        }
    }
    Ok(())
}

fn check_limits(req: &Request, info: &SourceInfo, limits: &Limits) -> Result<()> {
    let checks = [
        (
            "voltage",
            req.voltage,
            info.max_voltage,
            "device maximum",
            fmt_v as fn(f64) -> String,
        ),
        ("voltage", req.voltage, limits.voltage, "bench limit", fmt_v),
        (
            "current",
            req.current,
            info.max_current,
            "device maximum",
            fmt_a,
        ),
        ("current", req.current, limits.current, "bench limit", fmt_a),
    ];
    for (what, value, limit, kind, fmt) in checks {
        if let (Some(value), Some(limit)) = (value, limit)
            && value > limit + 1e-9
        {
            return Err(DeskError::LimitExceeded {
                what: what.to_string(),
                requested: fmt(value),
                limit: fmt(limit),
                kind: kind.to_string(),
                below: false,
            });
        }
    }
    Ok(())
}

/// Whether a read-back setpoint confirms the requested one: within the tolerance the model or
/// the driver states, or without one within the default.
fn close(want: f64, got: f64, tolerance: Option<f64>) -> bool {
    match tolerance {
        Some(t) => (want - got).abs() <= t + 1e-12,
        None => (want - got).abs() <= (want.abs() * TOLERANCE_REL).max(TOLERANCE_ABS),
    }
}

pub fn fmt_v(v: f64) -> String {
    format!("{v:.2} V")
}

pub fn fmt_a(a: f64) -> String {
    format!("{a:.3} A")
}

fn on_off(b: bool) -> String {
    if b { "on" } else { "off" }.to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Fake {
        state: Mutex<Reading>,
        calls: Mutex<Vec<String>>,
        readback: bool,
        ignore_voltage: bool,
        info: SourceInfo,
    }

    impl Fake {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn log(&self, s: impl Into<String>) {
            self.calls.lock().unwrap().push(s.into());
        }
    }

    #[async_trait]
    impl VoltageSource for Fake {
        async fn open(&self) -> Result<()> {
            self.log("open");
            Ok(())
        }
        async fn close(&self) -> Result<()> {
            self.log("close");
            Ok(())
        }
        async fn info(&self) -> Result<SourceInfo> {
            Ok(self.info)
        }
        async fn set_voltage(&self, v: f64) -> Result<()> {
            self.log(format!("v={v}"));
            if !self.ignore_voltage {
                self.state.lock().unwrap().voltage_set = Some(v);
            }
            Ok(())
        }
        async fn set_current(&self, a: f64) -> Result<()> {
            self.log(format!("a={a}"));
            self.state.lock().unwrap().current_set = Some(a);
            Ok(())
        }
        async fn set_output(&self, on: bool) -> Result<()> {
            self.log(format!("out={on}"));
            self.state.lock().unwrap().output = Some(on);
            Ok(())
        }
        async fn read(&self) -> Result<Reading> {
            let s = *self.state.lock().unwrap();
            Ok(if self.readback {
                s
            } else {
                Reading {
                    voltage: s.voltage_set,
                    ..Reading::default()
                }
            })
        }
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(f)
    }

    fn req(v: Option<f64>, a: Option<f64>, out: Option<bool>) -> Request {
        Request {
            voltage: v,
            current: a,
            output: out,
            ..Request::default()
        }
    }

    #[test]
    fn setpoints_before_output_and_verified() {
        let f = Fake {
            readback: true,
            ..Fake::default()
        };
        let r = run(apply(
            &f,
            req(Some(5.0), Some(1.0), Some(true)),
            Limits::default(),
            SourceInfo::default(),
            false,
        ))
        .unwrap();
        assert_eq!(f.calls(), ["v=5", "a=1", "out=true"]);
        assert!(r.verified.all());
        assert_eq!(r.verified.voltage, Some(true));
    }

    #[test]
    fn output_off_comes_before_setpoints() {
        let f = Fake {
            readback: true,
            ..Fake::default()
        };
        let r = run(apply(
            &f,
            req(Some(12.0), Some(1.0), Some(false)),
            Limits::default(),
            SourceInfo::default(),
            false,
        ))
        .unwrap();
        assert_eq!(f.calls(), ["out=false", "v=12", "a=1"]);
        assert!(r.verified.all());
    }

    #[test]
    fn unreadable_setpoints_are_unverified_not_confirmed() {
        let f = Fake::default();
        let r = run(apply(
            &f,
            req(Some(5.0), None, None),
            Limits::default(),
            SourceInfo::default(),
            false,
        ))
        .unwrap();
        assert_eq!(r.verified.voltage, None);
    }

    #[test]
    fn wrong_readback_fails() {
        let f = Fake {
            readback: true,
            ignore_voltage: true,
            ..Fake::default()
        };
        // The device keeps reporting its old setpoint.
        f.state.lock().unwrap().voltage_set = Some(1.0);
        let e = run(apply(
            &f,
            req(Some(5.0), None, None),
            Limits::default(),
            SourceInfo::default(),
            false,
        ))
        .unwrap_err();
        assert_eq!(e.code(), "verify_failed");
    }

    #[test]
    fn a_stated_tolerance_decides_what_confirmed_means() {
        let ask = |info: SourceInfo| {
            let f = Fake {
                readback: true,
                ignore_voltage: true,
                info,
                ..Fake::default()
            };
            // The device stays at 15 mV when 5 mV is asked for.
            f.state.lock().unwrap().voltage_set = Some(0.015);
            run(apply(
                &f,
                req(Some(0.005), None, None),
                Limits::default(),
                SourceInfo::default(),
                false,
            ))
        };
        // Without one, the default applies (0.5 % or 0.011): that counts as confirmed.
        let r = ask(SourceInfo::default()).unwrap();
        assert_eq!(r.verified.voltage, Some(true));
        // A model that states what its device resolves gets an honest answer.
        let e = ask(SourceInfo {
            voltage_tolerance: Some(0.001),
            ..SourceInfo::default()
        })
        .unwrap_err();
        assert_eq!(e.code(), "verify_failed");
    }

    #[test]
    fn the_stricter_tolerance_wins() {
        let a = SourceInfo {
            voltage_tolerance: Some(0.01),
            ..SourceInfo::default()
        };
        let b = SourceInfo {
            voltage_tolerance: Some(0.002),
            current_tolerance: Some(0.001),
            ..SourceInfo::default()
        };
        let s = a.stricter(b);
        assert_eq!(s.voltage_tolerance, Some(0.002));
        assert_eq!(s.current_tolerance, Some(0.001));
    }

    #[test]
    fn a_dropped_setpoint_never_reaches_a_live_output() {
        let f = Fake {
            readback: true,
            ignore_voltage: true,
            ..Fake::default()
        };
        // The device still holds an old, higher setpoint and drops the new one.
        f.state.lock().unwrap().voltage_set = Some(24.0);
        let e = run(apply(
            &f,
            req(Some(5.0), Some(1.0), Some(true)),
            Limits::default(),
            SourceInfo::default(),
            false,
        ))
        .unwrap_err();
        assert_eq!(e.code(), "verify_failed");
        assert!(e.to_string().contains("not switched on"), "{e}");
        assert_eq!(f.calls(), ["v=5", "a=1"], "the output is never switched");
        assert_eq!(f.state.lock().unwrap().output, None);
    }

    #[test]
    fn limits_are_checked_before_anything_is_sent() {
        let f = Fake {
            info: SourceInfo {
                max_voltage: Some(30.0),
                max_current: Some(5.0),
                ..SourceInfo::default()
            },
            ..Fake::default()
        };
        let bench = Limits {
            voltage: Some(12.0),
            current: None,
        };
        let e = run(apply(
            &f,
            req(Some(13.0), None, None),
            bench,
            SourceInfo::default(),
            false,
        ))
        .unwrap_err();
        assert_eq!(e.code(), "limit_exceeded");
        assert!(e.to_string().contains("bench limit"), "{e}");
        let e = run(apply(
            &f,
            req(None, Some(6.0), None),
            bench,
            SourceInfo::default(),
            false,
        ))
        .unwrap_err();
        assert!(e.to_string().contains("device maximum"), "{e}");
        assert!(f.calls().is_empty(), "nothing sent: {:?}", f.calls());
        // Exactly at the limit is fine.
        run(apply(
            &f,
            req(Some(12.0), None, None),
            bench,
            SourceInfo::default(),
            false,
        ))
        .unwrap();
    }

    #[test]
    fn output_on_checks_existing_setpoints_against_limits() {
        let f = Fake {
            readback: true,
            ..Fake::default()
        };
        f.state.lock().unwrap().voltage_set = Some(24.0);
        let bench = Limits {
            voltage: Some(12.0),
            current: None,
        };
        let e = run(apply(
            &f,
            req(None, None, Some(true)),
            bench,
            SourceInfo::default(),
            false,
        ))
        .unwrap_err();
        assert_eq!(e.code(), "limit_exceeded");
        assert!(!f.calls().contains(&"out=true".to_string()));
    }

    #[test]
    fn rejects_negative_and_nan() {
        let f = Fake::default();
        for v in [-1.0, f64::NAN, f64::INFINITY] {
            let e = run(apply(
                &f,
                req(Some(v), None, None),
                Limits::default(),
                SourceInfo::default(),
                false,
            ))
            .unwrap_err();
            assert_eq!(e.code(), "usage");
        }
    }

    #[test]
    fn dry_run_sends_nothing() {
        let f = Fake {
            readback: true,
            ..Fake::default()
        };
        let r = run(apply(
            &f,
            req(Some(5.0), Some(1.0), Some(true)),
            Limits::default(),
            SourceInfo::default(),
            true,
        ))
        .unwrap();
        assert!(!r.applied);
        assert!(f.calls().is_empty());
    }

    #[test]
    fn non_finite_fields_are_named() {
        assert_eq!(Reading::default().non_finite(), None);
        let r = Reading {
            current_set: Some(f64::NAN),
            ..Reading::default()
        };
        assert_eq!(r.non_finite(), Some("current_set"));
        let i = SourceInfo {
            max_voltage: Some(f64::INFINITY),
            ..SourceInfo::default()
        };
        assert_eq!(i.non_finite(), Some("max_voltage"));
    }
}
