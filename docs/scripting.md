# Scripted power paths (Lua)

Most devices need nothing more than a native binding:

```toml
power = { provider = "ha", entity = "switch.desk_outlet_1" }
```

When switching a device takes more than one call (press twice, power a fan first, wait for
something), its power path can be a Lua 5.4 script instead:

```toml
[devices.station]
name  = "Iron + fume extractor"
power = { script = "scripts/iron_with_extractor.lua", timeout = "15s", params = {
  iron      = { provider = "ha", entity = "switch.desk_outlet_4" },
  extractor = { provider = "ha", entity = "switch.desk_fan" },
} }
```

| Key | Meaning |
|---|---|
| `script` | Path to the `.lua` file, relative to the config file (or absolute) |
| `params` | Any table; passed to the script as `ctx.params`. Use it to keep scripts reusable |
| `timeout` | Limit for each call (`on`, `off`, `state`), default `30s` |

## The script

```lua
function on(ctx)  ... end   -- required
function off(ctx) ... end   -- required
function state(ctx) ... end -- optional
```

- A call **succeeds when the function returns** and **fails when it raises** (`error("...")`).
  The message, with file and line, ends up in the `script_failed` error.
- `state(ctx)` returns `"on"`, `"off"`, `"unknown"`, `true`, `false` or `nil`.
  - **With `state`**, desk treats the path like any other with read-back: `on` isn't called when
    the device is already on, the result is checked afterwards, and `toggle` works.
  - **Without `state`**, results are reported as `assumed` and `toggle` is refused.
- Globals persist between the calls of one command (e.g. `state` → `on` → `state`), but not
  between commands.

## Host API: `ctx`

| | |
|---|---|
| `ctx.device` | `{ id = "station", name = "Iron + fume extractor" }` |
| `ctx.params` | The binding's `params` table |
| `ctx.power(binding)` | A handle to any power path, written exactly like a native binding: `ctx.power{ provider = "ha", entity = "switch.x" }` or `ctx.power(ctx.params.iron)` |
| `ctx.sleep(ms)` / `ctx.sleep("1.5s")` | Wait without blocking other devices |
| `ctx.log(...)` | Log a line (arguments are joined with spaces). Shown on stderr as `device: line` and returned in `--json` output as `log` |

A power handle `p` (both `p:on()` and `p.on()` work):

| | |
|---|---|
| `p:on()`, `p:off()`, `p:toggle()` | Same semantics as `desk power …`: idempotent, verified. Returns the new state |
| `p:state()` | `"on"`, `"off"` or `"unknown"` |
| `p:set(true\|false)` | Raw send: no idempotence check, no verification (for quirks such as "press twice") |

Errors raised by the host (an unreachable provider, an unavailable entity, a failed verification)
keep their own error code (`provider_unreachable`, `endpoint_unavailable`, `verify_failed`, …) unless
the script catches them with `pcall`.

## Sandbox

Only the `string`, `table`, `math`, `utf8` and `coroutine` libraries are available. There is no
`io`, `os`, `require`/`package`, `debug`, `dofile` or `loadfile`: a script can only reach the outside
world through `ctx`. `print` writes to the log, so `--json` output stays parseable. The timeout also
interrupts busy loops.

## Examples

- [`examples/scripts/iron_with_extractor.lua`](../examples/scripts/iron_with_extractor.lua): two
  outlets in order, with delays; state comes from the iron outlet
- [`examples/scripts/double_press.lua`](../examples/scripts/double_press.lua): a send-only device
  that needs every command twice, 50 ms apart

Try them without hardware: `desk --config examples/desk.toml power on station`.
