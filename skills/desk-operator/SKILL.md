---
name: desk-operator
description: Operate electronics bench equipment (power supplies, soldering irons, mains outlets, ...) with the `desk` CLI. Use to switch devices on or off, set or read a power supply, check what's on, or configure desk devices.
---

# Operating the bench with `desk`

`desk` controls bench devices through capabilities: `power_switch` (a device's mains power, via
e.g. Home Assistant) and `voltage_source` (a bench power supply's setpoints, output and
measurements). Which device has which capability is in the schema. It describes itself, so learn
the bench from it instead of guessing.

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
5. **`desk power` is mains power to a device; `desk source output` is a PSU's output terminals.**
   Never mix them up. "Turn on the power supply" means mains (`desk power on psu`); "turn on the
   output" or "give me 5 V" means `desk source`.
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
desk library list --json                            # device definitions (models desk knows)
desk library show <vendor/model> --json

desk source read <device> --json                    # output state, setpoints, measured V/A/W
desk source set <device> 5v 1a [--on] --json        # units required: v, mv, a, ma
desk source output on|off <device> --json
desk source set <device> 5v --on --dry-run --json   # limits checked, nothing sent
desk source watch <device> --count 10 --json        # one JSON line per reading (see below)
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
| `capability_missing` | That device can't do this (e.g. no `source`); say so |
| `device_off` | Its mains power is off. Ask before switching it on with `desk power on` |
| `limit_exceeded` | Refused, nothing was sent. Tell the user the limit; never work around it or edit `limits` unasked |
| `protection_tripped` | A protection (OVP, OCP, …) switched the output off; nothing was sent. Tell the user which one. Use `--clear-protection` only after they found the cause and asked you to switch on again |
| `daemon_unavailable` | The background daemon behind `watch` failed or stopped. Show the message (it names the daemon's log); don't start `desk serve` yourself unless asked |

## Power supplies (`desk source`)

- **Only use voltages and currents the user stated.** Never pick or "try" values yourself. If the
  request is vague ("power my board"), ask for voltage and current limit.
- Set the current limit together with the voltage when the user gave one (`5v 500ma`).
- `--on` switches the output on *after* the setpoints. Use it only when the user wants the output on.
- Read `data.verified`: `true` means the value was read back and confirmed. A missing entry means
  the device can't report it, so say "set, but not confirmed by the device".
- Also report `data.warnings` and the measured values (`data.reading`). Depending on the device,
  `reading` also has `mode` (`cv`, or `cc` = the current limit is reached, e.g. a short or a
  charging battery), `protection` (`none` or what tripped), `temperature`, `input_voltage`, and
  `charge`/`energy` (Ah/Wh counters). `null` means the device doesn't report it.
- A tripped protection blocks `output on` (`protection_tripped`). `--clear-protection` overrides
  that and switches the output back on, so it's the user's call, never yours.
- When in doubt, `--dry-run` first: it checks the limits and shows what would be sent.
- **Watching:** `desk source watch <device> --json` prints one `{ ok, data }` line per reading
  (`data.reading`, `data.time`, `data.elapsed` in s, `data.dropped`, `data.failed`) until stopped; always pass
  `--count N` (or a timeout) so it ends. The last line is `ok: false` if the watch ended with an
  error, e.g. `device_off` after a mains cut. A background daemon holds the device meanwhile, so
  other `desk` commands keep working. Watching only reads; it never switches anything.

## Safety

Mains power has physical effects. A soldering iron starts heating when it's powered. A PSU or
DC load may come up with its previous output setting. When switching on, mention this if the
user's context suggests something is connected. Never switch devices on or off "to test" without
asking.

## Configuration (only when asked)

The config path is `data.config.path` in `desk schema --json`. If `desk library list` has a
definition for the model, use it instead of writing driver bindings by hand:

```toml
[devices.psu]
model   = "vendor/model"                                        # from `desk library list`
aliases = ["supply"]
power   = { provider = "ha", entity = "switch.desk_outlet_1" }  # from `desk provider endpoints`
source  = { port = { serial_number = "…" } }                    # which unit, from `desk port list`
limits  = { source = { voltage = 12, current = 2 } }                         # ask the user for bench limits
```

Values in the device merge into the definition's bindings. Never copy bench-specific things
(entities, limits, serial numbers) into a definition.

After editing it, check with `desk device list --json`. Lua power scripts are documented in the
project's `docs/scripting.md`.

**A model `desk library list` doesn't know** needs a new definition: follow the `desk-add-device`
skill (`desk skill desk-add-device`). Its `desk port probe` sends raw bytes without any limit
checks; never use it to operate devices.
