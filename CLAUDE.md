# desk-operator

A cross-platform (Linux, macOS, Windows) background tool, written in Rust, that gives you **one
interface for the equipment on an electronics bench**: power supplies, DC loads, multimeters,
soldering irons, scopes, and switched mains outlets. It is free and open source, and contributors
can add support for their own gear.

The goal is to type something like `psu 5v on, iron 330c` (CLI first, later a hotkey command
palette, later natural language) and have it happen. That includes switching the mains outlet
first if the device is off.

**Status:** M0–M5.1 done: mains power via Home Assistant or Lua scripts, the `voltage_source`
capability (bench power supplies) with Lua drivers and serial ports, a device library with a
specified definition format, agent-ready CLI (`--json`, `schema`, `skill`). One real device is
supported and smoke-tested (`fnirsi/ips3608`). **Next: M6, AI skills** (see `plan.md`, "M6").
Keep it lean: build only what the current milestone needs.

**Working agreements:**
- Develop and verify the generic tool with mocks and fakes. Real hardware is at most a short
  smoke test, and only with the user's explicit go-ahead for each step that switches something.
- Commit only when the user asks (they usually say so at the end of a milestone).
- Never put anything bench-specific (Home Assistant entities, serial numbers, the user's limits)
  into the repo: that lives in the user's `~/.config/desk-operator/desk.toml`.
- Don't guess facts about devices (ratings, URLs, protocol details): leave them out or verify.

## Core concepts (use these terms consistently)

| Term | Meaning |
|---|---|
| **Device** | Something on *one user's* bench, such as `psu` or `iron`, in their `desk.toml`. It has an id, a name, aliases, capabilities, and the user's own bindings (e.g. their power path) and `limits`. |
| **Device definition** | What's true for *every unit* of a model ("FNIRSI IPS3608"): driver, USB ids, maximums, docs, tests. Lives in the device library `devices/<vendor>/<model>/`. Never contains anything user-specific (no Home Assistant entities, no bench limits). |
| **Capability** | A standard, vendor-neutral interface defined by the core: `power_switch` (MVP); later `voltage_source`, `electronic_load`, `meter`, `heater`. |
| **Power path** | How a device gets **mains** power. This is not the same as a PSU's **output** (its terminals), which belongs to `voltage_source`. Never mix "power" and "output". |
| **Capability binding** | One config key per capability: `power` (power_switch), `source` (voltage_source); later `load`, `meter`, … Each is a native provider binding or a Lua script. |
| **Control path** | How a device is commanded (serial, USBTMC, LAN, …). Post-MVP. |
| **Provider** | A configured connection to something that can switch or control many things, such as Home Assistant, a Shelly, a USB relay board, or an RF dongle. You configure it once. |
| **Endpoint** | One thing a provider can list and control, such as an HA entity `switch.desk_outlet_1`. Providers can list their endpoints, which is what makes pickers possible. |
| **Binding** | A device's power path. It is either **native** (`{ provider = "ha", entity = "…" }`) or **script** (`{ script = "x.lua" }`). |
| **Host API** | The Rust functions exposed to Lua scripts (providers, `sleep`, `log`, later serial/USB). This is the real plugin contract, so keep it small and stable. |

### Power bindings

```toml
[devices.psu.power]               # native: the core calls the provider directly
provider = "ha"
entity   = "switch.desk_outlet_1"

[devices.lamp.power]              # script: the core runs Lua on(ctx) / off(ctx) / state(ctx)
script  = "scripts/lamp.lua"      # relative to the config dir
timeout = "15s"
params  = { entity = "switch.desk_outlet_2" }   # passed to the script as ctx.params
```

A script operation succeeds when the function returns. It fails if it raises `error(...)` or hits
its timeout. The CLI waits for the result. Scripts are optional: a native binding is the simple
default.

## Architecture principles

- **CLI first, agent-operable.** Everything the UI will do is a CLI command. Every command supports
  `--json`, with a stable `{ "ok": true, "data": … }` / `{ "ok": false, "error": { "code", "message" } }`
  envelope. Commands return serializable structs, and the human output is just a rendering of them.
- **Interactive only on a TTY.** When arguments are missing and stdin is a TTY, offer pickers. When
  it isn't, fail with a message that names the command that lists valid values.
- **Honest state.** A power state is `on | off | unknown`, with a confidence of `real` (read back)
  or `assumed` (send-only providers). Report partial failures (for example "mains on, verify
  failed"); never pretend something succeeded.
- **Timeouts on all I/O.** A missing device must never hang the CLI.
- **Safety lives in the core**, below every frontend and script. It is never only in the UI or the
  agent skill.
- **The core owns hardware.** Scripts get handles from the host API. They never open arbitrary
  ports or network connections themselves (the Lua stdlib is sandboxed: no `io`, `os`, `package`
  or `debug`).
- **Secrets are never in the config file.** Tokens come from an env var or a separate `token_file`
  (keyring later), and are never printed or logged.
- **The core never knows a specific model.** Nothing in `src/` names a vendor or model or reads the
  repo's `devices/` folder (`build.rs` bundles it; a test enforces both). A driver that needs
  something the API can't do reveals a gap in the API; fix it generically.
- **Cross-platform.** No platform-specific code without `cfg`-gated alternatives. Note: global
  hotkeys don't work on Wayland. The UI will expose a `desk palette` command for compositor
  keybinds.

## Tech stack

- Rust (stable, 2024 edition), single binary named `desk` (crate `desk-operator`).
- `clap` (derive), `tokio`, `reqwest` (rustls, json), `serde`, `toml` (+ `toml_edit` when the CLI
  edits config), `anyhow` at the CLI edge, `thiserror` in core modules, `directories` for config
  paths, `inquire` for TTY pickers.
- `mlua` with features `lua54`, `vendored`, `async`, `serialize`. Host functions are async and use
  tokio (`sleep` must be `tokio::time::sleep`, never a thread sleep).

## Layout (MVP: one crate, split into a workspace only when the daemon/UI arrives)

```
src/
  main.rs              entry: clap parse, tokio runtime, exit code
  cli/                 clap definitions (mod.rs) + one handler module per command group;
                       schema.rs generates `desk schema` from the clap tree
  output.rs            Report, --json envelope, human tables
  error.rs             DeskError: stable `code()` + `exit_code()` + `hint()` (part of the CLI contract)
  config.rs            load/validate desk.toml, Device, PowerBinding, device resolution
  model.rs             Capability, Power, PowerState, PowerFeatures
  power.rs             PowerSwitch trait + switch semantics (idempotent, read-back verification)
  source.rs            VoltageSource trait + semantics (limits, setpoints before output, verify, sessions)
  bench.rs             Config + provider Registry; resolves bindings, applies timeouts
  serial.rs            PortSpec matching, `desk port list`, async Port handle (exclusive opens)
  library.rs           device definitions: search path, `device.toml` parsing, merge, bundled files
  providers/           Provider trait, lazy Registry, Params helper; mock.rs, home_assistant.rs
  script/              mlua engine, sandbox, host API (`ctx`), timeout hook
examples/desk.toml     sample bench on the mock provider (+ examples/scripts/*.lua)
docs/scripting.md      user-facing Lua host API reference: keep it in sync with src/script/
docs/definition-format.md  device definition format v1 (spec); schemas/ has its JSON Schema (tested
                       against every definition in tests/library.rs)
skills/desk-operator/  SKILL.md for agents, embedded via include_str! (`desk skill`); update it when
                       commands, error codes or semantics change (tests/cli.rs checks codes)
tests/cli.rs           end-to-end tests of the binary against the mock provider
tests/home_assistant.rs  end-to-end tests against a stateful fake HA (wiremock)
tests/scripts.rs       end-to-end tests of Lua bindings (sandbox, timeouts, errors, logs)
tests/source.rs        end-to-end tests of voltage_source against the simulated PSU
tests/serial.rs        ctx.serial over pseudo-terminal pairs (Unix); FORK_LOCK avoids fd inheritance
tests/device_*.rs      device definitions against fake devices over ptys (belong to the library)
tests/library.rs       model = "…", merging, search path, desk library list|show|check
build.rs               bundles devices/ into the binary (`bundled:<path>`)
devices/               device library: <vendor>/<model>/device.toml + shared protocol drivers (see devices/README.md)
```

## Development

```sh
cargo build
cargo test                       # must pass without hardware or network (mock provider)
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo run -- --config examples/desk.toml power status
```

Testing without hardware: the `mock` provider stores state in `$DESK_MOCK_STATE` (tests point it
at a temp dir). Endpoint prefixes simulate failures: `fail_` (unavailable), `stuck_` (verify fails),
`blind_` (no readback); `latency = "…"` on the provider simulates slow I/O.

Conventions:
- No `unwrap()`/`expect()` outside tests.
- Every new provider or host-API function gets a test (mock or `wiremock`).
- Don't add dependencies or abstractions that the current milestone doesn't need.
- Update `plan.md` checkboxes when you finish a milestone item.
- New error variants need an `ERROR_CODES` entry (`src/error.rs`, enforced by a test) and a row in
  SKILL.md's failure table.
