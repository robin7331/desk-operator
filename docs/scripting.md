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
| `port` | The serial port the script may open (see below) |

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
| `ctx.clock()` | Monotonic milliseconds (float), for deadlines and "measured after my command?" checks |
| `ctx.log(...)` | Log a line (arguments are joined with spaces). Shown on stderr as `device: line` and returned in `--json` output as `log` |
| `ctx.channel` | The channel of this call (`"1"`, `"ch2"`, …) when the capability declares `channels`; `nil` otherwise |
| `ctx.output` | During an action that produces a file or a `stream()` call: the handle to fill (see *Actions that produce a file*, *Streams*); `nil` otherwise |
| `ctx.append{ into = ctx.output, data = s }` | Append bytes to that file or stream |
| `ctx.describe{ into = ctx.output, … }` | Say what a stream's bytes are, once before the first byte (see *Streams*) |
| `ctx.serial` | The binding's serial port (see *Serial ports*) |
| `ctx.sigrok` | The binding's sigrok device (see *sigrok devices*) |

A power handle `p` (both `p:on()` and `p.on()` work):

| | |
|---|---|
| `p:on()`, `p:off()`, `p:toggle()` | Same semantics as `desk power …`: idempotent, verified. Returns the new state |
| `p:state()` | `"on"`, `"off"` or `"unknown"` |
| `p:set(true\|false)` | Raw send: no idempotence check, no verification (for quirks such as "press twice") |

Errors raised by the host (an unreachable provider, an unavailable entity, a failed verification)
keep their own error code (`provider_unreachable`, `endpoint_unavailable`, `verify_failed`, …) unless
the script catches them with `pcall`.

## Serial ports (`ctx.serial`)

A script binding can name **one** serial port. The script can open that port and no other:

```toml
source = { script = "drivers/my_psu.lua", port = { usb_vid = 0x1a86, usb_pid = 0x7523 } }
# or: port = { path = "/dev/serial/by-id/usb-…" }, optionally with serial_number = "…"
```

Find the values with `desk port list` (`--all` also shows non-USB ports). Every given field must
match; if several ports match, add `serial_number` or `path`.

```lua
local port = ctx.serial.open{ baud = 115200 }   -- opens the binding's port
port:write(string.pack("<BBf", 0xF1, 0xC1, 5.0)) -- bytes are Lua strings
local reply = port:read(17, 500)                 -- 17 bytes, or what came within 500 ms (maybe "")
port:clear()                                     -- drop input that arrived but wasn't read
port:close()                                     -- also happens automatically after the command
```

`port:read(max, ms)` waits the whole timeout unless `max` bytes arrive, so for line-based text
protocols read one byte at a time until the line ending. The timeout defaults to 1 s. `port.path`
is the path of the opened port.

| `open{…}` option | Default | |
|---|---|---|
| `baud` | `9600` | |
| `data_bits`, `parity`, `stop_bits` | `8`, `"none"`, `1` | `parity`: `"none"`, `"odd"`, `"even"` |
| `flow` | `"none"` | `"software"`, `"hardware"` |
| `dtr` | unchanged | some USB devices only talk with DTR set |
| `wait` | `0` | keep looking for the port, e.g. `"3s"` right after switching mains on |

Ports are opened **exclusively**: while one command uses a port, nothing else can, and
`endpoint_unavailable` says "port is busy". Binary protocols are easy with Lua 5.4's
`string.pack`/`string.unpack` (`"<f"` = little-endian float32, `"B"` = byte, `">H"` = big-endian u16).

## sigrok devices (`ctx.sigrok`)

A script binding can name **one** sigrok device instead of (or next to) a port. desk runs
`sigrok-cli` for it; the script never names a program, a path or raw arguments
([`sigrok.md`](sigrok.md) says how desk finds the program):

```toml
capture = { script = "drivers/la.lua", sigrok = { driver = "demo" } }
# per bench, in desk.toml: sigrok = { conn = "1.23", backend = "vendor" }
```

| | |
|---|---|
| `ctx.sigrok.driver` | The binding's `driver` |
| `ctx.sigrok.conn` | The binding's `conn`, or `nil` |
| `ctx.sigrok.acquire{ … }` | Capture into the file of an action that produces one, or into a stream. Returns `{ bytes = n }` |
| `ctx.sigrok.measure{ … }` | Take readings from analog channels (a meter's display, a supply's output). Returns a list of readings |
| `ctx.sigrok.get{ … }` | Read config keys (a supply's setpoint, a meter's mode). Returns `{ key = value, … }` |
| `ctx.sigrok.set{ … }` | Change config keys. Returns nothing |

Without a `sigrok = { … }` binding, each of them fails with `config_invalid`.

```lua
local r = ctx.sigrok.acquire{
  config   = { samplerate = 10000000 },  -- sigrok config keys: the driver's business
  channels = { "D0", "D1" },             -- or "D0,D1"; default: all channels
  time     = 2000,                       -- ms (or "2s"); or samples = n, or frames = n
  triggers = "D0=r",                     -- optional; wait_trigger = true waits for it
  format   = "srzip",                    -- sigrok's output module (-O)
  into     = ctx.output,
  timeout  = "5s",                       -- required: the whole capture, trigger wait included
}
```

| Key | | Becomes |
|---|---|---|
| `config` | Table of sigrok config keys (`[a-z0-9_]+`) to strings, numbers or booleans. Whole numbers are written without a fraction (`10000000`, never `1e7`); booleans as `true`/`false`. Strings can't contain `:`, `=`, whitespace or control characters, or start with `-` | one `--config k=v:k=v` (keys sorted) |
| `channels` | A list of names, or one comma-separated string. Names are letters, digits, `_`, `.`, `-` (a range like `D0-D7` is passed as written) | one `--channels a,b` |
| `time` / `samples` / `frames` | Exactly one. `time` in ms (a number, or a duration string such as `"2s"`); `samples` and `frames` whole numbers above 0 | `--time <ms>`, `--samples <n>`, `--frames <n>` |
| `continuous` | `true`: until the stream is stopped, instead of `time`/`samples`/`frames`. Only into a stream | `--continuous` |
| `triggers` | sigrok's trigger text, e.g. `"D0=r,D1=1"`: like a config value, but `=` and `,` are allowed | `--triggers <t>` |
| `wait_trigger` | `true` to wait for the trigger (needs `triggers`) | `--wait-trigger` |
| `format` | sigrok output module id (`[a-z0-9_]+`), default `"srzip"` (`"binary"` into a stream) | `-O <format>` |
| `into` | Required: `ctx.output` | `-o <desk's temporary file>`; into a stream no `-o`: desk reads sigrok's stdout |
| `timeout` | Required for a file, optional for a stream: ms or a duration string | desk stops sigrok after it |

Each element is one argument, with `-d <driver>` (or `-d <driver>:conn=<conn>`) first and no
shell. Anything else in the table, or a value that breaks a rule above, is `script_failed` before
sigrok runs. While `acquire` runs, the call's own timeout is suspended; sigrok is stopped on its
`timeout` (`timeout`), at the size cap (`limit_exceeded`), on Ctrl-C or when a daemon client goes
away (`cancelled`). Then nothing of the capture is left, including files sigrok wrote next to it.
sigrok's stdin stays open until it exits, and its warnings on stderr never fail a capture: only
its exit code does. Failures: device busy or not found (`endpoint_unavailable`), no backend has
the driver (`config_invalid`), no `sigrok-cli` (`provider_unreachable`), and anything else as
`script_failed` with sigrok's own message.

Into a stream, desk reads sigrok's stdout in large pieces and passes it on; the stream ends when
sigrok does (a bounded acquisition ends it normally) or is stopped, and then sigrok is killed. A
continuous stream from the `demo` driver:

```lua
ctx.sigrok.acquire{ config = { samplerate = 10000000 }, channels = { "D0", "D1" },
                    continuous = true, format = "binary", into = ctx.output }
-- -d demo --config samplerate=10000000 --channels D0,D1 --continuous -O binary
```

sigrok's `binary` output packs the enabled channels into each sample in the order they were
enabled: with only D3 and D7 enabled, D3 is bit 0 and D7 bit 1. Describe `channels` in that order.

### Readings and settings through sigrok

`measure`, `get` and `set` are short calls for the `read()` and `set()` of a declared capability
(or of `source`). Each one runs `sigrok-cli` once. They don't suspend the call's own timeout: the
`timeout` they take is for that one run of sigrok, and the call's timeout still covers the whole
`read()` or `set()`.

```lua
function read(ctx)
  local out = {}
  for _, r in ipairs(ctx.sigrok.measure{ channels = { "P1" } }) do
    if r.value then out.reading = { value = r.value, unit = r.unit } end   -- `units = [...]`
  end
  local v = ctx.sigrok.get{ keys = { "voltage_target", "enabled" }, group = "CH1" }
  out.voltage, out.output = v.voltage_target, v.enabled
  return out
end

function set(ctx, name, value)
  local key = ({ voltage = "voltage_target", output = "enabled" })[name]
  ctx.sigrok.set{ config = { [key] = value }, group = "CH1" }
end
```

`ctx.sigrok.measure{ … }`:

| Key | | Becomes |
|---|---|---|
| `channels` | Required: a list of names, or one comma-separated string (checked like `acquire`'s) | one `--channels a,b` |
| `samples` / `time` | At most one. `samples` a whole number above 0, default `1`; `time` in ms or a duration string | `--samples <n>`, `--time <ms>` |
| `config` | Optional, like `acquire`'s: applied in the same run | one `--config k=v:k=v` (keys sorted) |
| `group` | Optional channel group for `config`: letters, digits, `_`, `.`, `-`, not starting with `-` | `-g <group>` |
| `timeout` | Optional: ms or a duration string. Default: the backend's `timeout` (10 s) | desk stops sigrok after it |

It returns the readings in the order sigrok printed them, one table each:

| Field | |
|---|---|
| `channel` | `"P1"` |
| `value` | The number in the base unit: sigrok's `0.0 mV` is `0` V, `3.3 kΩ` is `3300` Ω. `nil` when sigrok printed no number (an overload) |
| `unit` | A unit from desk's table (`V`, `A`, `Ω`, `Hz`, `°C`, …, see [`model-format.md`](model-format.md)), so it matches what a model declares. A unit the table doesn't know (`°F`, `dBm`) stays as printed, with the number as printed. `nil` without a unit |
| `flags` | What sigrok printed after the unit, e.g. `{ "DC" }` or `{ "AC", "RMS", "AUTO" }` |
| `text` | Everything sigrok printed after `<channel>: `, e.g. `"4.98 V DC"` |

sigrok prints one line per channel and sample. Lines that aren't readings (headers, logic
channels) are skipped; a channel that never printed is missing from the list; no reading at all is
`endpoint_unavailable`. desk runs sigrok without `-O`: its default output prints the readings.

`ctx.sigrok.get{ … }`:

| Key | | Becomes |
|---|---|---|
| `keys` | Required: a list of config keys (`[a-z0-9_]+`), each once | one `--get <key>` per key, in order |
| `group` | Optional channel group, as for `measure` | `-g <group>` |
| `timeout` | Optional, as for `measure` | |

sigrok prints one line per key; any other number of lines is `script_failed` with what it
printed. Values are typed: `true`/`false` become booleans, numbers become numbers, text in quotes
(`'square'`) becomes the text without them, and anything else (tuples, values with a unit) the
text as printed.

`ctx.sigrok.set{ … }`:

| Key | | Becomes |
|---|---|---|
| `config` | Required: at least one key, like `acquire`'s `config` | one `--config k=v:k=v` (keys sorted), then `--set` |
| `group` | Optional channel group, as for `measure` | `-g <group>` |
| `timeout` | Optional, as for `measure` | |

desk remembers nothing between calls. After a `set`, desk calls `read()` and compares, as for
every device: a driver whose `read()` reads the setting back with `get` gets it confirmed, and a
device that forgot it is reported (`verify_failed`, or "not confirmed" when `read()` leaves it
out). A sigrok device that forgets its config between runs of `sigrok-cli` should not declare
settings at all. Failures are the same as for `acquire`; an unknown key, a group the device
doesn't have and every other refusal come as `script_failed` with sigrok's own text.

## Drivers of declared capabilities

Any capability other than `power` and `source` is declared: the binding (or the model)
lists its `values`, `settings` and `actions` (format: [`model-format.md`](model-format.md)),
and the driver implements what they need:

```toml
[devices.iron.heater]
script   = "drivers/iron.lua"
port     = { path = "/dev/ttyUSB0" }
values   = { temperature = { type = "number", unit = "°C" } }
settings = { setpoint = { type = "number", unit = "°C", min = 50, max = 450 } }
actions  = { standby = { switches = true } }
```

```lua
function read(ctx)                   -- required if anything is readable: every value and setting
  return { temperature = 312.5, setpoint = 330 }
end
function set(ctx, name, value) end   -- required if there are settings
function action(ctx, name, args) end -- required if there are actions; args = { seconds = 10, … }
function stream(ctx, name, args) end -- required if there are streams (see *Streams*)
function open(ctx) end               -- optional: start of a session (connect, handshake)
function close(ctx) end              -- optional: end of a session (always called)
```

- `read()` returns **everything at once** (one round trip; `watch` calls it in a loop), numbers in
  the declared unit, choices as one of their strings. Names that aren't declared, wrong types, a
  choice outside its list are `script_failed`. Leave out what the device doesn't report.
- A value declared with `units = [...]` is returned as `{ value = 4.98, unit = "V" }`.
- `set` and `action` get values that desk already parsed (units), converted (to the declared
  unit) and checked (range, bench limits). Several settings of one command arrive one by one, in
  declaration order, except that a boolean setting with `switches` that is set to off/false is
  sent first; desk calls `read()` afterwards to verify them.
- With `channels`, every call sees `ctx.channel`.
- Arguments with a `default` arrive filled in; `optional` ones that weren't given are `nil`.

### Actions that produce a file

An action declared with `produces = "file"` writes a file that desk owns. During that call, and
only then, `ctx.output` is a handle to it. The handle is opaque: it has no methods and doesn't
tell the script where the file is. Pass it to a host function that fills it, as `into =
ctx.output`, and return a table that describes the file:

```lua
function action(ctx, name, args)
  local image = read_screenshot(ctx)                 -- your own code, e.g. over ctx.serial
  ctx.append{ into = ctx.output, data = image }      -- bytes go into the file
  return { format = "png", width = 800, height = 480 }
end
```

| Key | |
|---|---|
| `format` | Required: what the file is, e.g. `"png"`, `"srzip"` |
| `extension` | The file name's extension, 1 to 16 lowercase letters or digits. Default: `format` |
| anything else | Reported as the result's `meta`, untouched. Plain data only (no functions or handles) |

| Host function | |
|---|---|
| `ctx.append{ into = ctx.output, data = s }` | Append the bytes of the string `s` to the file |

| `ctx.sigrok.acquire{ …, into = ctx.output }` | Capture from the binding's sigrok device (see *sigrok devices*) |

Host functions that fill the file from the device directly come with their connection kinds.
Each takes its own `timeout`; while it runs, the call's own timeout is suspended and continues
afterwards with the time it had left.

Returning nothing, no `format`, or leaving the file empty fails the action (`script_failed`).
desk writes the file in a private temporary folder (`.<name>.<pid>.partial/`) in the destination
folder, moves it to its name when the action succeeded, and removes the folder with everything in
it when the action fails, times out, goes over the size cap (`limit_exceeded`) or is cancelled.
The size cap counts every file in that folder. The
result reports `file` (the absolute path), `bytes`, `format` and `meta`.

### Streams

A capability that declares `streams` (format: [`model-format.md`](model-format.md)) needs
`stream(ctx, name, args)`. During that call `ctx.output` is the stream's sink, an opaque handle
like a capture's. The driver describes the data once, then fills the sink until the source ends,
and returns:

```lua
function stream(ctx, name, args)
  ctx.describe{
    into       = ctx.output,
    unit_size  = 1,                                  -- bytes per sample
    samplerate = args.samplerate,
    channels   = { "D0", "D1", "D2", "D3" },          -- bit order: the first is bit 0
    driver     = ctx.sigrok.driver,                  -- anything else is `meta`
  }
  ctx.sigrok.acquire{ config = { samplerate = args.samplerate }, channels = "D0,D1,D2,D3",
                      continuous = true, into = ctx.output }
end
```

| `ctx.describe{ into = ctx.output, … }` | |
|---|---|
| `logic` content | `unit_size` (a whole number, 1 to 8), `samplerate` (above 0) and `channels` (a non-empty list of names, at most `unit_size * 8`) are required; they become the header's own fields |
| any other content | `unit_size` (default 1) is taken if present, so blocks hold whole units |
| everything else | the header's `meta`. Plain data only |

`ctx.describe` comes once, before the first byte: filling an undescribed stream, describing it
twice or describing a capture's file is `script_failed`. Returning without describing fails the
stream. The data is filled with the same host functions as a capture: `ctx.append{ into =
ctx.output, data = s }` and `ctx.sigrok.acquire{ …, into = ctx.output }`.

Timeouts: until the description, the call's timeout applies. After that every `ctx.append`
starts it again, and `acquire` suspends it, so a driver that keeps delivering never runs out of
time. A source that delivers nothing for 10 s ends the stream with `timeout`. When the last
subscriber leaves, the stream is stopped: host functions that fill it end (sigrok is killed),
`ctx.append` fails with `cancelled`, and the call is dropped if it doesn't return within 2 s.

Everything else is desk's, the same for every driver: units, ranges and limits before anything is
sent, the order of settings, verification (`verified`, or "not confirmed" for settings `read()`
doesn't return), dry runs, sessions with timeouts (default `10s`), streaming.

## Power supply drivers (`source = { script = … }`)

For a bench power supply (`voltage_source`, a well-known capability with fixed names):

```toml
[devices.psu]
source = { script = "drivers/my_psu.lua", timeout = "10s", ratings = { max_voltage = 30, max_current = 5 } }
limits = { source = { voltage = 12, current = 2 } }   # bench safety, enforced by desk, not by the driver
```

```lua
function set_voltage(ctx, volts) ... end   -- required
function set_current(ctx, amps) ... end    -- required
function set_output(ctx, on) ... end       -- required
function read(ctx)                         -- required: report what the device can tell
  return { output = true, voltage_set = 5, current_set = 1, voltage = 5.01, current = 0.2, power = 1.0 }
end
function info(ctx) return { max_voltage = 30, max_current = 5 } end   -- optional: maximums the device reports
function open(ctx) ... end                 -- optional: start of a session (connect, handshake)
function close(ctx) ... end                -- optional: end of a session
```

`read()` fields, all optional (leave out what the device doesn't report). Users see them under the
generic names in parentheses (`desk get psu`):

| Field | Meaning |
|---|---|
| `output` | output terminals on (`true`) or off (`output`) |
| `voltage_set`, `current_set` | setpoints, V and A (`voltage`, `current`) |
| `voltage`, `current`, `power` | measured at the output, V, A, W (`measured_voltage`, `measured_current`, `measured_power`) |
| `mode` | `"cv"` (holding the voltage) or `"cc"` (current limit reached); ignored while the output is off |
| `protection` | `"none"`, or what tripped: `"ovp"`, `"ocp"`, `"opp"`, `"otp"`, `"uvp"` (input under-voltage), `"other"` |
| `temperature` | internal temperature, °C |
| `input_voltage` | the supply's own input (e.g. its internal bus), V |
| `charge`, `energy` | the device's delivered-charge and energy counters, Ah and Wh |

Declare the device's maximums as `ratings` in the binding (`ratings = { max_voltage = 30,
max_current = 5 }`); desk enforces them (and the stricter value if `info()` also reports some).

**Declare your params** so desk can check them: unknown names, wrong types and missing required
params become config errors, and defaults are filled in before your functions run.

```lua
PARAMS = {
  address   = { type = "integer", required = true, doc = "bus address" },
  settle_ms = { type = "number", default = 300, doc = "wait before measuring" },
}
```

Types: `string`, `number`, `integer`, `boolean`, `table`, `array`. Without `PARAMS`, params are
passed through unchecked (fine for your own quick scripts; drivers in `models/` must declare it, see
[`model-format.md`](model-format.md)).

Drivers only implement primitives. Everything users rely on is done by desk itself, the same for
every driver:

- **Sessions:** one `open` … `close` per command around all steps. `close` is called even after
  errors and timeouts (devices that lock their front panel while connected get released). When
  the command succeeded and `close` then fails, the command still counts: its result is
  reported, with the error from `close` as a warning.
- **Limits:** values above `info()`'s maximum or the device's bench `limits` are refused before
  the driver is called. Switching the output on is also refused when the device *already* holds a
  setpoint above the bench limits (as far as `read()` reports it).
- **Order:** voltage and current before the output is switched on (`desk set psu output on
  voltage 5v` is applied in that order). When a command switches the output off, that comes first.
- **Channels:** with `channels = 2` on the binding, every call sees `ctx.channel`; users address
  `psu:1`, and `limits` can be per channel (`limits = { source = { voltage = 12, "2" = { voltage
  = 5 } } }`).
- **Protections:** while `read()` reports a tripped `protection`, switching the output on is
  refused (`protection_tripped`) unless the user passes `--clear-protection`. If a protection trips
  right after switching on, that's reported as `protection_tripped` too.
- **Verification:** after the steps, `read()` is compared with the request. Fields `read()` doesn't
  return are reported as *not confirmed*, never as confirmed. Return only what the device really
  reports.
- **`timeout`** (default `10s`) covers the whole session of one command.

## Sandbox

Only the `string`, `table`, `math`, `utf8` and `coroutine` libraries are available. There is no
`io`, `os`, `require`/`package`, `debug`, `dofile`, `loadfile` or `string.dump`: a script can only
reach the outside world through `ctx`. `load` compiles source text only, never precompiled chunks.
A script may use up to 64 MiB of memory. One `port:read` returns at most 64 KiB; ask again for more.
`print` writes to the log, so `--json` output stays parseable. The timeout also interrupts busy
loops written in Lua, and it can't be caught: `pcall`, `xpcall` and `coroutine.resume` catch other
errors as usual and pass a timeout on. Clean up in `close`, which gets its own time.

## Examples

- [`examples/scripts/iron_with_extractor.lua`](../examples/scripts/iron_with_extractor.lua): two
  outlets in order, with delays; state comes from the iron outlet
- [`examples/scripts/double_press.lua`](../examples/scripts/double_press.lua): a send-only device
  that needs every command twice, 50 ms apart
- [`examples/sigrok/demo.lua`](../examples/sigrok/demo.lua): a capture action on stock sigrok's
  `demo` driver (`desk --config examples/sigrok/desk.toml do la capture duration=200ms`, needs
  sigrok-cli)
- [`examples/sigrok/demo_meter.lua`](../examples/sigrok/demo_meter.lua): a meter on the same
  device, with `measure` and `get` (`desk --config examples/sigrok/desk.toml get dmm`)

Try them without hardware: `desk --config examples/desk.toml power on station`.
