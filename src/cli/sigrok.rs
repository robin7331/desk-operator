//! `desk sigrok drivers`: each sigrok backend with its drivers, to fill in a binding's
//! `sigrok = { driver = … }`. `desk sigrok show`: sigrok's own `--show` text for a driver, unparsed
//! (for writing a model, like `desk port probe`).
//!
//! These run in this process, not through the daemon. `show` opens the device: while desk
//! captures from it, sigrok reports it busy.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::Serialize;

use super::SigrokCmd;
use crate::config::{Config, ProviderConfig};
use crate::error::{DeskError, ErrorJson, Result};
use crate::output::{Report, table};
use crate::sigrok::{self, Backend, DriverInfo, SigrokSpec};

/// The configured providers and the folder they resolve against. Without a config file, only
/// the implicit backend exists; a config that is there must be valid.
fn providers(config: Option<&Path>) -> Result<(IndexMap<String, ProviderConfig>, PathBuf)> {
    let path = match config {
        Some(p) => Some(p.to_path_buf()),
        None => crate::config::default_path(),
    };
    match path {
        Some(p) if p.exists() => {
            let c = Config::load(Some(&p))?;
            let dir = c.path.parent().map(Path::to_path_buf).unwrap_or_default();
            Ok((c.providers, dir))
        }
        _ => Ok((IndexMap::new(), PathBuf::new())),
    }
}

#[derive(Serialize)]
struct BackendView {
    id: String,
    program: PathBuf,
    implicit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    drivers: Vec<DriverInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<ErrorJson>,
}

pub async fn run(config: Option<&Path>, cmd: SigrokCmd) -> Result<Report> {
    let (configs, base) = providers(config)?;
    match cmd {
        SigrokCmd::Drivers { backend } => drivers(&configs, &base, backend.as_deref()).await,
        SigrokCmd::Show {
            driver,
            conn,
            backend,
        } => show(&configs, &base, driver, conn, backend).await,
    }
}

async fn view(b: Backend) -> (BackendView, Option<DeskError>) {
    let mut v = BackendView {
        id: b.id.clone(),
        program: b.program.clone(),
        implicit: b.implicit,
        version: None,
        drivers: Vec::new(),
        error: None,
    };
    let result = async {
        let version = b.version().await?;
        let drivers = b.drivers().await?;
        Ok::<_, DeskError>((version, drivers))
    }
    .await;
    match result {
        Ok((version, drivers)) => {
            v.version = Some(version);
            v.drivers = drivers.to_vec();
            (v, None)
        }
        Err(e) => {
            v.error = Some(e.to_json());
            (v, Some(e))
        }
    }
}

async fn drivers(
    configs: &IndexMap<String, ProviderConfig>,
    base: &Path,
    only: Option<&str>,
) -> Result<Report> {
    let mut views = Vec::new();
    match only {
        // Asked for by name: its failure is the command's.
        Some(id) => {
            let (v, e) = view(sigrok::backend(configs, base, id)?).await;
            if let Some(e) = e {
                return Err(e);
            }
            views.push(v);
        }
        None => {
            for b in sigrok::backends(configs, base, true) {
                match b {
                    Ok(b) => views.push(view(b).await.0),
                    Err(e) => return Err(e),
                }
            }
        }
    }
    let mut human = String::new();
    for v in &views {
        human.push_str(&format!(
            "{}{}: {}",
            v.id,
            if v.implicit { " (implicit)" } else { "" },
            v.program.display()
        ));
        match (&v.error, &v.version) {
            (Some(e), _) => {
                human.push_str(&format!("\n  error: {}\n", e.message));
                if let Some(h) = &e.hint {
                    human.push_str(&format!("  hint: {h}\n"));
                }
            }
            (None, version) => {
                human.push_str(&format!(
                    ", {}, {}\n",
                    version.as_deref().unwrap_or("?"),
                    sigrok::drivers_text(v.drivers.len())
                ));
                let rows: Vec<Vec<String>> = v
                    .drivers
                    .iter()
                    .map(|d| vec![format!("  {}", d.id), d.description.clone()])
                    .collect();
                human.push_str(&table(&["  DRIVER", "DESCRIPTION"], &rows));
            }
        }
        human.push('\n');
    }
    human.push_str(
        "bind one in a script binding with: sigrok = { driver = \"<DRIVER>\" }\n\
         find connected devices with: desk provider endpoints <backend>\n",
    );
    Ok(Report::new(
        &serde_json::json!({ "backends": views }),
        human,
    ))
}

async fn show(
    configs: &IndexMap<String, ProviderConfig>,
    base: &Path,
    driver: String,
    conn: Option<String>,
    backend: Option<String>,
) -> Result<Report> {
    if !sigrok::valid_driver(&driver) {
        return Err(DeskError::Usage(format!(
            "`{driver}` is not a sigrok driver id (lowercase letters, digits, `-`, `_`; `desk \
             sigrok drivers` lists them)"
        )));
    }
    if let Some(c) = &conn
        && !sigrok::valid_conn(c)
    {
        return Err(DeskError::Usage(format!(
            "--conn `{c}` can't contain spaces or control characters"
        )));
    }
    let spec = SigrokSpec {
        driver: driver.clone(),
        build: None,
        conn: conn.clone(),
        backend,
    };
    let b = sigrok::resolve(configs, base, &spec).await?;
    let text = b.show(&driver, conn.as_deref()).await?;
    let human = if text.ends_with('\n') {
        text.clone()
    } else {
        format!("{text}\n")
    };
    Ok(Report::new(
        &serde_json::json!({ "backend": b.id, "driver": driver, "conn": conn, "text": text }),
        human,
    ))
}
