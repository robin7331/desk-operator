---
name: desk-add-device
description: Add a bench device that the `desk` CLI doesn't support yet, or improve an existing model: clone desk-operator, research the protocol, write and test the model, smoke-test it with the user, configure it, and open a pull request. Use when desk should control a new instrument or more of one.
---

# Adding a device to `desk`

Two different things come out of this, and they never mix:

- A **model** (`<vendor>/<model>/model.toml` + a Lua driver) holds what's true for
  **every unit** of the model: driver, USB ids, ratings, what it can do, sources. It belongs in the
  `models/` folder of **desk-operator**, and may be contributed there.
- The **device** in the user's `desk.toml` holds **their bench**: name, aliases, how it gets mains
  power, which unit it is (serial number), and their `limits`. It is never published.

For operating the bench (`desk power`, `desk get`, `desk set`, `desk do`, `desk watch`), follow the
`desk-operator` skill (`desk skill desk-operator`). Pass `--json` to every `desk` command.

## Ground rules

1. **Don't guess facts about devices.** Ratings, USB ids, frame layouts, timings: from a source
   or observed on the device, otherwise leave them out. A missing `max_power` is fine; a wrong one
   is not.
2. **Every step that switches something needs the user's go-ahead for that step:** mains on/off,
   setpoints, output on/off. Say what you'll send and what you expect to happen, then wait for
   their OK.
3. **Nothing connected to the outputs** while you develop and smoke-test. Ask the user to confirm
   this before the first command that reaches the device.
4. **Probe with read-only queries only:** frames the sources document as queries (name, version,
   status), or standard identification queries (SCPI `*IDN?`). Never scan or fuzz unknown command
   bytes: an undocumented command can switch an output or overwrite calibration. `desk port probe`
   sends raw bytes and checks no limits, so it is for this skill only, never for operating devices.
5. **Safety stays in desk:** the model's ranges (`ratings`, `min`/`max`) are the model's,
   the user's `limits` are their bench limits. Drivers never enforce or work around either. Never
   raise a user's `limits` unless they ask.
6. **Honest status:** `status = "community"` until the smoke test passed on real hardware. Drivers
   return only what the device really reports (`read()` fields it can't read stay `nil`, so desk
   says "not confirmed").
7. **Never copy bench things into a model** (entity ids, serial numbers, limits, paths).

## Where to work: a clone of desk-operator

One repository on GitHub, **`robin7331/desk-operator`**: desk itself and, in `models/`, every model, its driver and the vendor `README.md`. The fake-device tests are
`tests/model_<vendor>_<family>.rs`, the model format is `docs/model-format.md`, the Lua
host API is `docs/scripting.md`. `models/README.md` and `AGENTS.md` are the contributor guide; read
them first.

Before anything else, get a clone (ask the user where, if they have a preference):

1. **Find an existing one:** a checkout of desk-operator (`git -C <dir> remote -v`). Bring it up to
   date (`git -C <dir> pull`).
2. **Otherwise clone it.** With write access (the user owns it or is a collaborator), clone
   directly; otherwise fork first (`gh repo fork robin7331/desk-operator --clone`).
3. **Branch:** `git -C <clone> switch -c device/<vendor>-<model>`.
4. **Use its models while developing:** pass `DESK_MODELS=<clone>/models` to your own desk commands,
   and check with `desk model list --json` that `origin` is the clone, not `bundled`. This is for
   development only. It exists only in your shell, so never tell the user to set it; steps 8 and 10
   leave them a copy that works without it.

**No git or no network:** a model can still live in a plain `models/` folder next to the
user's `desk.toml` (the default of `desk model new`), and desk uses it right away. There's nothing to
copy then, but also no fake-device test: say so, rely on read-only probing and the smoke test, and
pack the folder for the user at the end (step 11).

## Workflow

### 1. Ask

- The **exact model** (and hardware/firmware version if the user knows it).
- **What it can do.** Any device fits: a **power supply** uses the well-known `source` capability
  (`voltage_source`: fixed names and stricter rules in desk). **Anything else** (meter, load, iron,
  sensor, relay board, …) gets a **declared capability**: the model declares its `settings`,
  `values` and `actions`, and desk applies its generic rules to them (see *Declaring what it can
  do*). Large data (scope waveforms, logic analyzer captures) isn't supported yet: say so.
- **Only switched on/off?** If the user just wants mains on/off (e.g. via a Home Assistant outlet),
  no model is needed: add a `power` binding to their device and you're done
  (`desk provider endpoints <id> --search …` finds the entity).
- **How it connects:** USB (CDC-ACM serial, USB-serial adapter, HID, USBTMC), RS-232, LAN,
  Bluetooth. desk drivers talk serial today (`ctx.serial`, `port = { … }`). Any other connection
  is a gap in desk itself: see *When desk itself is missing something*.

### 2. Check the existing models first

Someone may already have done this. Before writing anything, check the clone's models below, then
whether the user's desk is out of date (`desk update --check --json`), and search open work:

```sh
gh pr list -R robin7331/desk-operator --state all --search "<vendor> <model>"
gh issue list -R robin7331/desk-operator --state all --search "<vendor> <model>"
```

If that finds a pull request or issue for this device, show it to the user and let them choose:
build on it, or test it on their real device and report back. Don't start a second driver for it.
A model that already exists: continue with *Improving an existing model* instead. Without `gh`, say
that open work couldn't be checked.

```sh
desk model list --json
desk model show <vendor/model> --json   # bindings, driver params, status, sources
```

A family driver may already fit: the IPS3608 needed only data (`params`) on top of the DPS-150
driver. Vendors often ship one PC application for several models; that's a strong hint they share
a protocol.

### 3. Research

Look for, in this order of trust: the vendor's protocol documentation or programming manual;
**source code of a working tool** for this model; community reverse engineering with captures; forum
posts. A tool someone runs against the device beats a write-up, and some "protocol" blog pages are
generated filler: cross-check every frame and rating against a second source. Record the sources
you used (URLs) for the model's `sources` and the vendor README. Note what's unknown.

### 4. Scaffold

```sh
desk model new <vendor>/<model> --capability voltage_source --name "<Vendor Model>" \
    --max-voltage <V> --max-current <A> [--driver ../shared/<family>.lua] [--dir <clone>/models]
desk model new <vendor>/<model> --capability <name> --name "<Vendor Model>" --dir <clone>/models   # meter, heater, …
desk model check <vendor>/<model> --json
```

`new` writes `model.toml`, a driver skeleton with every required function (unless `--driver`) and
a vendor `README.md`. Fill in `sources`, `port = { usb_vid, usb_pid }` (when known), `params`, the
declarations (declared capabilities), and implement the driver (see *Driver essentials*). Rerun
`check` until it passes.

### 5. Fake-device test (in the clone)

Before touching hardware, run the driver against a fake device over a pseudo-terminal, like
`tests/model_fnirsi_dps150.rs`: a thread plays the device on the pty master side, `desk` opens
the slave as its port. Cover what the sources say: replies, missing echoes, pushed telemetry,
garbage before a frame, a silent device, a wrong model. The tests run the clone's own `desk` with
its files (`harness::desk()`, `harness::root()` from `tests/support/models.rs`). They need Rust
(`cargo`); without it, say so and skip to step 6. Then:

```sh
cd <clone>
cargo test                                   # every model's check + all fake-device tests
cargo clippy --all-targets -- -D warnings && cargo fmt
```

### 6. Find the device

Run `desk port list --json` before and after the user plugs it in or powers it on; the new entry is
the device (path, USB VID:PID, serial number). If the outlet is already a desk device, mains on is
`desk power on <device>`, with the user's OK. **Generic USB ids** (e.g. a microcontroller vendor's
default id such as Artery `2e3c:5740`) are shared with unrelated gadgets: then the driver must
identify the model (name query) before it sends anything else, and the user binds by
`serial_number`.

### 7. Probe read-only

Confirm with the user that nothing is connected to the outputs, then send documented queries:

```sh
desk port probe --usb <vid:pid> --baud <baud> --listen 2s --json            # just listen: pushed data?
desk port probe --usb <vid:pid> --baud <baud> --send "<name query hex>" --json
desk port probe <path> --text --send "*IDN?\n" --json                        # SCPI-style text
```

`data.steps[]` has one entry per `--send`: `sent`, `received` (hex), `ascii`, and `chunks` with
arrival times (bursts split by silence; shows push intervals). Include a protocol's connect and
disconnect frames in the same call when it needs them (one call is one port session).

**Find fields by diffing snapshots:** take a status reply, let the user change **one** thing on the
device (front panel: set e.g. 7.77 V, or switch the output; their OK first), take another, compare
byte by byte. Distinctive values make fields easy to find: search the reply for the value's
encoding (e.g. float32 little-endian). That's how the IPS3608's setpoint and output offsets were
found when no documentation had them. Note anything else you learn (boot time, locked front panel).

### 8. Configure (with test limits)

**First copy the model next to the config,** because a `model = …` desk can't find stops every desk
command for the whole bench, in every terminal. The folder is `models/` next to `desk.toml` (path:
`data.config.path` of `desk schema --json`). Copy `<vendor>/<model>/` from the clone, and anything it
refers to, like a driver in `<vendor>/shared/`. Copy only that, not the whole vendor folder, which
would override the vendor's other models too.

This copy wins over `DESK_MODELS`, so **copy again after every change** to the model or driver, or
your tests keep running the old one.

Then add the device to `desk.toml`. Ask the user for the id, name, power path, and
**conservative test limits**:

```toml
[devices.psu2]
model  = "<vendor>/<model>"
power  = { provider = "ha", entity = "switch.desk_outlet_3" }   # from `desk provider endpoints`
source = { port = { serial_number = "…" } }                     # from `desk port list`
limits = { source = { voltage = 5, current = 0.5 } }            # test limits, agreed with the user

[devices.iron]
model  = "<vendor>/<model>"
heater = { port = { serial_number = "…" } }                     # the capability's name
limits = { heater = { setpoint = 200 } }                        # a number = maximum, or { min, max }
```

Check with `desk device show <id> --json`.

### 9. Smoke test with the user

Values come from the user: propose low ones (e.g. 3.3 V, 100 mA) and use them only after they
agree. One step at a time, each confirmed:

1. `desk get <device> --json`: read-only. Does everything match the front panel/display?
2. Power supplies: `desk set <device> voltage 3.3v current 100ma --json` with the output off:
   `data.verified` true? The display? Then `desk set <device> output on --json` and `desk get`:
   `measured_voltage` ≈ setpoint (no load)? Then `desk set <device> output off --json`.
   Declared capabilities: each setting once (`desk set <device> <setting> <value> --json`, a
   harmless value), each action once (`desk do <device> <action> --json`), the user's OK for each.
3. `desk watch <device> --count 10 --json`: do values change as the display does?
4. Mains off, then on (`desk power off|on <device>`): state after power-up? Settings kept? How
   long until the port answers?

Fix the driver (and its fake-device test) for anything that differs, then repeat the affected step.

### 10. Finish

- In the model: `status = "tested"`, `tested = ["firmware … / hardware … (Mon YYYY)"]` (read
  the versions from the device if it reports them), `sources`.
- In the vendor `README.md`: protocol notes, sources, known gaps, **observed behavior** (boot,
  power-up state, ramps, echoes).
- `desk model check --json` passes.
- Ask the user for their real bench `limits` and replace the test limits.
- In the vendor's row of *Supported devices* in the clone's `README.md`: its status (add the row for
  a new device, sorted by vendor and model).
- In the clone: `cargo test` and clippy pass.
- **Leave a working install.** Copy the final model next to `desk.toml` again (step 8). Then check it
  the way the user will run it, in a shell without `DESK_MODELS` (`env -u DESK_MODELS` on Linux and
  macOS, `Remove-Item Env:DESK_MODELS` in PowerShell): `origin` in `desk model list --json` must start
  with `local`, and `desk get <device> --json` must work. Also ask the user to run `desk status` in a
  new terminal of their own.
- **Tell the user about the copy:** where it is, that it overrides a bundled model of the same name,
  and to delete it once a desk release includes the model. `desk model list` and `desk update` point
  it out then.

### 11. Pull request (only with consent)

Ask whether the user wants to contribute the model to desk-operator. Show exactly
what would go in: `models/<vendor>/…` (model, driver, README), `tests/model_<vendor>_<family>.rs`
and the `README.md` row, **never** their `desk.toml` or anything from it. Check the diff (`git -C <clone> diff main`) for
their serial number, entity ids and paths. Nothing leaves the machine before the user agreed to
this file list. Then, on the branch from *Where to work*:

- **`gh` installed and logged in** (`gh auth status`): commit, push (`git push -u origin <branch>`;
  from a fork, `origin` is the fork), and `gh pr create -R robin7331/desk-operator` with
  what the model supports, the tested versions, what the smoke test verified, the sources,
  and the known gaps.
- **git but no `gh`:** commit, push to their fork, and give the user the compare link the push
  prints. Or create a patch with `git format-patch main -o <dir>` for them to send.
- **No clone:** pack the vendor folder
  (`tar czf <vendor>-<model>.tar.gz -C <models dir> <vendor>`) and tell the user to attach it to
  an issue on desk-operator.

Until a desk release includes the model, their bench uses the copy next to `desk.toml`, whatever
happens to the clone or its branch. Merging doesn't change that: only a release does.

## When desk itself is missing something

Some devices need more than a model: a connection desk can't open yet (HID, USBTMC, LAN,
Bluetooth), a host API function, a kind of declaration desk doesn't have, extra settings or
actions on `source`. **Never work around it in the driver** (no side channels in `params`, no
abusing another capability). Instead:

1. Stop and tell the user what's missing, why the device needs it, and what it would take
   generically (for every device that connects this way, not just this model).
2. Check whether it's known: `gh issue list -R robin7331/desk-operator --search "<keywords>"`.
3. With the user's OK, open an issue there (`gh issue create -R robin7331/desk-operator`):
   the gap, the device that needs it, what the protocol looks like, sources. Generic only: no
   bench details (no serial numbers, entity ids, limits).
4. If the user wants it built now: that's work in a clone of desk-operator, following its
   `AGENTS.md` (mocks and fakes first, tests, its own pull request). Finish the model after
   desk can do what it needs, on the same branch or one built on top of it.

Until then, the model can still cover what desk can do already (for example the values over
serial), with the gap noted in the vendor `README.md`.

## Improving an existing model

Same workflow, starting from `desk model show <vendor/model>` instead of `desk model new`:
research, probe, update the driver and its fake-device test, smoke-test, update the vendor README,
then a pull request (step 11), all in the clone.

- **`community` → `tested`:** someone with the device runs the smoke test (step 9); then set
  `status` and `tested`.
- **New firmware or hardware revision** behaves differently: record it in the README, fix the
  driver, add the version to `tested`.
- **More of what the device reports or can do:** for a declared capability, declare it (a new
  value, setting or action) and implement it in the driver. For `source`, check the fixed names
  first (`desk schema --json`, `capabilities.voltage_source`); extra declarations on `source` aren't
  supported yet: a gap in desk (see *When desk itself is missing something*). Probe which fields the device really has. Re-check old
  findings while you're at it: research on the IPS3608 showed that a field once read as "rail
  voltage" is the temperature, and "maximums" are the user's protection thresholds.
- **Another model of a known family:** a new model with only data (`params`).
- **Never squeeze a feature into `params`** or a driver-only side channel: declare it.

## Declaring what it can do

For a declared capability (any name but `power`/`source`, e.g. `meter`, `heater`, `load`):

```toml
[heater]
script = "driver.lua"
port   = { usb_vid = 0x1234, usb_pid = 0x5678 }

[heater.values]                    # what it reports
temperature = { type = "number", unit = "°C", doc = "tip temperature" }

[heater.settings]                  # what it reports and you can change (switches = true by default)
setpoint = { type = "number", unit = "°C", min = 50, max = 450, doc = "target" }
mode     = { type = "choice", choices = ["normal", "sleep"] }

[heater.actions]                   # what it does on request; `switches` is required
standby  = { switches = true, doc = "heater to standby temperature" }
```

- Types: `number` (with `unit`), `boolean`, `choice` (`choices`), `string`. Names: lowercase, `_`.
- **Units from desk's unit table** (`V`, `A`, `W`, `Ω`, `Hz`, `F`, `H`, `s`, `°C`, `K`, `%`, `g`,
  `Pa`, `Ah`, `Wh`); others (`ppm`, `%RH`) are allowed but typed exactly. A value whose unit
  changes (a multimeter's reading) declares `units = ["V", "A", "Ω"]`; the driver returns
  `{ value = 4.98, unit = "V" }`.
- **`min`/`max` only from sources** (the device's range): desk refuses anything outside before it
  is sent. `tolerance`: how far a read-back may differ (default 0.5 %).
- **`switches`:** true for anything with a physical effect (heats, outputs, moves, energizes). Agents
  ask the user before those. Be honest: when in doubt, true.
- Settings apply in declaration order: declare the safe order for switching on (e.g. a setpoint
  before `enabled`). desk sends a switch-off (a boolean `switches` setting set to false) first by
  itself.
- Something only the rotary knob changes is a **value**, not a setting.
- `channels = 2` (or a list of names) for multi-channel devices: the driver gets `ctx.channel`.

## Driver essentials

A driver is Lua 5.4 in a sandbox (no `io`, `os`, `require`, `debug`): it reaches the device only
through `ctx`. Full reference: `docs/scripting.md`; format: `docs/model-format.md`.

```lua
PARAMS = {   -- everything the model's params may contain; desk validates and fills defaults
  name      = { type = "string", doc = "expected model name; refuse other devices" },
  settle_ms = { type = "number", default = 300, doc = "wait after a command before measuring" },
}

function open(ctx)   -- optional: once per command (connect, identify the model)
  port = ctx.serial.open{ baud = 115200, wait = "2s" }   -- only the binding's port; exclusive
end
function close(ctx) ... end                   -- optional: runs even after errors (unlock the panel)

-- declared capability: read every declared value and setting at once, in the declared unit
function read(ctx) return { temperature = 312.5, setpoint = 330, mode = "normal" } end
function set(ctx, name, value) ... end        -- if there are settings (value already checked)
function action(ctx, name, args) ... end      -- if there are actions (args already checked)

-- voltage_source: set_voltage(ctx, v), set_current(ctx, a), set_output(ctx, on), and
--   read(ctx) -> { output, voltage_set, current_set, voltage, current, power, mode, … }
-- power_switch: on(ctx), off(ctx), optional state(ctx)
```

- `port:write(bytes)`, `port:read(max, timeout_ms)` (may return fewer bytes or `""`),
  `port:clear()`; build frames with `string.pack`/`string.unpack` (`"<f"` float32 LE, `"B"` byte).
- `ctx.sleep(ms)`, `ctx.clock()` (monotonic ms), `ctx.log(...)`, `ctx.params`.
- Fail with `error("what went wrong")`; it becomes `script_failed` with the message.
- Parse robustly: resynchronize after garbage, check checksums, time out every wait.
- Some devices drop a write that's followed too quickly by the next frame: keep a quiet gap
  after writes (the FNIRSI driver's `gap_ms`).
- **Prefer data over code:** if models differ only in numbers (offsets, timings, names), share one
  driver in `<vendor>/shared/` and put the numbers in each model's `params`.
- desk does the rest for every driver: one session per command, units, ranges and limits before
  anything is sent, the order of settings, read-back verification, timeouts, `watch`.

## When something fails

| `error.code` | What to do |
|---|---|
| `config_invalid` | `desk model check` names the problem (unknown param, missing rating, bad port) |
| `endpoint_unavailable` | Port not found or busy: `desk port list`; is it powered, is another program using it? |
| `script_failed` | The driver raised an error: read the message and `log` lines, fix the driver |
| `timeout` | The device didn't answer in time: wrong baud or frame, still booting, or not powered |
| `verify_failed` | Sent but not confirmed: check the status layout or echo handling with `desk port probe` |
| `limit_exceeded` | Working as intended. Use lower values; never raise `limits` or `ratings` to get around it |
| `usage` | A malformed argument (e.g. hex for `--send`); the message says which |
