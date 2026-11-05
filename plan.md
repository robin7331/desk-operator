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

Noticed during the real test (resolved in the polish pass after M3):
- A real HA lists ~80 switchable entities: `provider endpoints` now suggests `--search` above 20
  results and shows a `<ENDPOINT>` placeholder instead of an arbitrary first entity.
- Long entity ids made tables very wide: `status` and dry-run show a short VIA column, and
  `device list` shortens paths in the middle. Full bindings stay in `device show` and JSON.
- JSON: `desk status` entries use `device` (like `power status`), not `id`.

**Done when:** the HA tests pass, and a smoke test switches a real HA entity on/off with verification.

### M2 — Lua script bindings
- [x] mlua 0.12 (Lua 5.4, vendored) with a sandboxed stdlib and an async host API (`ctx.power`, `sleep`, `log`, `params`, `device`)
- [x] `ScriptSwitch` with timeout (including busy loops) and error mapping; script logs on stderr and in `--json` (`log`)
- [x] Example scripts (`iron_with_extractor.lua`, `double_press.lua`), `docs/scripting.md`; 16 tests in `tests/scripts.rs`

**Done when:** scripted bindings work end to end in tests (against mock and fake HA), and a failing or
hanging script reports a useful error.

### M3 — Agent-ready
- [x] `desk schema --json`: bench, capabilities, every command/arg (generated from clap), error codes; works without a config
- [x] `--dry-run` for `power on|off|toggle`: resolves, validates and reads state, then reports `would_send` per device
- [x] `skills/desk-operator/SKILL.md`, embedded in the binary (`desk skill`); a test checks it against the schema
- [x] Try it: a fresh subagent with only `SKILL.md` and a wrapper pinned to the simulated bench answered
  "what's on?", switched the iron off with verification, and asked instead of toggling the
  send-only lamp. Its feedback led to fixes: `results` vs `devices` named correctly in the skill,
  `confidence: "none"` for unknown states, a clearer dry-run rule, and the example "station" got
  its own outlets plus an honest `state()` (it had shared one with `iron`).

**Done when:** an agent with only the skill can answer "what's on?" and "turn the iron off".

→ **Then stop, use it for a while, and refine before picking the next item.**

---

## M4 — Instrument control: the `voltage_source` capability

**Goal:** control the *functions* of an instrument, not only its mains power, through a generic
capability. Bench power supplies are the first class. The core never knows any specific model; the
first real driver (FNIRSI DPS-150 protocol family, which also covers the IPS3608) only uses generic APIs.
Anything it needs that the API can't do is a gap in the API, and gets fixed generically.

```toml
[devices.psu]
power  = { provider = "ha", entity = "switch.…" }        # power_switch: mains (exists)
source = { script = "drivers/fnirsi_dps150.lua",         # voltage_source: the instrument itself
           port   = { usb_vid = 0x1234, usb_pid = 0x5678 },
           params = { model = "ips3608" } }
limits = { voltage = 12.0, current = 2.0 }                # bench safety, enforced by the core
```

**Design**
- **One config key per capability** (`power`, `source`; later `load`, `meter`, …). Capabilities are
  then visible without loading any driver. A binding is native (`provider = …`) or a Lua script,
  exactly like power bindings.
- **`port` belongs to the binding**, not to the script: `ctx.serial.open{ baud = … }` opens the bound
  port and nothing else, so a driver can't reach other devices' ports. Ports are matched by
  `path` or USB `usb_vid`/`usb_pid`/`serial_number`, never by a fixed `/dev/tty…` alone.
- **Sessions:** a command opens one driver session (`open(ctx)` … `close(ctx)`, both optional)
  around all its steps, and always closes it, even on errors and timeouts. Some devices lock their
  front panel while connected, so no long-lived connection (and still no daemon).
- **Semantics in the core, not in drivers:** check limits (device maximum from `info()` and the
  bench `limits`), then setpoints, then output (always in that order), then read back. Setpoints
  that a driver can't read back are reported as **unverified**, never as confirmed.
- **Mains first:** if the device has a power path that reads `off`, fail fast with `device_off`
  and a hint, instead of a confusing "port not found". Switching it on automatically comes later,
  with the command language.
- **Single channel** for now. Multi-channel PSUs come later as `channel = n`.

**Driver contract (Lua)**: `info(ctx)` → `{ max_voltage, max_current }`, `set_voltage(ctx, v)`,
`set_current(ctx, a)`, `set_output(ctx, on)`, `read(ctx)` → `{ output?, voltage_set?, current_set?,
voltage?, current?, power? }`, optional `open`/`close`. Host API gains `ctx.serial.open{…}` → `port`
with `write(bytes)`, `read(n, timeout)`, `clear()`, `close()`. Lua 5.4's `string.pack("<f", 5.0)`
handles binary frames.

**CLI**
```
desk source set psu 5v 1a [--on] [--dry-run]   # units required: v/mv, a/ma
desk source output on|off psu [--dry-run]
desk source read psu                            # output, setpoints, measured V/A/W
```

**Steps** (each tested without hardware)
- [x] **M4.1** `voltage_source` model, `source`/`limits` config, core semantics, `desk source …`,
      simulated PSU in the mock provider (`load_ohms` for CC/CV behavior), schema + skill updates.
      New codes: `capability_missing` (3), `device_off` (4), `limit_exceeded` (8). Also refuses
      `output on` when the device already holds a setpoint above the bench limits.
- [x] **M4.2** Lua source drivers: shared script engine (`script::Engine`) for power and source
      scripts, sessions with guaranteed `close`, driver logs kept in errors, `docs/scripting.md`
- [ ] **M4.3** Serial host API: port matching, `ctx.serial.open`, read/write/timeouts; tested over a
      pseudo-terminal pair (Unix)
- [ ] **M4.4** `drivers/fnirsi_dps150.lua`, tested against a fake device that speaks the protocol
      over a pty (setpoints, output echo, pushed telemetry)
- [ ] **M4.5** Smoke test on a real IPS3608 (nothing connected to the output), only with the user's go-ahead

**Background (research, Nov 2026):** the IPS3608 is a USB CDC-ACM device that speaks the DPS-150
protocol family: frames `F1|F0, type, cmd, len, data, sum(cmd..data)`, float32 LE, set V/I = `C1`/`C2`
(no reply), output = `DB` (echoed), telemetry `C3` (V, A, W) pushed ~every 0.6 s, connect/disconnect
via `C1 00`. Sources: [hamsternz/fnirsi_psu](https://github.com/hamsternz/fnirsi_psu) (working C tool),
[DPS-150 protocol docs](https://philipp-iot.github.io/fnirsi_protocol/protocol/reference), FNIRSI ships
one PC app for "DPS-150 / IPS3608". Unknown until tested: USB VID:PID, setpoint/output readback.
Other PSU families the capability must also fit: SCPI-style text (Rigol, Siglent, Owon, Korad) and
Modbus registers (Riden RD60xx).

## After M4 (rough order, re-evaluate each time)

1. **Config editing from the CLI:** `device add`, `device power <dev>` with interactive provider/entity pickers (`toml_edit`, `inquire`)
2. **Driver library:** `driver = "fnirsi-dps150"` resolves to drivers bundled with desk; native Rust drivers for common families (SCPI, Modbus)
3. **More capabilities:** `electronic_load`, `meter`, `heater`; multi-channel sources; scope much later
4. **Command language:** `psu 5v 1a on, iron 330c then dmm dcv`, scenes, automatic "mains on → wait for port → configure"
5. **Palette + tray** (Tauri v2), with `desk palette` for Wayland compositor binds
6. **Daemon** when something needs long-lived state (palette live values, monitoring); the CLI talks to it over a local socket, with in-process fallback
7. **MCP server** (`desk mcp`), then natural language with your own API key, always as a previewed plan
8. **Community driver library**, Shelly-direct provider

## Decisions log

- **Lua (mlua) from day one, not Rhai:** familiar to many users, natively async with tokio, and
  scripts are optional per device. The declarative step DSL is dropped for now because Lua covers it.
- **`ctx.power{...}` instead of provider-specific script APIs (`ha:turn_on`):** scripts use the same
  binding syntax as the config, so they work with every provider, present and future, and get the
  same idempotent, verified switching. `p:set()` is the raw escape hatch. Provider-specific calls
  (e.g. arbitrary HA services) get added when a real device needs them.
- **Single crate for the MVP:** split into a workspace when the daemon or UI needs a shared core library.
- **No daemon in the MVP:** HA is stateless HTTP. M4 confirmed it's not needed for serial devices
  either: one session per command is the better default (some devices lock their front panel while
  connected).
- **Capability-named binding keys (`source = …`) instead of a generic `control = …`:** capabilities
  are known without loading drivers, and a driver's port is part of its binding (scripts can't open
  other ports).
- **"power" = mains, "output" = PSU terminals:** never conflate the two.
- **Verification polls instead of reading once:** HA/Shelly can report the old state briefly after
  a service call. Each power path declares a `settle` window (`PowerFeatures::settle`).
- **`token_file` besides `token_env`:** lets a token live in a 0600 file next to the config, so
  it's available to every shell (and agents) without being exported everywhere.
- **HA switchable domains:** `switch`, `light`, `fan`, `input_boolean`. Entity ids are validated
  strictly (`domain.object_id`, `[a-z0-9_]`), which also keeps them URL-safe.

## Open questions

- Binary name: `desk` (assumed) vs. something less generic.
