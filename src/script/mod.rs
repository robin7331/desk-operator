//! Lua scripting (mlua, Lua 5.4): the shared engine behind every script binding.
//!
//! * [`power::ScriptSwitch`]: a `power = { script = … }` binding (`on`, `off`, `state`)
//! * [`source::ScriptSource`]: a `source = { script = … }` driver (`set_voltage`, `read`, …)
//!
//! Every function gets `ctx` as its first argument. A call succeeds when it returns; `error(...)`
//! fails it. The host API (`ctx`) is documented in `docs/scripting.md` and is the plugin contract,
//! so keep it small and stable.
//!
//! Sandbox: only the `table`, `string`, `math`, `utf8` and `coroutine` libraries are loaded, and
//! `dofile`/`loadfile`/`string.dump` are removed, so there is no file, OS or network access except
//! through `ctx`. `load` only takes source text (binary chunks are unverified bytecode), and each
//! state has a memory limit.
//! `print` goes to the log. A global instruction hook enforces the timeout even in busy loops.

pub mod declared;
pub mod power;
pub mod source;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mlua::{
    Function, HookTriggers, IntoLua, Lua, LuaOptions, LuaSerdeExt, MultiValue, StdLib, Table,
    Value, VmState,
};

use crate::capture::Output;
use crate::config::Binding;
use crate::deadline::{self, Deadline};
use crate::error::{DeskError, Result};
use crate::power::{self as power_core, PowerSwitch, Target};
use crate::providers::Registry;
use crate::serial::{Port, PortSpec, Settings};
use crate::sigrok::{self, Acquire, Length, SigrokSpec};

pub use declared::ScriptDriver;
pub use power::ScriptSwitch;
pub use source::ScriptSource;

/// How often (in VM instructions) the timeout hook runs.
const HOOK_EVERY: u32 = 10_000;

/// Memory one script may use. Drivers need kilobytes; this stops a runaway allocation.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// The most one `port:read` returns. A read returns "up to" its count, so more is just asked again.
const MAX_READ: usize = 64 * 1024;

/// Device facts passed to the script as `ctx.device`.
pub struct DeviceInfo<'a> {
    pub id: &'a str,
    pub name: &'a str,
}

/// Everything a script binding needs to load.
pub struct ScriptSpec<'a> {
    pub device: DeviceInfo<'a>,
    pub path: &'a Path,
    pub params: &'a toml::Table,
    /// The binding's port, the only one `ctx.serial.open` may open.
    pub port: Option<&'a PortSpec>,
    /// The binding's sigrok device, the only one `ctx.sigrok.acquire` may capture from.
    pub sigrok: Option<&'a SigrokSpec>,
    pub timeout: Duration,
    pub providers: Arc<Registry>,
}

/// A loaded, sandboxed script with its `ctx`.
pub struct Engine {
    lua: Lua,
    ctx: Table,
    path: PathBuf,
    timeout: Duration,
    deadline: Deadline,
    log: Arc<Mutex<Vec<String>>>,
    params: Option<Vec<ParamSpec>>,
}

/// One entry of a driver's `PARAMS = { name = { type = …, required = …, default = …, doc = … } }`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ParamSpec {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

const PARAM_TYPES: &[&str] = &["string", "number", "integer", "boolean", "table", "array"];

fn toml_type_fits(kind: &str, v: &toml::Value) -> bool {
    matches!(
        (kind, v),
        ("string", toml::Value::String(_))
            | ("number", toml::Value::Float(_) | toml::Value::Integer(_))
            | ("integer", toml::Value::Integer(_))
            | ("boolean", toml::Value::Boolean(_))
            | ("table", toml::Value::Table(_))
            | ("array", toml::Value::Array(_))
    )
}

fn json_type_fits(kind: &str, v: &serde_json::Value) -> bool {
    matches!(
        (kind, v),
        ("string", serde_json::Value::String(_))
            | ("number", serde_json::Value::Number(_))
            | ("boolean", serde_json::Value::Bool(_))
            | ("table", serde_json::Value::Object(_))
            | ("array", serde_json::Value::Array(_))
    ) || (kind == "integer" && v.as_i64().is_some())
}

impl Engine {
    /// Create a sandboxed Lua state, install the host API and run the script's top level.
    pub fn load(spec: ScriptSpec) -> Result<Self> {
        let ScriptSpec {
            device,
            path,
            params,
            port,
            sigrok,
            timeout,
            providers,
        } = spec;
        let code = crate::models::read_text(path).map_err(|e| {
            DeskError::ConfigInvalid(format!(
                "device `{}`: cannot read script {}: {e}",
                device.id,
                path.display()
            ))
        })?;

        let lua = Lua::new_with(
            StdLib::TABLE | StdLib::STRING | StdLib::MATH | StdLib::UTF8 | StdLib::COROUTINE,
            LuaOptions::default(),
        )
        .map_err(internal)?;
        lua.set_memory_limit(MEMORY_LIMIT).map_err(internal)?;

        let log = Arc::new(Mutex::new(Vec::new()));
        let deadline = Deadline::default();
        deadline.start(timeout);
        install_timeout_hook(&lua, deadline.clone(), timeout, path)?;
        let ctx =
            host_api(&lua, &device, params, log.clone(), providers.clone()).map_err(internal)?;
        ctx.set("serial", serial_api(&lua, port.cloned()).map_err(internal)?)
            .map_err(internal)?;
        let api = sigrok_api(&lua, device.id, sigrok.cloned(), providers).map_err(internal)?;
        ctx.set("sigrok", api).map_err(internal)?;

        let engine = Engine {
            lua,
            ctx,
            path: path.to_path_buf(),
            timeout,
            deadline,
            log,
            params: None,
        };
        engine
            .lua
            .load(&code)
            .set_name(format!("@{}", file_name(path)))
            .exec()
            .map_err(|e| engine.error(e))?;
        engine.deadline.clear();
        let declared = engine.declared_params()?;
        if let Some(specs) = &declared {
            engine.check_params(specs, params)?;
        }
        Ok(Engine {
            params: declared,
            ..engine
        })
    }

    /// The driver's `PARAMS` declaration, if it has one.
    pub fn params(&self) -> Option<&[ParamSpec]> {
        self.params.as_deref()
    }

    fn declared_params(&self) -> Result<Option<Vec<ParamSpec>>> {
        let bad =
            |m: String| DeskError::ConfigInvalid(format!("{}: PARAMS: {m}", self.file_name()));
        let table = match self.lua.globals().get::<Value>("PARAMS") {
            Ok(Value::Nil) | Err(_) => return Ok(None),
            Ok(Value::Table(t)) => t,
            Ok(other) => return Err(bad(format!("must be a table, got {}", other.type_name()))),
        };
        let mut specs = Vec::new();
        for pair in table.pairs::<String, Table>() {
            let (name, spec) =
                pair.map_err(|_| bad("entries look like `name = { type = \"number\" }`".into()))?;
            let kind: String = spec
                .get("type")
                .map_err(|_| bad(format!("`{name}` needs `type = \"…\"`")))?;
            if !PARAM_TYPES.contains(&kind.as_str()) {
                return Err(bad(format!(
                    "`{name}`: unknown type `{kind}` (one of: {})",
                    PARAM_TYPES.join(", ")
                )));
            }
            let required: Option<bool> = spec.get("required").map_err(|e| bad(e.to_string()))?;
            let default: Value = spec.get("default").map_err(|e| bad(e.to_string()))?;
            let default = if default.is_nil() {
                None
            } else {
                let v: serde_json::Value = self
                    .lua
                    .from_value(default)
                    .map_err(|e| bad(e.to_string()))?;
                if !json_type_fits(&kind, &v) {
                    return Err(bad(format!(
                        "`{name}`: default doesn't match type `{kind}`"
                    )));
                }
                Some(v)
            };
            let doc: Option<String> = spec.get("doc").map_err(|e| bad(e.to_string()))?;
            specs.push(ParamSpec {
                name,
                kind,
                required: required.unwrap_or(false),
                default,
                doc,
            });
        }
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Some(specs))
    }

    /// Validate a binding's params against `PARAMS`, then fill in defaults.
    fn check_params(&self, specs: &[ParamSpec], given: &toml::Table) -> Result<()> {
        let bad =
            |m: String| DeskError::ConfigInvalid(format!("params for {}: {m}", self.file_name()));
        for (k, v) in given {
            let Some(spec) = specs.iter().find(|s| s.name == *k) else {
                let known: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
                return Err(bad(format!(
                    "unknown param `{k}` (the driver accepts: {})",
                    if known.is_empty() {
                        "none".to_string()
                    } else {
                        known.join(", ")
                    }
                )));
            };
            if !toml_type_fits(&spec.kind, v) {
                return Err(bad(format!("`{k}` must be a {}", spec.kind)));
            }
        }
        let params: Table = self.ctx.get("params").map_err(internal)?;
        for spec in specs {
            if given.contains_key(&spec.name) {
                continue;
            }
            match &spec.default {
                Some(d) => params
                    .set(spec.name.as_str(), self.lua.to_value(d).map_err(internal)?)
                    .map_err(internal)?,
                None if spec.required => {
                    return Err(bad(format!("missing required param `{}`", spec.name)));
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Whether the script defines a global function `name`.
    pub fn has(&self, name: &str) -> bool {
        matches!(
            self.lua.globals().get::<Value>(name),
            Ok(Value::Function(_))
        )
    }

    /// Fail with `config_invalid` unless every function in `names` is defined.
    pub fn require(&self, names: &[&str]) -> Result<()> {
        let missing: Vec<&str> = names.iter().copied().filter(|n| !self.has(n)).collect();
        if missing.is_empty() {
            return Ok(());
        }
        Err(DeskError::ConfigInvalid(format!(
            "script {} must define: {}",
            self.path.display(),
            missing
                .iter()
                .map(|n| format!("`{n}(ctx, ...)`"))
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    /// Call `name(ctx, args...)`, enforcing the timeout inside Lua too.
    pub async fn call(&self, name: &str, args: impl IntoLua) -> Result<Value> {
        let arg = args.into_lua(&self.lua).map_err(|e| self.error(e))?;
        let args = if arg.is_nil() { Vec::new() } else { vec![arg] };
        self.call_args(name, args).await
    }

    /// A JSON value as a Lua value (for arguments).
    pub fn to_lua(&self, v: &serde_json::Value) -> Result<Value> {
        self.lua.to_value(v).map_err(|e| self.error(e))
    }

    /// `ctx.output` for the next call: the handle of a capture's sink, or `nil`.
    pub fn set_output(&self, output: Option<&Output>) -> Result<()> {
        let v = match output {
            Some(o) => Value::UserData(
                self.lua
                    .create_userdata(OutputHandle(o.clone()))
                    .map_err(internal)?,
            ),
            None => Value::Nil,
        };
        self.ctx.set("output", v).map_err(internal)
    }

    /// `ctx.channel` for the next calls (`nil` without channels).
    pub fn set_channel(&self, channel: Option<&str>) -> Result<()> {
        self.ctx.set("channel", channel).map_err(|e| self.error(e))
    }

    /// Call `name(ctx, args...)` with several arguments.
    pub async fn call_args(&self, name: &str, args: Vec<Value>) -> Result<Value> {
        let f: Function = self.lua.globals().get(name).map_err(|e| self.error(e))?;
        let mut argv = MultiValue::new();
        argv.push_back(Value::Table(self.ctx.clone()));
        for a in args {
            argv.push_back(a);
        }
        // On the task's deadline stack, so a host function that fills a capture can suspend it.
        self.deadline.start(self.timeout);
        let result = deadline::scope(self.deadline.clone(), f.call_async::<Value>(argv)).await;
        self.deadline.clear();
        result.map_err(|e| self.error(e))
    }

    /// Convert a value returned by the script, with a helpful error naming the function.
    pub fn decode<T: serde::de::DeserializeOwned>(
        &self,
        function: &str,
        expected: &str,
        value: Value,
    ) -> Result<T> {
        self.lua.from_value(value).map_err(|e| {
            DeskError::ScriptFailed(format!(
                "{}: {function}() must return {expected}: {}",
                self.file_name(),
                lua_message(&e)
            ))
        })
    }

    pub fn file_name(&self) -> String {
        file_name(&self.path)
    }

    /// Host errors raised inside the script (e.g. HA unreachable) keep their code; anything the
    /// script raised itself becomes `script_failed`.
    pub fn error(&self, e: mlua::Error) -> DeskError {
        if let Some(inner) = find_desk_error(&e) {
            return inner.clone();
        }
        let name = self.file_name();
        let msg = lua_message(&e);
        // Lua usually prefixes "<chunk>:<line>:" itself; only add the file name when it didn't.
        if msg.starts_with(&name) {
            DeskError::ScriptFailed(msg)
        } else {
            DeskError::ScriptFailed(format!("{name}: {msg}"))
        }
    }

    pub fn take_log(&self) -> Vec<String> {
        self.log
            .lock()
            .map(|mut l| std::mem::take(&mut *l))
            .unwrap_or_default()
    }
}

fn install_timeout_hook(
    lua: &Lua,
    deadline: Deadline,
    timeout: Duration,
    path: &Path,
) -> Result<()> {
    let what = format!("script {}", file_name(path));
    let hook = move |_: &Lua, _: &mlua::debug::Debug| -> mlua::Result<VmState> {
        if deadline.expired() {
            return Err(mlua::Error::external(DeskError::Timeout {
                what: what.clone(),
                after: timeout,
            }));
        }
        Ok(VmState::Continue)
    };
    let triggers = HookTriggers::new().every_nth_instruction(HOOK_EVERY);
    // The main thread runs the top level; async calls run on new threads (global hook).
    lua.set_hook(triggers, hook.clone()).map_err(internal)?;
    lua.set_global_hook(triggers, hook).map_err(internal)
}

/// Build `ctx` and replace unsafe/noisy globals.
fn host_api(
    lua: &Lua,
    device: &DeviceInfo,
    params: &toml::Table,
    log: Arc<Mutex<Vec<String>>>,
    providers: Arc<Registry>,
) -> mlua::Result<Table> {
    let globals = lua.globals();
    globals.set("dofile", Value::Nil)?;
    globals.set("loadfile", Value::Nil)?;
    // Lua doesn't verify bytecode, so binary chunks are a way out of the sandbox: nothing can
    // produce them (`string.dump`) and `load` only takes source text.
    let string: Table = globals.get("string")?;
    string.set("dump", Value::Nil)?;
    lua.load(
        r#"
        local raw_load = load
        load = function(chunk, name, mode, ...)
          return raw_load(chunk, name, "t", ...)
        end
        "#,
    )
    .set_name("=sandbox")
    .exec()?;

    let ctx = lua.create_table()?;

    let dev = lua.create_table()?;
    dev.set("id", device.id)?;
    dev.set("name", device.name)?;
    ctx.set("device", dev)?;
    ctx.set("params", lua.to_value(params)?)?;

    // ctx.log(...) and print(...): tostring() each argument, join with spaces.
    let device_id = device.id.to_string();
    let log_fn = lua.create_function(move |lua, args: MultiValue| {
        let tostring: Function = lua.globals().get("tostring")?;
        let parts = args
            .into_iter()
            .map(|v| tostring.call::<String>(v))
            .collect::<mlua::Result<Vec<_>>>()?;
        let line = parts.join(" ");
        eprintln!("{device_id}: {line}");
        if let Ok(mut l) = log.lock() {
            l.push(line);
        }
        Ok(())
    })?;
    ctx.set("log", log_fn.clone())?;
    globals.set("print", log_fn)?;

    // ctx.sleep(ms) or ctx.sleep("1.5s")
    let sleep = lua.create_async_function(|_, arg: Value| async move {
        let d = match arg {
            Value::Integer(ms) if ms >= 0 => Duration::from_millis(ms as u64),
            Value::Number(ms) if ms >= 0.0 && ms.is_finite() => {
                Duration::try_from_secs_f64(ms / 1000.0)
                    .map_err(|_| mlua::Error::runtime("ctx.sleep: the duration is too long"))?
            }
            Value::String(s) => humantime::parse_duration(&s.to_str()?)
                .map_err(|e| mlua::Error::runtime(format!("ctx.sleep: invalid duration: {e}")))?,
            _ => {
                return Err(mlua::Error::runtime(
                    "ctx.sleep expects milliseconds (number) or a duration string like \"1.5s\"",
                ));
            }
        };
        tokio::time::sleep(d).await;
        Ok(())
    })?;
    ctx.set("sleep", sleep)?;

    // ctx.clock(): monotonic milliseconds (the sandbox has no `os.clock`/`os.time`).
    let start = Instant::now();
    ctx.set(
        "clock",
        lua.create_function(move |_, _: MultiValue| Ok(start.elapsed().as_secs_f64() * 1000.0))?,
    )?;

    // ctx.power{ provider = "ha", entity = "switch.x" } -> handle with on/off/toggle/state
    let power = lua.create_function(move |lua, spec: Value| {
        let json: serde_json::Value = lua.from_value(spec)?;
        let table: toml::Table = serde_json::from_value(json).map_err(|e| {
            mlua::Error::runtime(format!(
                "ctx.power expects a table like a power binding: {e}"
            ))
        })?;
        let binding = Binding::try_from(table).map_err(|e| {
            mlua::Error::external(DeskError::ConfigInvalid(format!("ctx.power: {e}")))
        })?;
        let Binding::Native {
            ref provider,
            ref params,
            ..
        } = binding
        else {
            return Err(mlua::Error::external(DeskError::ConfigInvalid(
                "ctx.power: a script cannot call another script; use `provider = ...`".into(),
            )));
        };
        let sw: Arc<dyn PowerSwitch> = providers
            .get(provider)
            .and_then(|p| p.power_switch(params))
            .map(Arc::from)
            .map_err(mlua::Error::external)?;
        power_handle(lua, sw, binding.describe())
    })?;
    ctx.set("power", power)?;

    // ctx.append{ into = ctx.output, data = "…" }: bytes the driver has into the capture.
    let append = lua.create_function(|_, spec: Table| {
        let out = output_of(&spec, "ctx.append")?;
        let data: mlua::LuaString = spec
            .get::<Option<mlua::LuaString>>("data")?
            .ok_or_else(|| mlua::Error::runtime("ctx.append: `data` must be a string of bytes"))?;
        out.write(&data.as_bytes()).map_err(mlua::Error::external)
    })?;
    ctx.set("append", append)?;

    Ok(ctx)
}

/// What `ctx.output` holds: a capture's sink. Opaque to scripts: no methods, no path.
struct OutputHandle(Output);

impl mlua::UserData for OutputHandle {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, _, ()| Ok("capture output"));
    }
}

/// The sink a host function fills: `spec.into`, which must be `ctx.output` of the running call.
pub(crate) fn output_of(spec: &Table, function: &str) -> mlua::Result<Output> {
    let bad = || mlua::Error::runtime(format!("{function}: `into` must be ctx.output"));
    let Value::UserData(ud) = spec.get::<Value>("into")? else {
        return Err(bad());
    };
    let out = ud.borrow::<OutputHandle>().map_err(|_| bad())?.0.clone();
    if !out.is_open() {
        return Err(mlua::Error::external(DeskError::ScriptFailed(format!(
            "{function}: this ctx.output belongs to an action call that has ended"
        ))));
    }
    Ok(out)
}

/// `ctx.serial`: `open{ baud = …, … }` opens the binding's port (and only that one).
fn serial_api(lua: &Lua, port: Option<PortSpec>) -> mlua::Result<Table> {
    let serial = lua.create_table()?;
    let open = lua.create_async_function(move |lua, opts: Option<Table>| {
        let port = port.clone();
        async move {
            let Some(spec) = port else {
                return Err(mlua::Error::external(DeskError::ConfigInvalid(
                    "ctx.serial.open: this binding has no `port = { ... }`".into(),
                )));
            };
            let settings = serial_settings(opts)?;
            let port = Port::open(spec, settings)
                .await
                .map_err(mlua::Error::external)?;
            port_handle(&lua, port)
        }
    })?;
    serial.set("open", open)?;
    Ok(serial)
}

/// `ctx.sigrok`: `driver`, `conn` and `acquire{ … }` for the binding's sigrok device (and only
/// that one). Without a `sigrok = { … }` binding, `acquire` fails with a config error.
fn sigrok_api(
    lua: &Lua,
    device: &str,
    spec: Option<SigrokSpec>,
    providers: Arc<Registry>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    if let Some(s) = &spec {
        t.set("driver", s.driver.as_str())?;
        t.set("conn", s.conn.as_deref())?;
    }
    let device = device.to_string();
    let acquire = lua.create_async_function(move |lua, req: Value| {
        let spec = spec.clone();
        let providers = providers.clone();
        let device = device.clone();
        async move {
            let Some(spec) = spec else {
                return Err(mlua::Error::external(DeskError::ConfigInvalid(
                    "ctx.sigrok.acquire: this binding has no `sigrok = { driver = \"…\" }`".into(),
                )));
            };
            let Value::Table(req) = req else {
                return Err(mlua::Error::runtime(
                    "ctx.sigrok.acquire expects a table, e.g. ctx.sigrok.acquire{ samples = 1000, \
                     into = ctx.output, timeout = \"5s\" }",
                ));
            };
            let acquire = acquire_request(&req)
                .map_err(|m| mlua::Error::runtime(format!("ctx.sigrok.acquire: {m}")))?;
            let out = output_of(&req, "ctx.sigrok.acquire")?;
            // Its own timeout instead of the script's and the session's.
            let _suspended = deadline::suspend();
            let bytes = sigrok::acquire(
                providers.configs(),
                providers.base_dir(),
                &device,
                &spec,
                &acquire,
                &out,
            )
            .await
            .map_err(mlua::Error::external)?;
            let r = lua.create_table()?;
            r.set("bytes", bytes)?;
            Ok(r)
        }
    })?;
    t.set("acquire", acquire)?;
    Ok(t)
}

/// A duration from Lua: milliseconds (number) or a string like "2s".
fn lua_duration(what: &str, v: &Value) -> std::result::Result<Duration, String> {
    let d = match v {
        Value::Integer(ms) if *ms > 0 => Duration::from_millis(*ms as u64),
        Value::Number(ms) if *ms > 0.0 && ms.is_finite() => {
            Duration::try_from_secs_f64(ms / 1000.0).map_err(|_| format!("`{what}` is too long"))?
        }
        Value::String(s) => {
            let s = s.to_str().map_err(|_| format!("`{what}` must be text"))?;
            humantime::parse_duration(s.trim())
                .map_err(|e| format!("`{what}` \"{s}\" is not a duration like \"2s\": {e}"))?
        }
        _ => {
            return Err(format!(
                "`{what}` must be milliseconds (a number above 0) or a duration like \"2s\""
            ));
        }
    };
    if d.is_zero() {
        return Err(format!("`{what}` must be above 0"));
    }
    Ok(d)
}

fn lua_count(what: &str, v: &Value) -> std::result::Result<u64, String> {
    match v {
        Value::Integer(n) if *n > 0 => Ok(*n as u64),
        Value::Number(n) if *n >= 1.0 && n.fract() == 0.0 && *n < 9.0e15 => Ok(*n as u64),
        _ => Err(format!("`{what}` must be a whole number above 0")),
    }
}

/// Check the table a driver passes to `ctx.sigrok.acquire` (everything but `into`).
fn acquire_request(t: &Table) -> std::result::Result<Acquire, String> {
    const KEYS: &[&str] = &[
        "config",
        "channels",
        "time",
        "samples",
        "frames",
        "triggers",
        "wait_trigger",
        "format",
        "into",
        "timeout",
    ];
    let lua_err = |e: mlua::Error| e.to_string();
    for pair in t.pairs::<Value, Value>() {
        let (k, _) = pair.map_err(lua_err)?;
        let known = match &k {
            Value::String(s) => s.to_str().is_ok_and(|s| KEYS.contains(&&*s)),
            _ => false,
        };
        if !known {
            return Err(format!(
                "unknown key {} (allowed: {})",
                match &k {
                    Value::String(s) => format!("`{}`", s.to_string_lossy()),
                    other => format!("of type {}", other.type_name()),
                },
                KEYS.join(", ")
            ));
        }
    }
    let get = |k: &str| t.get::<Value>(k).map_err(lua_err);

    let mut config = Vec::new();
    match get("config")? {
        Value::Nil => {}
        Value::Table(c) => {
            for pair in c.pairs::<Value, Value>() {
                let (k, v) = pair.map_err(lua_err)?;
                let Value::String(k) = k else {
                    return Err("`config` keys must be sigrok config names (strings)".into());
                };
                let k = k.to_string_lossy();
                sigrok::check_config_key(&k)?;
                let v = match v {
                    Value::String(s) => {
                        let s = s
                            .to_str()
                            .map_err(|_| format!("config `{k}` must be text"))?
                            .to_string();
                        sigrok::check_config_value(&k, &s)?;
                        s
                    }
                    Value::Integer(i) => i.to_string(),
                    Value::Number(n) => {
                        sigrok::number_text(n).map_err(|e| format!("config `{k}`: {e}"))?
                    }
                    Value::Boolean(b) => b.to_string(),
                    other => {
                        return Err(format!(
                            "config `{k}` must be a string, number or boolean, not {}",
                            other.type_name()
                        ));
                    }
                };
                config.push((k, v));
            }
            config.sort();
        }
        other => {
            return Err(format!(
                "`config` must be a table like {{ samplerate = 1000000 }}, not {}",
                other.type_name()
            ));
        }
    }

    let channels: Vec<String> = match get("channels")? {
        Value::Nil => Vec::new(),
        Value::String(s) => {
            let s = s
                .to_str()
                .map_err(|_| "`channels` must be text".to_string())?;
            s.split(',').map(|c| c.trim().to_string()).collect()
        }
        Value::Table(list) => {
            let mut v = Vec::new();
            for item in list.sequence_values::<Value>() {
                match item.map_err(lua_err)? {
                    Value::String(s) => v.push(s.to_string_lossy()),
                    other => {
                        return Err(format!(
                            "`channels` lists names like \"D0\", not {}",
                            other.type_name()
                        ));
                    }
                }
            }
            if v.is_empty() {
                return Err("`channels` is empty (leave it out for all channels)".into());
            }
            v
        }
        other => {
            return Err(format!(
                "`channels` must be a list like {{ \"D0\", \"D1\" }} or \"D0,D1\", not {}",
                other.type_name()
            ));
        }
    };
    for c in &channels {
        sigrok::check_channel(c)?;
    }

    let mut lengths = Vec::new();
    for k in ["time", "samples", "frames"] {
        let v = get(k)?;
        if v.is_nil() {
            continue;
        }
        lengths.push(match k {
            "time" => {
                let d = lua_duration("time", &v)?;
                // Whole milliseconds, never shorter than asked.
                let ms = d.as_nanos().div_ceil(1_000_000);
                Length::Time(u64::try_from(ms).map_err(|_| "`time` is too long".to_string())?)
            }
            "samples" => Length::Samples(lua_count(k, &v)?),
            _ => Length::Frames(lua_count(k, &v)?),
        });
    }
    let length = match lengths.as_slice() {
        [one] => *one,
        [] => return Err("give one of `time` (ms), `samples` or `frames`".into()),
        _ => return Err("give only one of `time`, `samples` and `frames`".into()),
    };

    let triggers = match get("triggers")? {
        Value::Nil => None,
        Value::String(s) => {
            let s = s
                .to_str()
                .map_err(|_| "`triggers` must be text".to_string())?;
            sigrok::check_triggers(&s)?;
            Some(s.to_string())
        }
        other => {
            return Err(format!(
                "`triggers` must be a string like \"D0=r\", not {}",
                other.type_name()
            ));
        }
    };
    let wait_trigger = match get("wait_trigger")? {
        Value::Nil => false,
        Value::Boolean(b) => b,
        other => {
            return Err(format!(
                "`wait_trigger` must be a boolean, not {}",
                other.type_name()
            ));
        }
    };
    if wait_trigger && triggers.is_none() {
        return Err("`wait_trigger = true` needs `triggers`".into());
    }
    let format = match get("format")? {
        Value::Nil => "srzip".to_string(),
        Value::String(s) => {
            let s = s
                .to_str()
                .map_err(|_| "`format` must be text".to_string())?;
            sigrok::check_format(&s)?;
            s.to_string()
        }
        other => {
            return Err(format!(
                "`format` must be a string like \"srzip\", not {}",
                other.type_name()
            ));
        }
    };
    let timeout = match get("timeout")? {
        Value::Nil => {
            return Err(
                "`timeout` is required: how long the whole capture may take, trigger wait included (ms or \"10s\")"
                    .into(),
            );
        }
        v => lua_duration("timeout", &v)?,
    };
    Ok(Acquire {
        config,
        channels,
        length,
        triggers,
        wait_trigger,
        format,
        timeout,
    })
}

fn serial_settings(opts: Option<Table>) -> mlua::Result<Settings> {
    let mut s = Settings::default();
    let Some(t) = opts else { return Ok(s) };
    let bad = |m: String| mlua::Error::runtime(format!("ctx.serial.open: {m}"));
    for pair in t.pairs::<String, Value>() {
        let (k, v) = pair?;
        match (k.as_str(), v) {
            ("baud", Value::Integer(b)) if b > 0 => s.baud = b as u32,
            ("data_bits", Value::Integer(b)) => {
                s.data_bits = match b {
                    5 => serialport::DataBits::Five,
                    6 => serialport::DataBits::Six,
                    7 => serialport::DataBits::Seven,
                    8 => serialport::DataBits::Eight,
                    _ => return Err(bad(format!("data_bits must be 5..8, got {b}"))),
                }
            }
            ("stop_bits", Value::Integer(b)) => {
                s.stop_bits = match b {
                    1 => serialport::StopBits::One,
                    2 => serialport::StopBits::Two,
                    _ => return Err(bad(format!("stop_bits must be 1 or 2, got {b}"))),
                }
            }
            ("parity", Value::String(p)) => {
                s.parity = match &*p.to_str()? {
                    "none" => serialport::Parity::None,
                    "odd" => serialport::Parity::Odd,
                    "even" => serialport::Parity::Even,
                    other => {
                        return Err(bad(format!(
                            "parity must be none, odd or even, got {other}"
                        )));
                    }
                }
            }
            ("flow", Value::String(f)) => {
                s.flow = match &*f.to_str()? {
                    "none" => serialport::FlowControl::None,
                    "software" => serialport::FlowControl::Software,
                    "hardware" => serialport::FlowControl::Hardware,
                    other => {
                        return Err(bad(format!(
                            "flow must be none, software or hardware, got {other}"
                        )));
                    }
                }
            }
            ("dtr", Value::Boolean(b)) => s.dtr = Some(b),
            ("wait", Value::String(w)) => {
                s.wait = humantime::parse_duration(&w.to_str()?)
                    .map_err(|e| bad(format!("invalid wait: {e}")))?
            }
            ("wait", Value::Integer(ms)) if ms >= 0 => s.wait = Duration::from_millis(ms as u64),
            (k, v) => {
                return Err(bad(format!(
                    "unsupported option `{k}` = {} (options: baud, data_bits, parity, stop_bits, flow, dtr, wait)",
                    v.type_name()
                )));
            }
        }
    }
    Ok(s)
}

/// A Lua table driving one open port: `write`, `read`, `clear`, `close`, `path`.
/// Works with `port:read(…)` and `port.read(…)`.
fn port_handle(lua: &Lua, port: Port) -> mlua::Result<Table> {
    /// Drop a leading `self` table (colon call syntax).
    fn args(mut a: MultiValue) -> MultiValue {
        if matches!(a.front(), Some(Value::Table(_))) {
            a.pop_front();
        }
        a
    }

    let handle = lua.create_table()?;
    handle.set("path", port.path().to_string())?;

    let p = port.clone();
    handle.set(
        "write",
        lua.create_async_function(move |_, a: MultiValue| {
            let p = p.clone();
            async move {
                let Some(Value::String(data)) = args(a).pop_front() else {
                    return Err(mlua::Error::runtime(
                        "port:write expects a string of bytes, e.g. port:write(\"\\x01\\x02\")",
                    ));
                };
                p.write(data.as_bytes().to_vec())
                    .await
                    .map_err(mlua::Error::external)
            }
        })?,
    )?;

    let p = port.clone();
    handle.set(
        "read",
        lua.create_async_function(move |lua, a: MultiValue| {
            let p = p.clone();
            async move {
                let mut a = args(a);
                let max = match a.pop_front() {
                    Some(Value::Integer(n)) if n > 0 => (n as usize).min(MAX_READ),
                    _ => {
                        return Err(mlua::Error::runtime(
                            "port:read expects a byte count, e.g. port:read(17, 500)",
                        ));
                    }
                };
                let timeout = match a.pop_front() {
                    None | Some(Value::Nil) => Duration::from_secs(1),
                    Some(Value::Integer(ms)) if ms >= 0 => Duration::from_millis(ms as u64),
                    Some(Value::Number(ms)) if ms >= 0.0 && ms.is_finite() => {
                        Duration::try_from_secs_f64(ms / 1000.0).map_err(|_| {
                            mlua::Error::runtime("port:read: the timeout is too long")
                        })?
                    }
                    Some(_) => {
                        return Err(mlua::Error::runtime(
                            "port:read timeout must be milliseconds",
                        ));
                    }
                };
                let data = p.read(max, timeout).await.map_err(mlua::Error::external)?;
                lua.create_string(&data)
            }
        })?,
    )?;

    let p = port.clone();
    handle.set(
        "clear",
        lua.create_async_function(move |_, _: MultiValue| {
            let p = p.clone();
            async move { p.clear().await.map_err(mlua::Error::external) }
        })?,
    )?;

    let p = port;
    handle.set(
        "close",
        lua.create_function(move |_, _: MultiValue| {
            p.close();
            Ok(())
        })?,
    )?;
    Ok(handle)
}

/// A Lua table whose functions drive one power path. Works with `p:on()` and `p.on()`.
fn power_handle(lua: &Lua, sw: Arc<dyn PowerSwitch>, label: String) -> mlua::Result<Table> {
    let handle = lua.create_table()?;
    for (name, target) in [
        ("on", Target::On),
        ("off", Target::Off),
        ("toggle", Target::Toggle),
    ] {
        let sw = sw.clone();
        let label = label.clone();
        let f = lua.create_async_function(move |_, _: MultiValue| {
            let sw = sw.clone();
            let label = label.clone();
            async move {
                let outcome = power_core::switch(&label, sw.as_ref(), target)
                    .await
                    .map_err(mlua::Error::external)?;
                Ok(outcome.after.state.to_string())
            }
        })?;
        handle.set(name, f)?;
    }
    // Raw send: no idempotence check, no verification. For quirks such as "press twice".
    let raw = sw.clone();
    let set = lua.create_async_function(move |_, args: MultiValue| {
        let sw = raw.clone();
        async move {
            let Some(Value::Boolean(on)) = args.into_iter().last() else {
                return Err(mlua::Error::runtime(
                    "set expects a boolean, e.g. p:set(true)",
                ));
            };
            sw.set(on).await.map_err(mlua::Error::external)
        }
    })?;
    handle.set("set", set)?;
    let state = lua.create_async_function(move |_, _: MultiValue| {
        let sw = sw.clone();
        async move {
            let s = sw.state().await.map_err(mlua::Error::external)?;
            Ok(s.state.to_string())
        }
    })?;
    handle.set("state", state)?;
    Ok(handle)
}

fn find_desk_error(e: &mlua::Error) -> Option<&DeskError> {
    match e {
        mlua::Error::CallbackError { cause, .. } => find_desk_error(cause),
        mlua::Error::WithContext { cause, .. } => find_desk_error(cause),
        mlua::Error::ExternalError(inner) => inner.downcast_ref::<DeskError>(),
        _ => None,
    }
}

/// The message without Lua's "runtime error:" prefix and stack traceback.
fn lua_message(e: &mlua::Error) -> String {
    let raw = match e {
        mlua::Error::RuntimeError(m) => m.clone(),
        mlua::Error::SyntaxError { message, .. } => message.clone(),
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => {
            return lua_message(cause);
        }
        other => other.to_string(),
    };
    raw.split("\nstack traceback")
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn internal(e: mlua::Error) -> DeskError {
    DeskError::Internal(format!("lua: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host function that fills a capture suspends the script's per-call deadline; it
    /// continues afterwards with the time it had left.
    #[tokio::test]
    async fn a_suspending_host_call_stops_the_script_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.lua");
        std::fs::write(
            &path,
            "function filled(ctx) ctx.slow(300); for i = 1, 100000 do end end\n\
             function slept(ctx) ctx.sleep(300); for i = 1, 100000 do end end\n",
        )
        .unwrap();
        let engine = Engine::load(ScriptSpec {
            device: DeviceInfo { id: "t", name: "t" },
            path: &path,
            params: &toml::Table::new(),
            port: None,
            sigrok: None,
            timeout: Duration::from_millis(100),
            providers: Arc::new(Registry::new(Default::default(), dir.path().to_path_buf())),
        })
        .unwrap();
        let slow = engine
            .lua
            .create_async_function(|_, ms: u64| async move {
                let _suspended = deadline::suspend();
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(())
            })
            .unwrap();
        engine.ctx.set("slow", slow).unwrap();
        engine.call("filled", Value::Nil).await.unwrap();
        let e = engine.call("slept", Value::Nil).await.unwrap_err();
        assert_eq!(e.code(), "timeout", "{e}");
    }
}
