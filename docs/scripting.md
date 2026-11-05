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

## Instrument drivers (`source = { script = … }`)

A script can also be the driver of a capability. For a bench power supply (`voltage_source`):

```toml
[devices.psu]
source = { script = "drivers/my_psu.lua", timeout = "10s", params = { model = "x" } }
limits = { voltage = 12, current = 2 }   # bench safety, enforced by desk, not by the driver
```

```lua
function set_voltage(ctx, volts) ... end   -- required
function set_current(ctx, amps) ... end    -- required
function set_output(ctx, on) ... end       -- required
function read(ctx)                         -- required: report what the device can tell
  return { output = true, voltage_set = 5, current_set = 1, voltage = 5.01, current = 0.2, power = 1.0 }
end
function info(ctx) return { max_voltage = 30, max_current = 5 } end   -- optional, enables max checks
function open(ctx) ... end                 -- optional: start of a session (connect, handshake)
function close(ctx) ... end                -- optional: end of a session
```

Drivers only implement primitives. Everything users rely on is done by desk itself, the same for
every driver:

- **Sessions:** one `open` … `close` per command around all steps. `close` is called even after
  errors and timeouts (devices that lock their front panel while connected get released).
- **Limits:** values above `info()`'s maximum or the device's bench `limits` are refused before
  the driver is called. Switching the output on is also refused when the device *already* holds a
  setpoint above the bench limits (as far as `read()` reports it).
- **Order:** voltage, then current, then output.
- **Verification:** after the steps, `read()` is compared with the request. Fields `read()` doesn't
  return are reported as *not confirmed*, never as confirmed. Return only what the device really
  reports.
- **`timeout`** (default `10s`) covers the whole session of one command.

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
