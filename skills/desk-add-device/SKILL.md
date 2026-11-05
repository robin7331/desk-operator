---
name: desk-add-device
description: Add a bench device that the `desk` CLI doesn't support yet: research its protocol, write and test a device definition, smoke-test it with the user, configure it, optionally publish it. Use when the user wants desk to control a new instrument.
---

# Adding a device to `desk`

Two different things come out of this, and they never mix:

- A **device definition** (`devices/<vendor>/<model>/device.toml` + a Lua driver) holds what's true
  for **every unit** of the model: driver, USB ids, ratings, sources. It may be published.
- The **device** in the user's `desk.toml` holds **their bench**: name, aliases, how it gets mains
  power, which unit it is (serial number), and their `limits`. It is never published.

For operating the bench (`desk power`, `desk source`), follow the `desk-operator` skill
(`desk skill desk-operator`). Pass `--json` to every `desk` command.

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
5. **Safety stays in desk:** the definition's `ratings` are the model's maximums, the user's
   `limits` are their bench limits. Drivers never enforce or work around either. Never raise a
   user's `limits` unless they ask.
6. **Honest status:** `status = "community"` until the smoke test passed on real hardware. Drivers
   return only what the device really reports (`read()` fields it can't read stay `nil`, so desk
   says "not confirmed").
7. **Never copy bench things into a definition** (entity ids, serial numbers, limits, paths).

## Where to work

A **checkout of the desk-operator repository** is strongly recommended: it has the format spec
(`docs/definition-format.md`), the host API (`docs/scripting.md`), reference drivers
(`devices/*/shared/*.lua`), the fake-device test harness (`tests/device_*.rs`) and is needed for
publishing. Point desk at it with `DESK_DEVICES=<checkout>/devices` so `desk library` and the
user's config find the new definition.

Without a checkout (only an installed `desk`), a definition can still live in `devices/` next to
the user's `desk.toml` (the default of `desk library new`). Then there's no fake-device test: say
so, and rely on the read-only probing and the smoke test.

## Workflow

### 1. Ask

- The **exact model** (and hardware/firmware version if the user knows it).
- **What it is:** desk has two capabilities, `voltage_source` (bench power supplies) and
  `power_switch` (mains power). A load, meter, iron or scope needs a new capability in desk's core
  first; tell the user that's not supported yet, and offer the next point meanwhile.
- **Only switched on/off?** If the user just wants mains on/off (e.g. via a Home Assistant outlet),
  no definition is needed: add a `power` binding to their device and you're done
  (`desk provider endpoints <id> --search …` finds the entity).
- **How it connects:** USB (CDC-ACM serial, USB-serial adapter, USBTMC), RS-232, LAN. desk drivers
  talk serial today (`ctx.serial`). Other transports are a gap in the host API, a core change.

### 2. Check the library first

```sh
desk library list --json
desk library show <vendor/model> --json   # bindings, driver params, status, sources
```

A family driver may already fit: the IPS3608 needed only data (`params`) on top of the DPS-150
driver. Vendors often ship one PC application for several models; that's a strong hint they share
a protocol.

### 3. Research

Look for, in this order of trust: the vendor's protocol documentation or programming manual;
**source code of a working tool** for this model; community reverse engineering with captures; forum
posts. A tool someone runs against the device beats a write-up, and some "protocol" blog pages are
generated filler: cross-check every frame and rating against a second source. Record the sources
you used (URLs) for the definition's `sources` and the vendor README. Note what's unknown.

### 4. Scaffold

```sh
desk library new <vendor>/<model> --capability voltage_source --name "<Vendor Model>" \
    --max-voltage <V> --max-current <A> [--driver ../shared/<family>.lua] [--dir <checkout>/devices]
desk library check <vendor>/<model> --json
```

`new` writes `device.toml`, a driver skeleton with every required function (unless `--driver`) and
a vendor `README.md`. Fill in `sources`, `port = { usb_vid, usb_pid }` (when known),
`params`, and implement the driver (see *Driver essentials*). Rerun `check` until it passes.

### 5. Fake-device test (in a checkout)

Before touching hardware, run the driver against a fake device over a pseudo-terminal, like
`tests/device_fnirsi_dps150.rs`: a thread plays the device on the pty master side, `desk` opens the
slave as its port. Cover what the sources say: replies, missing echoes, pushed telemetry, garbage
before a frame, a silent device, a wrong model. Then:

```sh
cargo test --test device_<vendor>_<family>
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

Add the device to the user's `desk.toml` (path: `data.config.path` of `desk schema --json`); with a
checkout, the user's shell needs `DESK_DEVICES=<checkout>/devices` until the definition is bundled.
Ask the user for the id, name, power path, and **conservative test limits**:

```toml
[devices.psu2]
model  = "<vendor>/<model>"
power  = { provider = "ha", entity = "switch.desk_outlet_3" }   # from `desk provider endpoints`
source = { port = { serial_number = "…" } }                     # from `desk port list`
limits = { source = { voltage = 5, current = 0.5 } }            # test limits, agreed with the user
```

Check with `desk device show <id> --json`.

### 9. Smoke test with the user

Values come from the user: propose low ones (e.g. 3.3 V, 100 mA) and use them only after they
agree. One step at a time, each confirmed:

1. `desk source read <device> --json`: read-only. Do output state and setpoints match the front panel?
2. `desk source set <device> 3.3v 100ma --json` with the output off: `data.verified` true? The display?
3. `desk source output on <device> --json`, then `read`: measured voltage ≈ setpoint (no load)?
4. `desk source output off <device> --json`.
5. Mains off, then on (`desk power off|on <device>`): output state after power-up? Setpoints kept?
   How long until the port answers?

Fix the driver (and its fake-device test) for anything that differs, then repeat the affected step.

### 10. Finish

- In the definition: `status = "tested"`, `tested = ["firmware … / hardware … (Mon YYYY)"]` (read
  the versions from the device if it reports them), `sources`.
- In the vendor `README.md`: protocol notes, sources, known gaps, **observed behavior** (boot,
  power-up state, ramps, echoes).
- `desk library check --json` passes.
- Ask the user for their real bench `limits` and replace the test limits.
- In a checkout: `cargo test` and clippy pass.

### 11. Publish (optional, only with consent)

Ask whether the user wants to contribute the definition. Show exactly what would be published:
`devices/<vendor>/…` (definition, driver, README) and its `tests/device_*.rs`, **never** their
`desk.toml` or anything from it. Check the files for their serial number, entity ids and paths
before committing. Nothing leaves the machine before the user agreed to this file list.

The upstream repository is the one the checkout came from (`git remote -v`); if that's unclear,
ask the user. Then, depending on what's available:

- **`gh` installed and logged in** (`gh auth status`): branch (`device/<vendor>-<model>`), commit,
  `gh repo fork --remote`, push, `gh pr create` with what the definition supports, the tested
  versions, what the smoke test verified, the sources, and the known gaps.
- **git but no `gh`:** commit on a branch, then give the user the steps: fork on the web,
  `git remote add fork <their fork URL>`, `git push -u fork <branch>`, and open the compare page
  the push prints. Or create a patch with `git format-patch -1 -o <dir>` for them to send.
- **No checkout:** pack the vendor folder
  (`tar czf <vendor>-<model>.tar.gz -C <devices dir> <vendor>`) and tell the user to attach it to
  an issue or upload it in the web UI.

## Driver essentials

A driver is Lua 5.4 in a sandbox (no `io`, `os`, `require`, `debug`): it reaches the device only
through `ctx`. Full reference: `docs/scripting.md`; format: `docs/definition-format.md`.

```lua
PARAMS = {   -- everything the definition's params may contain; desk validates and fills defaults
  name      = { type = "string", doc = "expected model name; refuse other devices" },
  settle_ms = { type = "number", default = 300, doc = "wait after a command before measuring" },
}

function open(ctx)   -- optional: once per command (connect, identify the model)
  port = ctx.serial.open{ baud = 115200, wait = "2s" }   -- only the binding's port; exclusive
end
function set_voltage(ctx, volts) ... end      -- voltage_source: required
function set_current(ctx, amps) ... end
function set_output(ctx, on) ... end
function read(ctx)   -- return only fields the device reported
  return { output = true, voltage_set = 5, current_set = 1, voltage = 5.01, current = 0.2 }
end
function close(ctx) ... end                   -- optional: runs even after errors (unlock the panel)
-- power_switch drivers: on(ctx), off(ctx), optional state(ctx)
```

- `port:write(bytes)`, `port:read(max, timeout_ms)` (may return fewer bytes or `""`),
  `port:clear()`; build frames with `string.pack`/`string.unpack` (`"<f"` float32 LE, `"B"` byte).
- `ctx.sleep(ms)`, `ctx.clock()` (monotonic ms), `ctx.log(...)`, `ctx.params`.
- Fail with `error("what went wrong")`; it becomes `script_failed` with the message.
- Parse robustly: resynchronize after garbage, check checksums, time out every wait.
- **Prefer data over code:** if models differ only in numbers (offsets, timings, names), share one
  driver in `devices/<vendor>/shared/` and put the numbers in each definition's `params`.
- desk does the rest for every driver: one session per command, limits before anything is sent,
  voltage → current → output order, read-back verification, timeouts.

## When something fails

| `error.code` | What to do |
|---|---|
| `config_invalid` | `desk library check` names the problem (unknown param, missing rating, bad port) |
| `endpoint_unavailable` | Port not found or busy: `desk port list`; is it powered, is another program using it? |
| `script_failed` | The driver raised an error: read the message and `log` lines, fix the driver |
| `timeout` | The device didn't answer in time: wrong baud or frame, still booting, or not powered |
| `verify_failed` | Sent but not confirmed: check the status layout or echo handling with `desk port probe` |
| `limit_exceeded` | Working as intended. Use lower values; never raise `limits` or `ratings` to get around it |
| `usage` | A malformed argument (e.g. hex for `--send`); the message says which |
