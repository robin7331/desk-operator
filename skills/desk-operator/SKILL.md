---
name: desk-operator
description: Operate electronics bench equipment (power supplies, DC loads, multimeters, soldering irons, switched mains outlets) with the `desk` CLI. Use when the user asks to switch bench devices on or off, asks what is on, or wants to inspect or configure desk-operator devices and providers.
---

# Operating the bench with `desk`

`desk` controls the mains power of bench devices through providers (e.g. Home Assistant) or Lua
scripts. It describes itself, so learn the bench from it instead of guessing.

## Always

1. **Start with `desk schema --json`.** It lists the devices (ids, names, aliases, capabilities,
   power paths), every command with its arguments, and every error code. Use only device ids or
   aliases from it.
2. **Pass `--json` to every command** and read stdout: `{ "ok": bool, "data": …, "error"?: { code, message, hint? } }`.
   stderr only carries logs and human messages.
3. **Change power only when the user asked for that change.** "What's on?" is read-only: use
   `desk status --json`, which lists every device, including devices without a power path
   (`power: null`; mention those as "not controllable"). Use `--all` only when the user said
   "everything" or "all".
4. **Preview with `--dry-run`** when a request covers several devices, or when you had to pick
   between several plausible devices (for example two devices whose names both fit). Show the plan
   and confirm before switching. A name or alias that `desk` resolves to exactly one device is not a
   guess: an explicit request for one device can run directly.
5. **"power" is mains power to the device**, not a PSU's output terminals. Output, voltage and
   current control don't exist yet: say so and don't improvise.
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
desk power on <device>... --dry-run --json          # what would be sent; nothing is switched
desk device list|show <device> --json
desk provider list|test <id> --json
desk provider endpoints <id> --search <text> --json # e.g. Home Assistant entities
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

## Safety

Mains power has physical effects. A soldering iron starts heating when it's powered. A PSU or
DC load may come up with its previous output setting. When switching on, mention this if the
user's context suggests something is connected. Never switch devices on or off "to test" without
asking.

## Configuration (only when asked)

The config path is `data.config.path` in `desk schema --json`. Devices look like:

```toml
[devices.psu]
name    = "Bench power supply"
aliases = ["supply"]
power   = { provider = "ha", entity = "switch.desk_outlet_1" }   # from `desk provider endpoints`
```

After editing it, check with `desk device list --json`. Lua power scripts are documented in the
project's `docs/scripting.md`.
