---
name: desk-operator
description: Operate electronics bench equipment (power supplies, meters, loads, soldering irons, mains outlets, ...) with the `desk` CLI. Use to switch devices on or off, read or change a device's values and settings, check what's on, or configure desk devices.
---

# Operating the bench with `desk`

`desk` controls bench devices. Every device can have mains power (`desk power`, e.g. via Home
Assistant) and any number of capabilities, each with **settings** (you can change them),
**values** (it reports them) and **actions** (it does them). Power supplies have fixed names
(`voltage`, `current`, `output`, `measured_voltage`, …); other devices declare their own. The schema
lists every device's names with types, units, ranges and limits, so learn the bench from it
instead of guessing.

## Always

1. **Start with `desk schema --json`.** It lists the devices (ids, names, aliases, capabilities,
   power paths), every command with its arguments, and every error code. Use only device ids or
   aliases from it.
2. **Pass `--json` to every command** and read stdout: `{ "ok": bool, "data": …, "error"?: { code, message, hint? } }`.
   stderr only carries logs and human messages.
3. **Change power only when the user asked for that change.** "What's on?" is read-only: use
   `desk status --json`, which lists every device, including devices without a power path
   (`power: null`; mention those as "not controllable"). Switch every device only when the user said
   "everything" or "all". Careful: **`desk power off` without device names (or with `all`) switches
   every device off.** Always name the devices unless everything is meant.
4. **Preview with `--dry-run`** when a request covers several devices, or when you had to pick
   between several plausible devices (for example two devices whose names both fit). Show the plan
   and confirm before switching. A name or alias that `desk` resolves to exactly one device is not a
   guess: an explicit request for one device can run directly.
5. **`desk power` is mains power to a device; `desk set psu output on` is a PSU's output
   terminals.** Never mix them up. "Turn on the power supply" means mains (`desk power on psu`);
   "turn on the output" or "give me 5 V" means `desk set`.
6. **Know what a state means:** `confidence: "real"` was read back. `"assumed"` means `desk` sent
   the command but can't read the state back, so report it as unverified. `state: "unknown"` (with
   `confidence: "none"`) means nothing is known, or a combined device is only partly on.
   `on` and `off` are never toggles. On send-only paths they're simply sent again every time.
7. **Never read, print or copy access tokens** (e.g. `token_file`, `HA_TOKEN`).

## Commands

```sh
desk schema --json                                  # the bench, commands, error codes
desk status --json                                  # every device's power + provider health
desk power status [<device>...] --json
desk power on|off|toggle <device>... --json         # several devices run concurrently
desk power off [all] --json                         # every device off (switching on needs --all)
desk power on <device>... --dry-run --json          # what would be sent; nothing is switched
desk device list|show <device> --json
desk provider list|test <id> --json
desk provider endpoints <id> --search <text> --json # e.g. Home Assistant entities
desk port list --json                               # USB serial ports (for a binding's port = {…})
desk model list --json                              # the kinds of device desk knows
desk model show <vendor/model> --json

desk get <device> [<name>...] --json                # values and settings (all, or named)
desk set <device> <setting> <value> [...] --json    # e.g. voltage 5v current 1a output on
desk set <device> <setting> <value> --dry-run --json  # ranges and limits checked, nothing sent
desk do <device> <action> [<arg>=<value>...] --json  # e.g. desk do iron standby
desk watch <device> [<name>...] --count 10 --json   # one JSON line per reading (see below)
desk get <device>:<channel> --json                  # one channel of a multi-channel device
desk set <device> [<setting>] --json                # what it takes (see below); changes nothing
desk do <device> [<action>] --json                  # the same for actions and their arguments
```

There's one entry per device: `data.results[]` for `power on|off|toggle`, `data.devices[]` for
`power status` and `status`. Report each device's outcome. When `ok` is false, `error` describes
the first failure only.

## When something fails

| `error.code` | What to do |
|---|---|
| `device_not_found`, `device_ambiguous` | Check `desk schema`; ask the user which device they meant |
| `no_power_path` | The device has no power binding; tell the user |
| `provider_unreachable` | Report it (Home Assistant offline, bad token). Retry at most once |
| `endpoint_unavailable` | That outlet or entity is offline or unknown; report it |
| `state_unknown` | Toggle isn't possible here; ask whether they want `on` or `off` |
| `verify_failed` | The command was sent but not confirmed. Report it; don't repeat it blindly |
| `timeout` | Report it with the message; the device may be in an intermediate state, so check `power status` |
| `script_failed` | Show the message and any `log` lines from the result |
| `config_invalid` | Show the message. Don't edit the config unless the user asks |
| `capability_missing` | That device can't do this (e.g. no `source`, nothing to watch); say so |
| `device_off` | Its mains power is off. Ask before switching it on with `desk power on` |
| `limit_exceeded` | Refused, nothing was sent. Tell the user the limit; never work around it or edit `limits` unasked |
| `protection_tripped` | A protection (OVP, OCP, …) switched the output off; nothing was sent. Tell the user which one. Use `--clear-protection` only after they found the cause and asked you to switch on again |
| `daemon_unavailable` | The background daemon behind `watch` failed or stopped. Show the message (it names the daemon's log); don't start `desk serve` yourself unless asked |

## Values, settings and actions (any device)

- **Only use values the user stated.** Never pick or "try" values yourself. If the request is
  vague ("power my board", "heat it up"), ask for the values.
- **Units are required** for numbers that have one (`5v`, `250ma`, `330c`, `1.5kohm`, `2min`).
  Prefixes are case-sensitive: `m` is milli, `M` mega. Numbers in results are in the declared unit
  without prefixes (`0.25`, not 250 mA); a value with changing units comes as `{ value, unit }`.
- **`switches: true`** in the schema means a setting or action has a physical effect (heats,
  outputs, energizes): only on the user's request, and mention it when something may be connected.
- Read `data.verified`: `true` means the setting was read back and confirmed. A missing entry
  means the device can't report it, so say "set, but not confirmed by the device".
- Report `data.values` (and `data.channels` for multi-channel devices) and `data.warnings`. A name
  that's absent isn't reported by the device.
- When in doubt, `--dry-run` first: it checks ranges and limits and shows what would be sent.
- To find out what a setting or action takes, leave the value or action out: `desk set <device>
  <setting> --json` gives its choices or unit, range and `limit` in `data`, and its current `value`
  if it could be read (else `value_error`). It changes nothing and still answers `ok: false` with
  `error.code` `usage`, because nothing was set. Read `data`; don't report that as a failure.

## Power supplies

- Settings `voltage`, `current` (the limit) and `output`; values `measured_voltage`,
  `measured_current`, `measured_power`, and if reported `mode` (`cv`, or `cc` = the current limit
  is reached, e.g. a short or a charging battery), `protection` (`none` or what tripped),
  `temperature`, `input_voltage`, `charge`/`energy` (Ah/Wh counters).
- Set the current limit together with the voltage when the user gave one
  (`desk set psu voltage 5v current 500ma`). desk applies setpoints before the output, whatever
  order you write them in. Add `output on` only when the user wants the output on.
- A tripped protection blocks `output on` (`protection_tripped`). `--clear-protection` overrides
  that and switches the output back on, so it's the user's call, never yours.

## Watching

`desk watch <device> [<name>...] --json` prints one `{ ok, data }` line per reading
(`data.values`, `data.time`, `data.elapsed` in s, `data.dropped`, `data.failed`) until stopped;
always pass `--count N` (or a timeout) so it ends. `--changes` prints only when a value changed. The
last line is `ok: false` if the watch ended with an error, e.g. `device_off` after a mains cut. A
background daemon holds the device meanwhile, so other `desk` commands keep working. Watching only
reads; it never switches anything.

## Safety

Mains power has physical effects. A soldering iron starts heating when it's powered. A PSU or
DC load may come up with its previous output setting. When switching on, mention this if the
user's context suggests something is connected. Never switch devices on or off "to test" without
asking.

## Configuration (only when asked)

The config path is `data.config.path` in `desk schema --json`. If `desk model list` has a
model for the model, use it instead of writing driver bindings by hand:

```toml
[devices.psu]
model   = "vendor/model"                                        # from `desk model list`
aliases = ["supply"]
power   = { provider = "ha", entity = "switch.desk_outlet_1" }  # from `desk provider endpoints`
source  = { port = { serial_number = "…" } }                    # which unit, from `desk port list`
limits  = { source = { voltage = 12, current = 2 } }            # ask the user for bench limits
# other capabilities the same way: heater = { … }, limits = { heater = { setpoint = 380 } }
```

Values in the device merge into the model's bindings. Never copy bench-specific things
(entities, limits, serial numbers) into a model.

After editing it, check with `desk device list --json`. Lua power scripts are documented in the
project's `docs/scripting.md`.

**A model `desk model list` doesn't know** needs a new model: follow the `desk-add-device`
skill (`desk skill desk-add-device`). Its `desk port probe` sends raw bytes without any limit
checks; never use it to operate devices.
