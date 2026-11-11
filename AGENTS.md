# desk-operator

A cross-platform (Linux, macOS, Windows) background tool, written in Rust, that gives you **one
interface for the equipment on an electronics bench**: power supplies, DC loads, multimeters,
soldering irons, scopes, and switched mains outlets. It is free and open source, and contributors
can add support for their own gear.

The goal is to type something like `psu 5v on, iron 330c` (CLI first, later a hotkey command
palette, later natural language) and have it happen. That includes switching the mains outlet
first if the device is off.

**Status:** M0–M5.1 done: mains power via Home Assistant or Lua scripts, the `voltage_source`
capability (bench power supplies) with Lua drivers and serial ports, a set of models with a
specified format, agent-ready CLI (`--json`, `schema`, `skill`). Four models so far
(see *Supported devices* in `README.md`): `fnirsi/ips3608`, `fnirsi/dps-150`, `owon/xdm1041`,
`owon/oel1515`.
**M6, AI skills, done**: `desk skill list|install`, `desk port probe` and the `desk-add-device`
skill, validated when a fresh session added the XDM1041. Streaming is done: a daemon
(`desk serve`, started on demand) holds device sessions so `desk watch` streams while other
commands keep working (`docs/streaming.md`). **M7, open capabilities, done** (`plan.md`,
"M7"): models declare values, settings and actions; one command set for every device
(`desk get|set|do|watch`; `desk source` is gone); channels. **Public since 2026-11-09**, releases
`v0.1.0` to `v0.3.1`: CI, release builds, one-line installers and `desk update`. **M8, captures
and sigrok, done** (`plan.md`, "M8"): actions that produce a file (`produces = "file"`, a sink the
core owns), and sigrok as a connection kind (`sigrok-cli` run as a separate program,
`docs/sigrok.md`). **M8.1**: readings and settings through sigrok (`ctx.sigrok.measure|get|set`
in a driver's `read()`/`set()`). **M9.1, raw binary streams** (`plan.md`, "M9"): models declare
streams, a source fills a stream sink, the daemon fans its blocks out to subscribers over its
socket (`desk stream`, `docs/streaming.md`). Still open: M9.2 (activity),
validating the `desk-setup` skill on a real bench, and real installs and serial devices on macOS
and Windows (roadmap in `plan.md`).
Keep it lean: build only what the current milestone needs.

**One public repository**, two parts:
- **The core** (`src/`): it never names a vendor or model.
- **The models** (`models/`): definitions of what each product can do and their drivers, bundled into the binary by
  `build.rs`, with their fake-device tests in `tests/model_*.rs`. `models/README.md` is the
  contributor guide. A change to the model format or host API that breaks models needs a
  matching change there, in the same commit.

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
| **Model** | What's true for *every unit* of a product ("FNIRSI IPS3608"): driver, USB ids, maximums, docs, tests. Lives in the model library (`models/<vendor>/<model>/`). Never contains anything user-specific (no Home Assistant entities, no bench limits). |
| **Capability** | A named group of what a device can do, with one driver binding. **Well-known** ones have names and stricter rules fixed by the core: `power` (power_switch, mains) and `source` (voltage_source, bench power supplies). Any other name (`heater`, `meter`, …) is **declared**: the model lists its values, settings and actions, and the core applies its generic rules. |
| **Value / setting / action** | What a capability declares: a **value** is reported (`temperature`), a **setting** is reported and changeable (`setpoint`), an **action** is done on request (`standby`). Typed (`number` with a unit, `boolean`, `choice`, `string`), with ranges; `switches` marks a physical effect. |
| **Stream** | The fourth thing a capability declares: live data until stopped (`samples`), with a `content` (`logic` is well-known: `unit_size`, `samplerate`, `channels` in bit order; any other name is opaque) and arguments like an action's. The driver's `stream()` describes it once and fills a stream sink; the daemon cuts it into blocks and fans them out (`desk stream`). |
| **Power path** | How a device gets **mains** power. This is not the same as a PSU's **output** (its terminals), which is the `output` setting of `source`. Never mix "power" and "output". |
| **Capability binding** | One config key per capability: `power`, `source`, or the declared capability's name. Each is a native provider binding or a Lua script. |
| **Connection** | What a binding may open to reach its device: a serial `port` or a `sigrok` driver (run through `sigrok-cli`); other kinds (LAN, USBTMC, …) when a device needs one. A driver's connection is part of its capability binding. |
| **Provider** | A configured connection to something that can switch or control many things, such as Home Assistant, a Shelly, a USB relay board, an RF dongle, or a sigrok backend (a `sigrok-cli` program; stock `sigrok-cli` on PATH is the implicit `sigrok`). You configure it once. |
| **Endpoint** | One thing a provider can list and control, such as an HA entity `switch.desk_outlet_1`. Providers can list their endpoints, which is what makes pickers possible. |
| **Binding** | A device's power path. It is either **native** (`{ provider = "ha", entity = "…" }`) or **script** (`{ script = "x.lua" }`). |
| **Host API** | The Rust functions exposed to Lua scripts (providers, `sleep`, `log`, `clock`, serial ports, sigrok captures, readings and settings). This is the real plugin contract, so keep it small and stable. |

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

- **One command set for every device.** `desk get|set|do|watch|stream <device>[:<channel>] …` work on
  any capability; only mains has its own group (`desk power`). Never add commands per kind of
  device.
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
- **A connection kind lives in three places.** Its spec is one field of `Connections`
  (`src/connections.rs`), its host API is `src/script/<kind>.rs`, and a backend it runs through
  is a provider that says how its endpoints are used (`Provider::endpoint_use`). Generic code asks
  the provider, never its kind; `bench.rs` and `cli/models.rs` don't change for a new kind.
- **Secrets are never in the config file.** Tokens come from an env var or a separate `token_file`
  (keyring later), and are never printed or logged.
- **The core never knows a specific model.** Nothing in `src/` names a vendor or model or reads the
  repo's `models/` folder (`build.rs` bundles it; a test enforces both). A driver that needs
  something the API can't do reveals a gap in the API; fix it generically.
- **Cross-platform.** No platform-specific code without `cfg`-gated alternatives. Note: global
  hotkeys don't work on Wayland. The UI will expose a `desk palette` command for compositor
  keybinds.

## Tech stack

- Rust (stable, 2024 edition), single binary named `desk` (crate `desk-operator`).
- `clap` (derive), `clap_complete` (feature `unstable-dynamic`: review the shell scripts when
  upgrading it), `tokio`, `reqwest` (rustls, json), `serde`, `toml` (+ `toml_edit` when the CLI
  edits config), `thiserror` (`DeskError`) everywhere; parse helpers return `String` errors that
  callers wrap with context, `directories` for config paths, `inquire` for TTY pickers.
- `mlua` with features `lua54`, `vendored`, `async`, `serialize`. Host functions are async and use
  tokio (`sleep` must be `tokio::time::sleep`, never a thread sleep).

## Layout (one crate; split into a workspace only when a UI needs a shared library)

```
src/
  main.rs              entry: clap parse, tokio runtime, exit code
  cli/                 clap definitions (mod.rs) + one handler module per command group;
                       schema.rs generates `desk schema` from the clap tree
  output.rs            Report, --json envelope, human tables
  error.rs             DeskError: stable `code()` + `exit_code()` + `hint()` (part of the CLI contract)
  config.rs            load/validate desk.toml, Device, PowerBinding, device resolution
  connections.rs       Connections: what a script binding may open (`port`, `sigrok`), their
                       parsing, model check and the backends (providers) they run through
  model.rs             Capability, Power, PowerState, PowerFeatures
  power.rs             PowerSwitch trait + switch semantics (idempotent, read-back verification)
  source.rs            VoltageSource trait + PSU semantics (limits, setpoints before output, protections)
  declared.rs          declarations (values/settings/actions), typed values, generic rules, Driver trait
  units.rs             the unit table: parsing `330c`, `250mA`, `1.5kΩ`; formatting
  capture.rs           actions that produce a file: the sink (private temp folder, size cap,
                       rename, never overwrite), capture folder, `--output`/`--max-size`, cancel
  stream/              streams: the description (`logic` rules), the stream sink (blocks of whole
                       units, flushed when full or 5 ms old), the fan-out with per-subscriber
                       queues and drop counts; frame.rs = the `desk-stream-1` frame codec
  deadline.rs          timeouts that a host function filling a capture can suspend
  ops.rs               a device's catalog of names, get/set/do ops (resolved + checked), running them
  bench.rs             Config + provider Registry; resolves bindings, applies timeouts
  serial.rs            PortSpec matching, `desk port list`, async Port handle (exclusive opens);
                       `desk port probe` (raw hex in/out) lives in cli/port.rs
  sigrok.rs            sigrok as a connection kind: the `sigrok = { … }` binding, backends (the
                       implicit `sigrok-cli` on PATH, `kind = "sigrok"` providers), resolution,
                       running sigrok-cli (the checked command lines of `acquire`, `measure`,
                       `get`, `set`; parsing readings and values); `desk sigrok
                       drivers|show` lives in cli/sigrok.rs, the provider in providers/sigrok.rs
  models.rs           models: search path, `model.toml` parsing, merge, bundled files
  providers/           Provider trait, lazy Registry, implicit providers, Params helper; mock.rs,
                       home_assistant.rs, sigrok.rs
  script/              mlua engine, sandbox, host API (`ctx`), timeout hook; one file per
                       connection kind's host API: serial.rs (`ctx.serial`), sigrok.rs (`ctx.sigrok`)
  daemon/              `desk serve`: local socket (ipc.rs), JSON-lines protocol, CLI forwarding,
                       `watch` streams; session.rs = live device sessions + per-device lock;
                       hub.rs = one running binary stream per device (start, join, refuse, end)
  cli/ops.rs           `desk get`, `desk set`, `desk do` (rendering included); left-out arguments
                       list what's possible and end as a usage error
  cli/stream.rs        `desk stream`: the client of a binary stream (payload, `--header`,
                       `--frames`, `--discard`, `--duration`, `--bytes`); prints its own output
  cli/complete.rs      `desk completions <shell>` and the candidates behind Tab (config and
                       models only, never a device)
  cli/update.rs        `desk update`: download, verify, swap the binary, refresh global skills
                       (only release builds, marked by `DESK_RELEASE_BUILD`; else --force)
examples/desk.toml     sample bench on the mock provider (+ examples/scripts/*.lua)
examples/sigrok/       sample bench on stock sigrok's `demo` driver (needs sigrok-cli)
examples/stream_edges.rs  an example consumer of `desk stream --frames` (std only): edges per channel
docs/scripting.md      user-facing Lua host API reference: keep it in sync with src/script/
docs/streaming.md      `desk watch`, `desk stream` (header, frames, drops, throughput), the daemon
                       and its socket protocol (for app authors)
docs/sigrok.md         sigrok for users: getting it, vendor builds, backends, troubleshooting
docs/model-format.md  model format (spec); schemas/ has its JSON Schema (tested
                       against every model in tests/models.rs)
skills/<name>/         SKILL.md per agent skill (desk-operator, desk-setup, desk-add-device), embedded via
                       include_str! in src/cli/skill.rs (`desk skill list|<name>|install`); update
                       them when commands, error codes or semantics change (tests/cli.rs checks
                       commands and codes, and that every skills/ folder is registered)
tests/cli.rs           end-to-end tests of the binary against the mock provider
tests/home_assistant.rs  end-to-end tests against a stateful fake HA (wiremock)
tests/scripts.rs       end-to-end tests of Lua bindings (sandbox, timeouts, errors, logs)
tests/source.rs        end-to-end tests of voltage_source against the simulated PSU
tests/daemon.rs        watch + daemon: streaming, commands during a watch, endings, the socket
tests/streams.rs       `desk stream` against the mock source: header, pattern, frames, joiners,
                       refusals, a slow subscriber, endings, busy devices, idle exit, Lua drivers
tests/declared.rs      declared capabilities: units, ranges, limits, verify, channels, Lua drivers
tests/captures.rs      actions that produce a file: defaults, where files go, size cap, timeouts,
                       cancel (Ctrl-C, daemon client gone), dry runs, Lua drivers and ctx.output
tests/serial.rs        ctx.serial over pseudo-terminal pairs (Linux); FORK_LOCK avoids fd inheritance
tests/sigrok.rs        sigrok against a fake sigrok-cli: backends, `desk sigrok`, bindings,
                       resolution, ctx.sigrok.acquire (exact command lines, checks, failures,
                       timeout, size cap, cancel, one user per device); one test uses the real
                       `sigrok-cli -d demo` when it is installed
tests/sigrok_streams.rs streams from sigrok (`acquire{ continuous = true }`): command line, stdout,
                       two subscribers, no child left; the real `demo` driver where installed
tests/sigrok_values.rs ctx.sigrok.measure|get|set: meters and supplies through `desk get|set|watch`,
                       exact command lines, checks, failures, timeouts; the real `demo` meter
tests/update.rs        `desk update` against a fake release (wiremock); swapping the binary on Unix
tests/models.rs       model = "…", merging, search path, desk model list|show|check
tests/model_checks.rs       every model passes the check, and the README lists it with its status
tests/model_*.rs      one file per protocol family: a driver against a fake device on a pty (Linux)
tests/support/models.rs  helpers for those: this checkout's desk with models/ on its search path
tests/support/sigrok.rs  a bench with a fake sigrok-cli backend (for tests/sigrok.rs and sigrok
                       models); tests/support/fake_sigrok_cli.rs is that fake, a Cargo example
build.rs               bundles models/<vendor>/… into the binary (`bundled:<path>`)
install.sh, install.ps1  one-line installers: download a release, verify it, offer the skills
.github/workflows/     ci.yml (fmt, clippy, tests on all three OSes); release.yml (builds, runs the
                       installers against them; a `v*` tag publishes, a manual run only tests);
                       badges.yml (counts archive downloads for the README badge, `badges` branch)
about.toml, about.hbs  `cargo about`: the accepted dependency licenses (a new one fails CI), and the
                       THIRD-PARTY-LICENSES.txt that every release archive carries
models/                every model and its driver, vendor READMEs (models/README.md)
```

## Development

```sh
cargo build
cargo test                       # must pass without hardware or network (mock provider)
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo run -- --config examples/desk.toml power status
```

The core's own tests never depend on which models exist (`tests/models.rs` picks
whatever is bundled). A new device adds its row to *Supported devices* in `README.md`.

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
