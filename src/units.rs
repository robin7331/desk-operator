//! The unit table: which units desk knows, how they may be typed, and parsing `330c`, `250mA`,
//! `1.5kΩ` into a number in the declared unit.
//!
//! Prefixes are case-sensitive (`m` milli, `M` mega), unit symbols are not (`5V` = `5v`). A bare
//! number is only accepted for unitless values, so a unit is never guessed. Units the table
//! doesn't know may still be declared: they must be typed as declared (any case), without prefixes.

/// A known unit: its symbol (what models declare and output shows), how it may be typed
/// (lowercase), and whether SI prefixes apply.
struct Known {
    symbol: &'static str,
    typed: &'static [&'static str],
    prefixes: bool,
    /// Extra spellings with a factor, e.g. minutes for seconds.
    scaled: &'static [(&'static str, f64)],
}

const KNOWN: &[Known] = &[
    Known {
        symbol: "V",
        typed: &["v"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "A",
        typed: &["a"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "W",
        typed: &["w"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "Ω",
        typed: &["ω", "ohm", "ohms", "r"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "Hz",
        typed: &["hz"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "F",
        typed: &["f"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "H",
        typed: &["h"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "g",
        typed: &["g"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "Pa",
        typed: &["pa"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "s",
        typed: &["s"],
        prefixes: true,
        scaled: &[("min", 60.0), ("h", 3600.0)],
    },
    Known {
        symbol: "°C",
        typed: &["°c", "c", "degc"],
        prefixes: false,
        scaled: &[],
    },
    Known {
        symbol: "K",
        typed: &["k"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "%",
        typed: &["%"],
        prefixes: false,
        scaled: &[],
    },
    Known {
        symbol: "Ah",
        typed: &["ah"],
        prefixes: true,
        scaled: &[],
    },
    Known {
        symbol: "Wh",
        typed: &["wh"],
        prefixes: true,
        scaled: &[],
    },
];

const PREFIXES: &[(&str, f64)] = &[
    ("p", 1e-12),
    ("n", 1e-9),
    ("u", 1e-6),
    ("µ", 1e-6),
    ("μ", 1e-6),
    ("m", 1e-3),
    ("k", 1e3),
    ("K", 1e3),
    ("M", 1e6),
    ("G", 1e9),
];

fn known(symbol: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.symbol == symbol)
}

/// Whether `symbol` is a known unit's symbol.
pub fn is_known(symbol: &str) -> bool {
    known(symbol).is_some()
}

/// A declared unit that is a known unit's *typed* spelling instead of its symbol (`"C"` for
/// `"°C"`): returns the symbol to use instead.
pub fn misspelled(declared: &str) -> Option<&'static str> {
    if is_known(declared) {
        return None;
    }
    let lower = declared.to_lowercase();
    KNOWN
        .iter()
        .find(|k| k.typed.contains(&lower.as_str()) || k.symbol.to_lowercase() == lower)
        .map(|k| k.symbol)
}

/// Split `"330c"` into `(330.0, "c")`. The number may have a sign, decimals and an exponent.
fn split(input: &str) -> Option<(f64, &str)> {
    let s = input.trim();
    let bytes = s.as_bytes();
    let mut end = 0;
    if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
        end += 1;
    }
    let digits_start = end;
    while end < bytes.len() && (bytes[end].is_ascii_digit() || bytes[end] == b'.') {
        end += 1;
    }
    if end == digits_start {
        return None;
    }
    // An exponent only when digits follow (`1e3`), so `5e` isn't eaten.
    if end + 1 < bytes.len() && (bytes[end] == b'e' || bytes[end] == b'E') {
        let mut e = end + 1;
        if e < bytes.len() && (bytes[e] == b'-' || bytes[e] == b'+') {
            e += 1;
        }
        if e < bytes.len() && bytes[e].is_ascii_digit() {
            while e < bytes.len() && bytes[e].is_ascii_digit() {
                e += 1;
            }
            end = e;
        }
    }
    let number: f64 = s[..end].parse().ok()?;
    Some((number, s[end..].trim()))
}

/// Parse `input` as a number in `unit` (a declared symbol; `None` = unitless). Returns the value
/// in that unit, or a message saying what's expected.
pub fn parse(input: &str, unit: Option<&str>) -> Result<f64, String> {
    let example = |u: &str| match known(u) {
        Some(k) => format!("e.g. 5{}", k.typed[0]),
        None => format!("e.g. 5{u}"),
    };
    let (number, suffix) = split(input).ok_or_else(|| match unit {
        Some(u) => format!("`{input}` is not a number with a unit ({})", example(u)),
        None => format!("`{input}` is not a number"),
    })?;
    if !number.is_finite() {
        return Err(format!("`{input}` is not a finite number"));
    }
    let Some(unit) = unit else {
        return if suffix.is_empty() {
            Ok(number)
        } else {
            Err(format!(
                "`{input}`: this value has no unit, give a plain number"
            ))
        };
    };
    if suffix.is_empty() {
        return Err(format!("`{input}` needs a unit: {}", example(unit)));
    }
    let value = factor(suffix, unit).map(|f| number * f).ok_or_else(|| {
        format!(
            "`{input}`: `{suffix}` is not a unit for {unit} ({})",
            example(unit)
        )
    })?;
    if !value.is_finite() {
        return Err(format!("`{input}` is not a finite number"));
    }
    Ok(value)
}

/// The factor that turns a typed suffix into the declared unit, if it is a spelling of it.
fn factor(suffix: &str, unit: &str) -> Option<f64> {
    let lower = suffix.to_lowercase();
    let Some(k) = known(unit) else {
        // Unknown units: exactly as declared (any case), no prefixes.
        return (lower == unit.to_lowercase()).then_some(1.0);
    };
    // The unit itself wins over a prefix reading (`k` is kelvin, `h` hours or henry).
    if k.typed.contains(&lower.as_str()) || lower == k.symbol.to_lowercase() {
        return Some(1.0);
    }
    if let Some((_, f)) = k.scaled.iter().find(|(s, _)| *s == lower) {
        return Some(*f);
    }
    if !k.prefixes {
        return None;
    }
    // Prefixes are case-sensitive: compare the prefix as typed, the rest case-insensitively.
    PREFIXES.iter().find_map(|(p, f)| {
        let rest = suffix.strip_prefix(p)?.to_lowercase();
        (k.typed.contains(&rest.as_str()) || rest == k.symbol.to_lowercase()).then_some(*f)
    })
}

/// A unit as an instrument prints it (`mV`, `kΩ`, `µA`, `°C`): the known unit's symbol and the
/// power of ten its prefix stands for (`mV` → `("V", -3)`). Symbols and prefixes are matched as
/// printed (case-sensitive); the ohm sign U+2126 counts as `Ω`. `None` for units the table
/// doesn't know.
pub fn printed(text: &str) -> Option<(&'static str, i32)> {
    let text = text.replace('\u{2126}', "Ω");
    if let Some(k) = known(&text) {
        return Some((k.symbol, 0));
    }
    PREFIXES.iter().find_map(|(p, f)| {
        let k = known(text.strip_prefix(p)?)?;
        k.prefixes.then(|| (k.symbol, f.log10().round() as i32))
    })
}

/// A value for humans: `4.98 V`, `330 °C`, `0.25 A`. Up to 6 significant digits.
pub fn format(value: f64, unit: Option<&str>) -> String {
    let n = format!("{value:.6}");
    let n = n.trim_end_matches('0').trim_end_matches('.');
    let n = if n == "-0" { "0" } else { n };
    match unit {
        Some("%") => format!("{n}%"),
        Some(u) => format!("{n} {u}"),
        None => n.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() <= 1e-12 * a.abs().max(b.abs()).max(1.0)
    }

    #[test]
    fn units_and_prefixes() {
        let ok = |s: &str, u: &str, want: f64| {
            let got = parse(s, Some(u)).unwrap_or_else(|e| panic!("{s}: {e}"));
            assert!(close(got, want), "{s} → {got}, want {want}");
        };
        ok("5v", "V", 5.0);
        ok("5V", "V", 5.0);
        ok("500mv", "V", 0.5);
        ok("250mA", "A", 0.25);
        ok("250ma", "A", 0.25);
        ok("1.5kΩ", "Ω", 1500.0);
        ok("1MΩ", "Ω", 1e6);
        ok("1mΩ", "Ω", 1e-3);
        ok("4.7kohm", "Ω", 4700.0);
        ok("10r", "Ω", 10.0);
        ok("330c", "°C", 330.0);
        ok("330°C", "°C", 330.0);
        ok("330 degc", "°C", 330.0);
        ok("250ms", "s", 0.25);
        ok("2min", "s", 120.0);
        ok("1h", "s", 3600.0);
        ok("10uH", "H", 1e-5);
        ok("1h", "H", 1.0);
        ok("300k", "K", 300.0);
        ok("2kk", "K", 2000.0);
        ok("50%", "%", 50.0);
        ok("-1.5v", "V", -1.5);
        ok("1e3hz", "Hz", 1000.0);
        ok("1.2Ah", "Ah", 1.2);
        // Mega, not milli: 500 megavolts (a range check refuses it).
        ok("500MV", "V", 5e8);
    }

    #[test]
    fn never_guesses() {
        assert!(parse("5", Some("V")).unwrap_err().contains("needs a unit"));
        assert!(
            parse("5a", Some("V"))
                .unwrap_err()
                .contains("not a unit for V")
        );
        assert!(parse("5mc", Some("°C")).is_err(), "°C takes no prefixes");
        assert!(parse("five volts", Some("V")).is_err());
        assert!(parse("5v", None).unwrap_err().contains("no unit"));
        assert_eq!(parse("42", None).unwrap(), 42.0);
        assert!(parse("1e999v", Some("V")).is_err(), "not finite");
        assert!(
            parse("1e308kv", Some("V")).is_err(),
            "not finite after the prefix"
        );
    }

    #[test]
    fn unknown_units_are_exact() {
        assert_eq!(parse("400ppm", Some("ppm")).unwrap(), 400.0);
        assert_eq!(parse("400PPM", Some("ppm")).unwrap(), 400.0);
        assert!(parse("4kppm", Some("ppm")).is_err());
        assert_eq!(parse("55 %RH", Some("%RH")).unwrap(), 55.0);
    }

    #[test]
    fn declared_symbols() {
        assert_eq!(misspelled("C"), Some("°C"));
        assert_eq!(misspelled("ohm"), Some("Ω"));
        assert_eq!(misspelled("v"), Some("V"));
        assert_eq!(misspelled("V"), None);
        assert_eq!(misspelled("ppm"), None);
    }

    #[test]
    fn printed_units() {
        assert_eq!(printed("V"), Some(("V", 0)));
        assert_eq!(printed("mV"), Some(("V", -3)));
        assert_eq!(printed("kΩ"), Some(("Ω", 3)));
        assert_eq!(printed("M\u{2126}"), Some(("Ω", 6)));
        assert_eq!(printed("µA"), Some(("A", -6)));
        assert_eq!(printed("uA"), Some(("A", -6)));
        assert_eq!(printed("kHz"), Some(("Hz", 3)));
        assert_eq!(printed("°C"), Some(("°C", 0)));
        assert_eq!(printed("K"), Some(("K", 0)), "kelvin, not a prefix");
        assert_eq!(printed("%"), Some(("%", 0)));
        assert_eq!(printed("mAh"), Some(("Ah", -3)));
        assert_eq!(printed("m°C"), None, "°C takes no prefixes");
        assert_eq!(printed("mv"), None, "as printed");
        assert_eq!(printed("°F"), None);
        assert_eq!(printed("dBm"), None);
        assert_eq!(printed(""), None);
    }

    #[test]
    fn formats() {
        assert_eq!(format(4.98, Some("V")), "4.98 V");
        assert_eq!(format(330.0, Some("°C")), "330 °C");
        assert_eq!(format(50.0, Some("%")), "50%");
        assert_eq!(format(0.000_001, None), "0.000001");
        assert_eq!(format(-0.0, Some("A")), "0 A");
    }
}
