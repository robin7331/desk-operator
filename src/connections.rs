//! Connections: what a script binding may open to reach its device. One field per connection
//! kind (a serial `port`, a `sigrok` device); its host API lives in `src/script/<kind>.rs`, and a
//! backend it runs through is a provider whose endpoints are used as this connection key.

use serde::Serialize;

use crate::serial::PortSpec;
use crate::sigrok::SigrokSpec;

/// Every connection a script binding has. Serialised flat into the binding (`port`, `sigrok`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Connections {
    /// The only serial port this script may open (`ctx.serial.open`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<PortSpec>,
    /// The only sigrok device this script may capture from (`ctx.sigrok`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sigrok: Option<Box<SigrokSpec>>,
}

/// A provider a connection runs through, and the provider kind it must have.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BackendRef<'a> {
    pub provider: &'a str,
    pub kind: &'static str,
}

impl Connections {
    /// The binding keys of the connections, in order.
    pub const KEYS: &[&str] = &["port", "sigrok"];

    /// Take the connection keys out of a script binding's table.
    pub fn take(t: &mut toml::Table) -> Result<Connections, String> {
        let port = match t.remove("port") {
            None => None,
            Some(toml::Value::Table(p)) => Some(PortSpec::try_from(p)?),
            Some(_) => {
                return Err(
                    "`port` must be a table, e.g. { usb_vid = 0x1a86, usb_pid = 0x7523 }".into(),
                );
            }
        };
        let sigrok = match t.remove("sigrok") {
            None => None,
            Some(toml::Value::Table(s)) => Some(Box::new(SigrokSpec::try_from(s)?)),
            Some(_) => {
                return Err("`sigrok` must be a table, e.g. { driver = \"demo\" }".into());
            }
        };
        Ok(Connections { port, sigrok })
    }

    /// What `Binding::describe` adds after the script, e.g. ` on <port> with sigrok demo`.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        if let Some(port) = &self.port {
            s.push_str(&format!(" on {port}"));
        }
        if let Some(sigrok) = &self.sigrok {
            s.push_str(&format!(" with {sigrok}"));
        }
        s
    }

    /// Check the connections of a model's binding table: only what is true for every unit. A
    /// serial `port`'s per-unit keys are left to the model schema.
    pub fn check_model(binding: &toml::Table) -> Result<(), String> {
        if let Some(t) = binding.get("sigrok") {
            crate::sigrok::check_model_table(t)?;
        }
        Ok(())
    }

    /// The providers these connections run through, each with the kind it must have.
    pub fn backends(&self) -> Vec<BackendRef<'_>> {
        let mut out = Vec::new();
        if let Some(b) = self.sigrok.as_ref().and_then(|s| s.backend.as_deref()) {
            out.push(BackendRef {
                provider: b,
                kind: crate::sigrok::KIND,
            });
        }
        out
    }

    /// Whether these connections run through provider `id`.
    pub fn uses(&self, id: &str) -> bool {
        self.backends().iter().any(|b| b.provider == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> toml::Table {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn takes_only_its_keys_and_names_its_backends() {
        let mut table = t(r#"
            script = "x.lua"
            port = { usb_vid = 1, usb_pid = 2 }
            sigrok = { driver = "demo", backend = "vendor" }
        "#);
        let c = Connections::take(&mut table).unwrap();
        assert_eq!(table.keys().collect::<Vec<_>>(), ["script"]);
        assert_eq!(
            c.backends(),
            [BackendRef {
                provider: "vendor",
                kind: crate::sigrok::KIND
            }]
        );
        assert!(c.uses("vendor") && !c.uses("sigrok"));
        assert!(
            c.describe().ends_with(" with sigrok demo via vendor"),
            "{}",
            c.describe()
        );

        let none = Connections::take(&mut t("script = \"x.lua\"")).unwrap();
        assert_eq!(none, Connections::default());
        assert_eq!(none.describe(), "");
        assert!(none.backends().is_empty());
        assert!(Connections::take(&mut t("port = 1")).is_err());
        assert!(Connections::take(&mut t("sigrok = \"demo\"")).is_err());
    }

    #[test]
    fn a_model_gives_only_what_every_unit_shares() {
        assert!(Connections::check_model(&t("sigrok = { driver = \"demo\" }")).is_ok());
        let e = Connections::check_model(&t("sigrok = { driver = \"demo\", conn = \"1.2\" }"));
        assert!(e.unwrap_err().contains("per bench"));
    }
}
