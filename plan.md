# Plan

## MVP goal

> From a terminal, or from an AI agent, I can turn my bench devices' mains power on and off by name.
> Some devices go through Home Assistant directly, and others through a Lua script.

```sh
desk power on psu          # HA switches the Shelly outlet; state is read back
desk power on lamp         # runs scripts/lamp.lua on(ctx) and waits for it
desk status --json         # an agent can see everything
```

Done means: `cargo test` passes without hardware, it works against a real Home Assistant (a short
smoke test), and an agent can drive it using `SKILL.md`.

**Development vs. use:** milestones build the generic tool and are verified with the mock provider
and fakes (e.g. the fake HA in tests). Real hardware is at most a short smoke test at the end of a
milestone. Setting up an actual bench (mapping outlets, naming devices) is *using* the tool. It
happens after the MVP and lives in the user's `~/.config/desk-operator/`, never in this repo.

### In scope

- Config file (`desk.toml`): providers, devices, power bindings (native or Lua script)
- `power_switch` capability: on / off / toggle / status
- Providers: `mock` (tests and demos) and `home-assistant`
- Lua script bindings via mlua, with a minimal host API
- `--json` output, exit codes, `desk schema`, agent skill

### Out of scope (deliberately)

Daemon, GUI/palette/tray/hotkey, instrument control (voltage, current, measurements), serial/USB
host API, command language (`psu 5v on, …`), Shelly-direct provider, MCP, natural language,
declarative step DSL (Lua covers it), WASM/external-process plugins, keyring, safety limits (no
setpoints exist yet).

---

## MVP design

### Config

Location: `--config <path>` > `$DESK_CONFIG` > platform config dir (`directories`, e.g.
`~/.config/desk-operator/desk.toml`). Script paths resolve relative to the config file's directory.

```toml
[providers.ha]
kind      = "home-assistant"
url       = "http://homeassistant.local:8123"
token_env = "HA_TOKEN"

[providers.sim]
kind = "mock"

[devices.psu]
name    = "FNIRSI IPS3608"
aliases = ["supply", "power supply"]
power   = { provider = "ha", entity = "switch.desk_outlet_1" }

[devices.iron]
name  = "Soldering iron"
power = { provider = "ha", entity = "switch.desk_outlet_4" }

[devices.lamp]
name  = "Bench lamp"
power = { script = "scripts/lamp.lua", timeout = "15s", params = { entity = "switch.desk_outlet_2" } }
```

```rust
#[serde(untagged)]
enum PowerBinding {
    Native { provider: String, #[serde(flatten)] params: toml::Table },
    Script { script: PathBuf, timeout: Option<Duration>, params: Option<toml::Table> },
}
```

### Core types

```rust
trait Provider {                                  // one instance per [providers.x]
    async fn test(&self) -> Result<ProviderInfo>;
    async fn endpoints(&self, cap: Capability) -> Result<Vec<Endpoint>>;
    fn power_switch(&self, params: &toml::Table) -> Result<Box<dyn PowerSwitch>>;
}

trait PowerSwitch {
    fn features(&self) -> PowerFeatures;          // readback (more flags when a provider needs them)
    async fn set(&self, on: bool) -> Result<()>;
    async fn state(&self) -> Result<PowerState>;
}

struct PowerState { state: On | Off | Unknown, confidence: Real | Assumed }
```

A script binding is a `PowerSwitch` too (`ScriptPowerSwitch`), so the CLI never cares which kind it has.

**Native semantics:** `set(on)` → read back `state()` → mismatch = error `verify_failed`.
**Script semantics:** call `on(ctx)` / `off(ctx)`. Success = it returned. `state(ctx)` is optional;
without it the state is `unknown`, or `assumed` after a successful set.

### Lua script contract

Implemented in M2; the user-facing reference is [`docs/scripting.md`](docs/scripting.md).

- `on(ctx)` / `off(ctx)` are required, `state(ctx)` is optional. Success = return, failure = `error(...)`.
- Host API: `ctx.device`, `ctx.params`, `ctx.power(binding)` (handle with `on/off/toggle/state/set`),
  `ctx.sleep(ms | "1.5s")`, `ctx.log(...)` (`print` = log).
- Sandbox: `string`, `table`, `math`, `utf8`, `coroutine` only; no `dofile`/`loadfile`. The timeout
  (default 30 s) is enforced with an instruction hook too, so busy loops can't hang the CLI.
- Host errors keep their codes. Script-raised errors become `script_failed` with file and line.

### Home Assistant provider

REST API with `Authorization: Bearer $HA_TOKEN`, 5 s request timeout:

- `test`: `GET /api/`, then `GET /api/config` (version)
- `endpoints(power_switch)`: `GET /api/states`, filtered to the `switch`, `light`, `input_boolean`
  and `fan` domains
- `set`: `POST /api/services/<domain>/turn_on|turn_off` with `{ "entity_id": … }`
- `state`: `GET /api/states/<entity_id>`; `unavailable` → error `endpoint_unavailable`

### Mock provider

State persists in a JSON file under the platform state/cache dir (override with `DESK_MOCK_STATE`),
so `desk power on x` then `desk power status x` works across separate CLI invocations. It supports
injected failures for tests (e.g. endpoint names starting with `fail_`).

### CLI (MVP)

```
desk power on|off|toggle <device>... | --all
desk power status [<device>...]
desk status                                   # all devices + provider health
desk device list | show <device>
desk provider list | test <id> | endpoints <id> [--capability power_switch]
desk schema                                   # devices, bindings, capabilities, commands
global: --json  --config <path>  --dry-run  -v
```

- Devices can be named by id or alias (case-insensitive). Ambiguous → error listing the candidates.
- Multiple devices run concurrently. The result is reported per device and the exit code is
  non-zero if any failed.
- Every target is resolved (and checked for a power path) **before** anything is switched.
- `status` always succeeds (errors are shown inline). `power status` fails if any read fails.

| Exit | JSON `error.code` |
|---|---|
| 0 | — |
| 1 | `internal` |
| 2 | `usage`, `device_not_found`, `device_ambiguous` |
| 3 | `config_invalid`, `no_power_path` |
| 4 | `provider_unreachable`, `endpoint_unavailable`, `state_unknown` |
| 5 | `script_failed` |
| 6 | `timeout` |
| 7 | `verify_failed` |

With several devices the per-device results are always in `data`. The top-level `error` and exit
code come from the first failure.

---

## Milestones

### M0 — Skeleton on the mock provider *(goal: hours)*
- [x] Set up Rust via mise (`mise use rust@stable` → `mise.toml`), `cargo init --name desk-operator`, binary `desk`
- [x] Config loading + validation with good error messages (all problems reported at once, unknown keys rejected)
- [x] `model.rs`, `Provider`/`PowerSwitch` traits, mock provider with persisted state + failure modes (`fail_`, `stuck_`, `blind_`, `latency`)
- [x] `power on|off|toggle|status`, `device list|show`, `status`
- [x] Output layer: human tables + `--json` envelope + exit codes (usage errors too)
- [x] `examples/desk.toml` using only `mock`; integration tests in `tests/cli.rs` (plain `std::process`, no extra deps)

**Done when:** `desk --config examples/desk.toml power on psu && desk ... power status --json` works and is tested.

### M1 — Home Assistant: first real bench test ✦ *MVP is usable here*
- [x] HA provider: `test`, `endpoints`, `set`, `state` (+ end-to-end tests against a stateful fake HA in `tests/home_assistant.rs`)
- [x] `provider list|test|endpoints [--search]`; endpoints include the binding params to paste
- [x] Native semantics: read-back verification, polling within a per-provider settle window (HA: 3 s)
- [x] Token from `token_env` (default `HA_TOKEN`) or `token_file`; a missing token only breaks that provider's devices
- [x] Smoke test against a real HA 2026.10 with a Shelly Power Strip 4 Gen4: on/off with verification
  in ≈0.2 s.

Noticed during the real test (refine later, not blocking):
- A real HA lists ~80 switchable entities (camera toggles etc.); `--search` is essential, and the
  "bind a device with" example uses the first entity in the list, which is arbitrary.
- Long entity ids make the POWER PATH column in `status` very wide; show the HA friendly name, or
  truncate.

**Done when:** the HA tests pass, and a smoke test switches a real HA entity on/off with verification.

### M2 — Lua script bindings
- [x] mlua 0.12 (Lua 5.4, vendored) with a sandboxed stdlib and an async host API (`ctx.power`, `sleep`, `log`, `params`, `device`)
- [x] `ScriptSwitch` with timeout (including busy loops) and error mapping; script logs on stderr and in `--json` (`log`)
- [x] Example scripts (`iron_with_extractor.lua`, `double_press.lua`), `docs/scripting.md`; 16 tests in `tests/scripts.rs`

**Done when:** scripted bindings work end to end in tests (against mock and fake HA), and a failing or
hanging script reports a useful error.

### M3 — Agent-ready
- [ ] `desk schema --json`
- [ ] `--dry-run` for power commands (prints the resolved binding + the actions it would take)
- [ ] `skills/desk-operator/SKILL.md` (start with `schema`, always `--json`, honour `assumed`, no unasked power changes)
- [ ] Try it: let an agent (Claude Code) operate the bench through the skill alone

**Done when:** an agent with only the skill can answer "what's on?" and "turn the iron off".

→ **Then stop, use it for a while, and refine before picking the next item.**

---

## After the MVP (rough order, re-evaluate each time)

1. **Config editing from the CLI:** `device add`, `device power <dev>` with interactive provider/entity pickers (`toml_edit`, `inquire`)
2. **Serial/USB host API for Lua:** `ctx.usb.wait_for{vid,pid}`, `port:write`, `port:query`, enabling wake/verify sequences
3. **First instrument capability:** `voltage_source` + a driver for the IPS3608 (if it has a PC interface) or another PSU; the first safety limits
4. **Daemon** owning the hardware (exclusive serial ports); the CLI talks to it over a local socket, with in-process fallback
5. **Command language:** `psu 5v 1a on, iron 330c then dmm dcv`, plus scenes
6. **Palette + tray** (Tauri v2), with `desk palette` for Wayland compositor binds
7. **MCP server** (`desk mcp`), then natural language with your own API key, always as a previewed plan
8. **Community driver library**, Shelly-direct provider, more capabilities (load, meter, heater; scope much later)

## Decisions log

- **Lua (mlua) from day one, not Rhai:** familiar to many users, natively async with tokio, and
  scripts are optional per device. The declarative step DSL is dropped for now because Lua covers it.
- **`ctx.power{...}` instead of provider-specific script APIs (`ha:turn_on`):** scripts use the same
  binding syntax as the config, so they work with every provider, present and future, and get the
  same idempotent, verified switching. `p:set()` is the raw escape hatch. Provider-specific calls
  (e.g. arbitrary HA services) get added when a real device needs them.
- **Single crate for the MVP:** split into a workspace when the daemon or UI needs a shared core library.
- **No daemon in the MVP:** HA is stateless HTTP. The daemon arrives with serial devices.
- **"power" = mains, "output" = PSU terminals:** never conflate the two.
- **Verification polls instead of reading once:** HA/Shelly can report the old state briefly after
  a service call. Each power path declares a `settle` window (`PowerFeatures::settle`).
- **`token_file` besides `token_env`:** lets a token live in a 0600 file next to the config, so
  it's available to every shell (and agents) without being exported everywhere.
- **HA switchable domains:** `switch`, `light`, `fan`, `input_boolean`. Entity ids are validated
  strictly (`domain.object_id`, `[a-z0-9_]`), which also keeps them URL-safe.

## Open questions

- Does the FNIRSI IPS3608 have a PC control interface (USB serial / Modbus)? This decides post-MVP item 3.
- Binary name: `desk` (assumed) vs. something less generic.
