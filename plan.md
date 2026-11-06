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
- [x] **M4.3** Serial host API: `port = { … }` in script bindings, `ctx.serial.open{…}` with
      write/read/clear/close, `wait` for ports to appear, exclusive opens, `desk port list`
      (USB only unless `--all`). Tested over pseudo-terminal pairs (Unix), stable over 30 repeated runs
- [x] **M4.4** First device definitions: `models/fnirsi/` with a shared DPS-150-family protocol
      driver and two definitions (`ips3608`, `dps-150`), tested against a fake device that speaks
      the protocol over a pty (connect/disconnect, setpoints, output echo, pushed telemetry, resync).
      API gap it revealed, fixed generically: `ctx.clock()` (the sandbox had no time source, needed
      to ignore measurements taken before a command). A test now enforces that `src/` never
      mentions `models/` or a vendor.
- [x] **M4.5** Smoke test on a real IPS3608 (firmware V1.1, nothing connected to the output). Worked:
      read, set + output on, all confirmed by read-back; bench limit refusal; `device_off` with mains
      off; reading right after mains on. Found on hardware and fixed in the driver/definition, not
      the core: no output echo (unlike the DPS-150 docs), so setpoints and output come from the full
      status (layout found by diffing snapshots, offsets live in the definition); the name query
      guards against other devices with Artery's generic USB id `2e3c:5740`; a boot window
      (`boot_ms`) because the port appears before the device answers; `settle_ms = 1000` for the
      output ramp. Also learned: after a mains cut the output comes back off, setpoints are kept.

**Lessons for M5's definition format:** a definition needs a way to identify the model (generic
USB ids are common), device-specific data the driver interprets (status offsets), and a
"tested on hardware" status with firmware versions. The user had to copy all driver params into
their own config, which is exactly what `model = "…"` must remove.

**Background (research, Nov 2026):** the IPS3608 is a USB CDC-ACM device that speaks the DPS-150
protocol family: frames `F1|F0, type, cmd, len, data, sum(cmd..data)`, float32 LE, set V/I = `C1`/`C2`
(no reply), output = `DB` (echoed), telemetry `C3` (V, A, W) pushed ~every 0.6 s, connect/disconnect
via `C1 00`. Sources: [hamsternz/fnirsi_psu](https://github.com/hamsternz/fnirsi_psu) (working C tool),
[DPS-150 protocol docs](https://philipp-iot.github.io/fnirsi_protocol/protocol/reference), FNIRSI ships
one PC app for "DPS-150 / IPS3608". Unknown until tested: USB VID:PID, setpoint/output readback.
Other PSU families the capability must also fit: SCPI-style text (Rigol, Siglent, Owon, Korad) and
Modbus registers (Riden RD60xx).

## Devices vs. device definitions (terminology, decided after M4.3)

| | **Device** | **Device definition** |
|---|---|---|
| What | `psu` on *one* bench | "FNIRSI IPS3608": what's true for every unit of a model |
| Contains | name, aliases, the user's power path (HA, Shelly, …), bench `limits`, which unit (serial number) | driver(s), USB ids, device maximums, docs, tests, tested firmware |
| Lives in | the user's `~/.config/desk-operator/desk.toml` (never in a repo) | the device library: `models/<vendor>/<model>/` |

A definition never contains anything about how a particular user switches mains or names things.

**Naming (2026-11-06):** a **device** is on your bench, a **model** is what kind of thing it is. `desk
library`, "definition", `device.toml` and `devices/` became `desk model`, "model", `model.toml` and
`models/`. Older sections below may still say "definition" or "library" for a model.

**In this repository:** the library is `models/`, bundled by `build.rs` (vendor folders only). Its
fake-device tests are `tests/model_*.rs`, run against this checkout's `desk`. The core's tests don't
depend on which definitions are bundled. (It had its own repository, desk-operator-devices, for one
day on 2026-11-06. A merged device still needed a submodule update to reach anyone, so it moved back
with its history for a lean start. It can be split out again when device updates need to outpace
desk builds.)

## M5 — Device library ✓

- **`model = "fnirsi/ips3608"`** in a device: the definition supplies its capability bindings
  (`source = …`, ports, params, maximums). The user's config adds what's theirs (`power`, `limits`,
  aliases) and may override per unit (e.g. `port.serial_number` when two identical units exist).
- **Definition format** (`model.toml`): id, name, kind, `requires = "desk-operator >= x"`, bindings,
  docs and a status (`community`, `tested on hardware`, firmware versions). Shared drivers for
  protocol families (one driver, several definitions).
- **Where definitions come from:** bundled with desk (`models/` embedded at build time), a local
  folder (`~/.config/desk-operator/models/`, for your own or unpublished ones), later the remote
  library repo.
- **CLI:** `desk devices list|show|check` (validate a definition: format, driver functions, tests).
- **Definition tests that travel with the definition** (a fake device described next to the
  driver), so the library can live in its own repo later. M4.4's fake device is written in Rust in
  `tests/`; M5 decides how definitions carry their own.
- Contributor docs: layout, contract, what "tested" means, how to submit.

**Done:**
- [x] `model = "vendor/model"` with key-by-key merging (own `script`/`provider` replaces);
      device name defaults to the definition's
- [x] `model.toml` format: `[device]` (id must match the folder, `requires` checked, status,
      tested) + capability tables; **script bindings only** (providers are rejected)
- [x] Search path: `models/` next to the config → `$DESK_MODELS` → bundled (embedded by
      `build.rs` as `bundled:<path>`; `src/` still never names a vendor or reads the repo folder)
- [x] `desk model list|show|check` (`check` loads every driver in the sandbox, no hardware)
- [x] Port matching with `path` + USB ids follows symlinks (`/dev/serial/by-id/…`) and refuses a
      different USB device on that path
- [x] `models/README.md` as contributor guide; skill uses `model =`
- [x] Real bench switched to `model = "fnirsi/ips3608"` + its serial number; read verified on hardware
- Decision: definition tests stay in Rust, in `tests/model_*.rs` (2026-11-06); a Lua
  fake-device format (`desk model test`, no Rust needed) is a later option for contributors.

## M5.1 — A defined definition format ✓

Before AI skills generate definitions, the format must be precise: what's required, what's
optional, what's validated. Found gaps after M5: device maximums hid in free-form `params`
(convention only, typos silently ignored), `kind`/`status` were free text, bench `limits` were
device-wide (ambiguous once a device has two capabilities), no scaffold.

- [x] **Bench limits per capability:** `limits = { source = { voltage = 12, current = 2 } }`
- [x] **Ratings** per capability binding, standardized by the core: `voltage_source` →
      `ratings = { max_voltage, max_current, max_power? }`, enforced by the core (stricter of
      ratings and the driver's `info()`); required in definitions
- [x] **Drivers declare their params** (`PARAMS = { name = { type = "string", doc = … } }`):
      unknown keys, wrong types and missing required params are config errors; defaults are applied.
      Required for library drivers
- [x] **Format v1:** `status` = `community` | `tested`, `kind` dropped (capabilities say it),
      optional `sources = [urls]`
- [x] **`desk model new vendor/model --capability voltage_source`** scaffold;
      `desk model show` lists the driver's params
- [x] **Spec** `docs/model-format.md` + JSON Schema for editors

## M6 — AI skills (in progress)

A suite of agent skills shipped with desk, building on the M5.1 format. The existing
`skills/desk-operator/SKILL.md` (operate the bench, printed by `desk skill`) is the first.

### Skill: add a device

Ask the user for the model, research it, produce a definition, test it, smoke-test it with the
user, write *their* bench part into *their* config, and offer to publish the definition (never the
bench part). It should follow the workflow that worked for the IPS3608 in M4.5:

1. **Ask:** exact model, what it is (capability), how it connects (USB, LAN, …), whether the user
   wants it controlled by desk or only switched on/off (then it's just a `power` binding, no
   definition needed).
2. **Check the library first:** `desk model list` / `show`; an existing family driver may fit
   (e.g. the DPS-150 driver covered the IPS3608 with only data in the definition).
3. **Research:** vendor docs, community reverse engineering, existing open-source tools. Weigh
   sources (a working tool's source code beats a blog post; in M4.4 a blog turned out to be
   templated content). Record sources in `sources`.
4. **Scaffold:** `desk model new vendor/model --capability … --name … --max-voltage … --max-current …`
   (or `--driver ../shared/x.lua`), then implement/fill in, `desk model check`.
5. **Fake-device test** (like `tests/model_fnirsi_dps150.rs`) before touching hardware.
6. **Find the device:** `desk port list` before/after plugging in or powering on (VID:PID, serial).
   Generic VIDs (e.g. Artery `2e3c:5740`) mean the driver must identify the model first.
7. **Probe read-only first:** a small diagnostic script (a `power` script binding with `port` and
   `ctx.serial`) that connects and dumps every frame as hex; then read-only queries (name,
   firmware, full status). **Diff status snapshots** around one state change to find fields: that's
   how the IPS3608's setpoint and output offsets were found (offset 107 = output) although the
   community docs didn't have them.
8. **The user's config:** `model = …`, their `power` path, `port.serial_number`, and
   conservative *test* `limits` (the smoke test runs through the user's config, which already
   holds the power path).
9. **Smoke test with the user:** nothing connected to outputs, low values within the test limits,
   each switching step confirmed by the user. Also mains off/on (boot behavior, output state
   after power-up). Record observed behavior in the vendor README; set `status = "tested"` with
   versions; replace the test limits with the user's real ones.
10. **Publish (optional, with consent):** a PR adding only `models/<vendor>/…`.

### Decisions

- **Shipping:** `desk skill` (still desk-operator), `desk skill list`, `desk skill <name>`,
  `desk skill install --global | --project | --dir <path>` (`~/.claude/skills`, `./.claude/skills`;
  on a terminal without a flag it asks: globally, this project, or not now). Writes
  `<dir>/<name>/SKILL.md`, replaces only older versions of desk's own skills unless `--force`,
  all-or-nothing. A global install costs every session only the skill descriptions (~130 tokens),
  so they're kept short. The future `curl | sh` installer just runs `desk skill install` (reading
  the answer from `/dev/tty`).
- **Self-sufficient without skills:** `desk --help` tells agents where to start (`desk schema
  --json`, `desk skill desk-operator`) and the core rules; a test requires help text on every
  command and argument.
- **Probe tool in desk:** yes, minimal: `desk port probe` (hex or `--text` sends, timestamped hex
  replies). It's raw (no limits), so the skills restrict it to documented read-only queries and
  forbid it for operating devices. Diffing snapshots stays the agent's job.
- **Fake-device tests:** Rust, in a clone's `tests/model_*.rs`; without a clone or Rust the
  skill says there's no fake test and relies on probing + smoke test.
- **Publishing:** skill-guided, consent on the exact file list first. `gh` if available; else git
  branch + push/compare-URL steps or `git format-patch`; without a checkout a tarball of the
  vendor folder. The upstream URL comes from the checkout's remote (none is set yet).
- **Capabilities:** the skill targets `voltage_source` and `power_switch`; anything else is reported
  as needing a core capability first.

**Done:**
- [x] `desk skill list|<name>|install`; skills registry in `src/cli/skill.rs`, tested against `skills/`
- [x] `desk skill install` scopes (`--global`, `--project`, `--dir`) with a picker on a terminal
- [x] `desk --help` guides agents without a skill; every command and argument has help (tested)
- [x] `desk port probe [path] [--usb VID:PID] [--serial-number] [--baud] [--line 8N1] [--send …]
      [--text] [--listen 1s]` (pty tests in `tests/serial.rs`)
- [x] `skills/desk-add-device/SKILL.md`: the workflow above, ground rules, driver essentials,
      publishing with and without `gh`
- [x] `desk schema` lists groups that take their own arguments (`desk skill [name]`); a test checks
      every skill mentions only real commands and error codes
- [ ] Validate the add-device skill end to end on a second real device (with the user)

## Roadmap to going public (agreed 2026-11-05)

1. **IPS3608 complete:** research the full protocol (every command, status field, push rate),
   probe the real unit read-only, then bring every feature it really has into desk: generic
   `voltage_source` extensions where they fit (e.g. CV/CC mode, protections, counters), driver and
   definition updated. The add-device skill gets an "improve an existing definition" section.
   - [x] Research (25+ sources, decompiles of the official app) and read-only probes on the unit:
         README corrected (offset 24 = temperature, 76–92 = protection thresholds, DF = hardware /
         E0 = firmware), full command table and status layout, never send type C0 (firmware mode)
   - [x] `voltage_source` `read()`: optional `mode` (cv|cc), `protection`, `temperature`,
         `input_voltage`, `charge` (Ah), `energy` (Wh); mode dropped while the output is off
   - [x] Core: a tripped protection refuses `output on` (`protection_tripped`, also for dry runs and
         trips right after switching on) unless `--clear-protection`; warnings on every read
   - [x] Driver: 50 ms quiet gap after writes (dropped writes), all status fields, input via query
         C0 (IPS3608); definitions: IPS3608 285 W + versions fixed, DPS-150 gets USB id, name check
         and status layout (still community)
   - [x] Fake-device tests: every field, the gap (fails without it), a latched trip; sim PSU with
         CV/CC and `tripping_*`; read verified on the real IPS3608
   - Not exposed, decided with the user (2026-11-05: not needed now; they write settings stored on
     the device): protection thresholds, presets, metering on/off and counter reset, brightness,
     volume. CC on the IPS3608 needs a load to confirm (whenever one is connected).
   - **Step 1 done.**
2. **Streaming (agreed 2026-11-05):** live readings for other programs (e.g. a LiPo charge
   logger), while desk and other programs keep controlling the device. desk itself never acts on a
   reading: the app decides, desk is the backend (limits still apply to every command).
   - **Daemon** (`desk serve`, started on demand by the first subscriber, exits when idle). It owns
     the device sessions; nothing else can open the port while it holds it. Local socket:
     `$XDG_RUNTIME_DIR/desk-operator/desk.sock` (0600) on Unix, a per-user named pipe on Windows.
   - **Protocol:** JSON lines. A request carries the same arguments as the CLI and gets the same
     `{ok,data,error}` envelope back; `watch` turns the connection into a stream of readings.
   - **CLI routing:** device commands go through the daemon when it runs, else in-process as today.
     So `desk source set psu 4.2v` works while something watches `psu`.
   - **`desk source watch <device> [--interval 100ms] [--count N] --json`:** one JSON line per
     reading with a wall-clock and a monotonic timestamp. Rate = what the device delivers (core
     loops `read()`; default 100 ms for every device, `--interval 0` = back to back; a definition
     default wasn't needed). A slow subscriber loses old readings and gets a `dropped` count;
     nothing is slowed down or hidden.
   - A session stays open while anyone watches (the IPS3608 locks its front panel meanwhile);
     commands run between readings. Last subscriber gone → session closed, panel free.
   - Example: a small script that logs a charge to CSV (Python, standard library only).
   - Tests: watch + set at once, two subscribers, dropped count, no daemon → in-process, daemon
     killed mid-watch → clear error, socket permissions; against the sim PSU and the fake IPS3608.
   - [x] Daemon: lock file per config + build, socket 0600 (short temp path if too long), idle
         exit, SIGTERM/Ctrl-C closes sessions; `daemon_unavailable` error code
   - [x] Sessions: one per watched device, watchers at their own interval, commands between
         readings, mains off/toggle ends it with `device_off`, per-device port lock
   - [x] `desk source watch`, CLI forwarding of `power`/`source`, socket protocol
         (`docs/streaming.md`), skill + schema updated
   - [x] Tests: `tests/daemon.rs` (sim PSU) and the fake IPS3608 (one connect for watch + set)
   - [x] Example `examples/watch_to_csv.py` (CSV log, optional switch-off below a current in CV)
   - [ ] Windows named-pipe code is written but not compiled yet (no Windows toolchain here; the
         release CI in step 4 must build it)
   - [x] Read-only smoke test on the real IPS3608: watch streams, port free afterwards; driver
         now reads exact frame lengths (112 → 55 ms per reading, ~18/s) and rounds float32 values
   - Later, not now: **captures** (scopes, logic analyzers): binary blocks moved by the core, never
     per sample through Lua or JSON; probably sigrok for analyzers, desk orchestrating.
3. **Open capabilities** (decided 2026-11-05, design in "M7" below): any device declares its own
   values, settings and actions; the core enforces generic safety. Replaces "fixed capabilities
   first". Includes the first new connection type (whatever the multimeter needs) and channels.
4. **Dogfood as a user (before going public):** install the local build (`cargo install --path .`,
   `desk skill install --global`), add the multimeter in a fresh session with only the installed
   skills (it's read-only, so the safest first device for an agent).
5. **Public:** license, release CI for Linux/macOS/Windows (also builds the Windows named-pipe code
   for the first time), `curl | sh` installer that offers `desk skill install`, then make the
   repo public.
   - [x] One private GitHub repo, `robin7331/desk-operator` (2026-11-06): the library is
     `models/`, bundled at build time, its tests are in `tests/`; the `desk-add-device` skill
     clones the repo, branches and opens pull requests there.
   - [ ] **Before making desk-operator public: clean its git history.** Early commits contain the
     user's PSU serial number (docs examples, M5) and a Home Assistant entity (a unit test).
     The library's merged history started clean.
   - [ ] CI: run `cargo test`, which includes the fake-device tests.

## M7 — Open capabilities (design, 2026-11-05; to be reviewed before building)

**Why:** desk should work with basically any device, not only the kinds its core knows. So far a
new kind of device (multimeter, DC load, soldering iron) meant new Rust code in desk itself, which
a user's agent can't add. From now on a **device definition declares what the model can do**, and
the core enforces generic rules based on those declarations. Mains power (`desk power`) stays
shared by every device.

### Model

- A **capability** is a named group of things a device can do, with one driver binding. Two kinds:
  - **well-known** (`power` = power_switch, `source` = voltage_source): the core defines the
    names and adds stricter rules on top of the generic ones (below). More can become well-known
    later when many devices share one (e.g. `meter`), but nothing has to. Only `power` keeps a
    command group of its own (`desk power`), because mains is shared by every device; everything
    else, power supplies included, uses the generic commands. **`desk source` is dropped**
    (decided 2026-11-05; nothing is public yet, so nobody depends on it).
  - **custom**: any other name. The definition declares everything.
- Inside a capability:
  - **value**: something the device reports (`temperature`, `voltage`, `weight`);
  - **setting**: something it reports and you can change (`setpoint`, `range`, `brightness`);
  - **action**: something it does on request, optionally with arguments (`standby`, `zero`,
    `reset_counters`).
- Types: `number` (with a unit), `boolean`, `choice` (a fixed list of strings), `string`.
- **A value whose unit changes** (a multimeter's reading is volts, ohms or amps depending on its
  function) declares the possible units, `units = ["V", "A", "Ω"]`, instead of one `unit`. The
  driver returns `{ value = 4.98, unit = "V" }` for it; the core checks the unit is one of the
  declared ones and reports both (also in `watch`). Only values can do this; a setting always has
  one fixed unit, so a typed value can be checked before anything is sent.

**A multimeter in this model** (illustrative names, not a real model):

```toml
[meter]
script = "acme_dmm.lua"
hid    = { usb_vid = 0x1234, usb_pid = 0x5678 }

[meter.values]
reading  = { type = "number", units = ["V", "A", "Ω", "Hz", "F", "°C"], doc = "the display" }
overload = { type = "boolean", doc = "OL on the display" }
function = { type = "choice", choices = ["dcv", "acv", "dca", "aca", "ohm", "hz", "cap", "temp"] }

[meter.actions]
hold = { switches = false, doc = "freeze the display" }
```

`function` is a value here because on most handheld meters only the rotary knob changes it; a
bench meter that can switch remotely declares it as a setting instead. `meter` may become
well-known later (fixed names for every meter) once several meters exist.
- A well-known capability may declare extra values, settings and actions too. That's how the
  IPS3608 can expose its protection thresholds or a counter reset later without core changes.

**`voltage_source` in the new model** (names fixed by the core, so every PSU looks the same):

| Kind | Names |
|---|---|
| settings | `voltage`, `current` (the setpoints), `output` (on/off) |
| values | `measured_voltage`, `measured_current`, `measured_power`; if the device reports them: `mode` (cv/cc), `protection`, `temperature`, `input_voltage`, `charge`, `energy` |

A setting's name is what you set; the measurements are `measured_…` (all three alike, so scripts
find them under one pattern). Mains keeps its own commands (`desk power on|off`, decided: no
rename to enable/disable or `mains`); where it appears in generic output (`desk get iron`), it is
listed as `mains`, never as `power`.
Its extra rules stay in the core, whichever command is used:
- the setpoints are applied before the output in one `set`, whatever order you typed them in;
- `output on` checks the setpoints already on the device against the limits;
- `output on` is refused while a protection is tripped, unless `--clear-protection` (a `set` flag,
  a usage error anywhere else).

### Definition format

Every top-level table other than `[device]` is a capability; its name is the key, as `[power]` and
`[source]` already are. A custom capability needs a `script` and at least one declaration, so a
typo like `[sorce]` is an error, not a new capability.

```toml
[heater]
script  = "iron.lua"
port    = { usb_vid = 0x1234, usb_pid = 0x5678 }
timeout = "5s"
params  = { address = 1 }

[heater.values]
temperature = { type = "number", unit = "°C", doc = "tip temperature" }
heating     = { type = "boolean" }

[heater.settings]
setpoint = { type = "number", unit = "°C", min = 50, max = 450, tolerance = 2, doc = "target" }
mode     = { type = "choice", choices = ["normal", "boost", "sleep"] }

[heater.actions]
standby = { switches = true, doc = "heater to standby temperature" }
boost   = { switches = true, args = { seconds = { type = "number", unit = "s", min = 1, max = 60 } } }
```

- `min` / `max`: the device's range (like ratings today), enforced by the core.
- `switches = true`: an action or setting with a physical effect (heats, outputs, moves). The
  schema shows it, agents ask first, `--dry-run` covers it. Settings default to `switches = true`,
  values can't switch, actions must say which (no default).
- `tolerance`: how far a read-back may differ and still count as confirmed (default: 0.5 % or
  the last digit the device reports, like `voltage_source`).
- Names are unique within a device; `heater.setpoint` is accepted where two capabilities clash.

On the user's side, nothing new to learn: the device merges into the capability by name, and
limits go per capability and setting.

```toml
[devices.iron]
model  = "acme/iron-t12"
power  = { provider = "ha", entity = "switch.desk_outlet_4" }
heater = { port = { serial_number = "…" } }
limits = { heater = { setpoint = 380 } }        # a number = maximum; or { min = …, max = … }
```

### Driver contract (Lua) for custom capabilities

```lua
function read(ctx)                 -- required if anything is readable: every value and setting at once
  return { temperature = 312.5, heating = true, setpoint = 330, mode = "normal" }
end
function set(ctx, name, value)     -- required if there are settings; value already checked + converted
function action(ctx, name, args)   -- required if there are actions; args checked + converted
function open(ctx) / close(ctx)    -- optional, as today (a session per command or per watch)
```

`read` returns everything in one round trip, because one call per value is slow over serial, and
`watch` needs a single call. Numbers come in the declared unit, so the core does the conversion.
Lua functions get `ctx.channel` when the capability has channels (below).

### Semantics in the core (the same for every custom capability)

1. Parse the arguments with the **unit table** (below): a number with a unit needs the unit
   (`330c`, `330°C`, `1.5ka`, `250ms`); a bare number only for unitless ones. Booleans take
   `on/off/true/false`, and choices must be one of the list.
2. Check the device range (`min`/`max`) and the user's `limits` **before anything is sent**
   (`limit_exceeded`, as today). Unknown names are a `usage` error that lists the valid ones.
3. Check mains first (`device_off`), then open a session (with timeouts, and close it on every
   path), as for `voltage_source`.
4. Settings: `set`, then `read`, then compare within `tolerance`, which gives `verified`
   per setting: true, absent (the device doesn't report it), or `verify_failed`.
5. Actions: `action`, then `read` if there is one, to report the state after.
6. `--dry-run` for every setting and action: check everything, send nothing.
7. Streaming: `watch` works for any capability with a `read`, through the daemon as now, and
   commands still run between readings.

What the core **can't** know is what a value means physically (that an iron shouldn't heat
unattended). The definition's `switches`, ranges and docs carry that; anything more specific stays
a well-known capability or later a driver `check` hook (below).

### Units

The core has one fixed table of known units. Definitions must use its symbols (`desk model check`
rejects `"C"` for `"°C"`); what you type may use the spellings in the table.

| Unit | Typed as | SI prefixes |
|---|---|---|
| V, A, W | `v`, `a`, `w` | yes |
| Ω | `ohm`, `r`, `Ω` | yes |
| Hz, F, H, g, Pa | `hz`, `f`, `h`, `g`, `pa` | yes |
| s | `s` | yes (`ms`, `us`, `µs`); plus `min` and `h` for minutes and hours |
| °C | `c`, `°c`, `degc` | no |
| K | `k` (kelvin) | yes, but `mk` etc. only (a bare `k` is kelvin, not kilo) |
| % | `%` | no |

- **Prefixes are case-sensitive** (`m` milli, `M` mega, `k` kilo, `u`/`µ` micro, `n`, `p`, `G`);
  **unit symbols are not** (`5V` = `5v`). So `1MΩ` and `1mΩ` differ, while `250mA` = `250ma`.
  (This changes `500MV`, which used to mean 500 mV; now it's 500 megavolts and refused by the
  range check, so it can't do harm.)
- Where a prefix and a unit look alike, the unit wins: `h` is hours or henry depending on the
  declared unit, which the core knows.
- **A unit the table doesn't know** (`ppm`, `%RH`, `dBm`) is allowed: it must be typed exactly as
  declared (case-insensitive) and takes no prefixes. Adding it to the table is a core change.
- Values come out in the declared unit, without prefixes (`0.25` A, never `250 mA`), so scripts
  never parse units.

### Commands

```sh
desk get   <device> [<name>...]                      # values + settings (all, or the named ones)
desk set   <device> <name> <value> [<name> <value>...] [--dry-run]
desk do    <device> <action> [<arg>=<value>...] [--dry-run]
desk watch <device> [<name>...] [--interval] [--count] [--changes]   # any capability
desk power on|off|toggle|status <device>            # unchanged: mains, every device
```

Decided: the command comes first, then the device, then the name (`desk set iron setpoint 330c`),
like `desk power on iron`. Not device first (`desk iron set …`).

Power supplies, before and after:

| Before (M4–M6) | After (M7) |
|---|---|
| `desk source set psu 5v 1a --on` | `desk set psu voltage 5v current 1a output on` |
| `desk source output off psu` | `desk set psu output off` |
| `desk source read psu` | `desk get psu` |
| `desk source watch psu` | `desk watch psu` |

Several settings in one `set` are applied in the definition's declaration order (well-known
capabilities: the core's order, e.g. setpoints before output), so the definition decides the safe
order, not the typing.

`watch` without names reports every value and setting of every capability, one line per reading.
With names it reports only those. `--changes` prints a line only when one of the watched values
changed (for things that rarely change: mode, protection, heating; a number counts as changed
when it moved by more than its `tolerance`, so measurement noise doesn't), otherwise one line per
interval (even spacing, for plots). The device is read in full either way; names only filter.

`<device>:<channel>` addresses a channel (`desk set psu:2 voltage 5v`). `desk schema` lists per
device every capability with its values, settings, actions, types, units, ranges, effective limits
and `switches`, so agents need no new skill per kind of device.

### Channels

`channels = 2` (or `channels = ["ch1", "ch2"]`) on a capability: every value, setting and action
exists per channel; the driver gets `ctx.channel`. Without a channel, `get` and `watch` report every
channel, while `set` and `do` require one. Limits can be per capability (all channels) or per
channel (`limits = { source = { voltage = 12, ch2 = { voltage = 5 } } }`). `voltage_source` gets
channels the same way (multi-channel PSUs).

### Connections (host API)

Drivers stay sandboxed: they can only open what their binding names, and the core opens it. Today
that's `port = { … }` (serial). New connection types become binding keys of their own:

| Key | For | Example devices |
|---|---|---|
| `port` (exists) | serial / USB CDC | FNIRSI PSUs, many meters via USB-serial |
| `hid = { usb_vid, usb_pid, serial_number }` | USB HID | cheap multimeters (UNI-T, …) |
| `tcp = { host, port }` | raw TCP, SCPI over LAN | Rigol, Siglent, Keysight |
| `usbtmc = { usb_vid, usb_pid, serial_number }` | USB test & measurement class | the same brands over USB |
| `http = { base_url }` | HTTP APIs | WLAN devices, Shelly-style gadgets |
| `ble = { address \| name }` | Bluetooth LE | some meters and irons |

Build only the one the first new device needs (the multimeter), each with a fake-device test
like `tests/serial.rs`. Host and address come from the user's config (definitions give ports and
USB ids only).

### Planned extensions (the format leaves room; not built in M7)

- **Rules across values:** an optional driver hook `check(ctx, request)` that can refuse a request
  before anything is sent (e.g. "volts × amps ≤ 100 W", "only heat while the fan runs"), and
  declarative ones where they're common (`max_power` exists for sources).
- **Events:** things that happen on the device (button pressed, alarm), as extra `watch` lines.
- **Captures:** large binary data (scope waveforms, logic analyzer captures, screenshots), moved by
  the core, not through Lua or JSON per sample.
- **Scenes / sequences** across devices (the command language in "After M6").

### Build order

1. [x] Format: custom capabilities and declarations parsed and checked (config, definitions,
   JSON schema, `desk model check`; `desk model new --capability <name>` writes a skeleton),
   merge, limits per capability/setting/channel.
2. [x] Core: unit table (`units.rs`), declarations and generic rules (`declared.rs`), catalog and
   ops (`ops.rs`); the mock simulates declared devices (`follow`, `fixed`, `last_action`,
   `stuck_`/`blind_`/`fail_`).
3. [x] Lua contract (`read`, `set`, `action`, `ctx.channel`); `docs/scripting.md`,
   `docs/model-format.md` (v2).
4. [x] CLI `get`, `set`, `do`, `watch` (`--changes`, names, channels); schema and `device show`
   list every name with type, unit, range, limit, `switches`; daemon routing. `voltage_source`
   runs on the generic commands with its rules intact; **`desk source` removed** (CLI, schema,
   skills, docs, example, tests).
5. [x] Skills: desk-operator uses the generic commands; desk-add-device declares capabilities and
   only stops for a missing connection type.
6. [x] Channels (declared and `source`), per-channel limits.
7. [ ] The multimeter's connection type (needs its model and interface).
8. [x] CLAUDE.md (core concepts: capability, value, setting, action).

Not built yet (in the design, no device needs it so far): extra values, settings and actions on
`source` (the IPS3608's protection thresholds would use this), declarative cross-value rules and
the `check` hook, events, captures.

### Open questions (for the user)

- Multimeter and DC load: exact models and how they connect.

## After M6 (rough order, re-evaluate each time)

1. **Config editing from the CLI:** `device add`, `device power <dev>` with interactive provider/entity pickers (`toml_edit`, `inquire`)
2. **Native Rust drivers** for common protocol families (SCPI, Modbus) next to Lua drivers
3. ~~More capabilities~~: replaced by open capabilities (M7); scope captures much later
4. **Command language:** `psu 5v 1a on, iron 330c then dmm dcv`, scenes, automatic "mains on → wait for port → configure"
5. **Palette + tray** (Tauri v2), with `desk palette` for Wayland compositor binds
6. ~~Daemon~~: moved to roadmap step 2 (streaming)
7. **MCP server** (`desk mcp`), then natural language with your own API key, always as a previewed plan
8. Shelly-direct provider

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
- **Daemon after all, for streaming (2026-11-05):** a subscriber needs a long-lived session while
  other commands keep working, and only one process can hold a serial port. One-shot sessions stay
  the default when nothing watches.
- **Open capabilities instead of a fixed list (2026-11-05):** desk should cover basically any
  device. Definitions declare values, settings and actions; the core enforces generic rules
  (units, ranges, limits, verify, dry-run, timeouts). Well-known capabilities keep stricter rules.
- **Left-out arguments list what's possible, as a usage error (2026-11-06, issue #1):** `desk set
  <device> [<setting>]` and `desk do <device> [<action>]` describe the choices, unit, range and
  limit, read the current value if they can, and change nothing. They still exit 2 with `ok: false`
  and the listing in `data`: `desk set psu voltage $EMPTY && desk set psu output on` must stop
  there. Mains is never switched for it; a device that can't answer just has no current value.
- **One command set for every device (2026-11-05):** `desk get|set|do|watch <device> <name> …`,
  command before device. `desk source` is dropped; PSU rules apply through the generic commands.
  `desk power` stays its own group (mains is shared by every device).
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
