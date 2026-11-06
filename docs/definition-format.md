# Device definition format (v2)

A **device definition** describes what's true for every unit of a model. It lives at
`<vendor>/<model>/device.toml` in a device library and is used by devices with
`model = "<vendor>/<model>"`. A **device** (in the user's `desk.toml`) adds what's specific to one
bench: how it gets mains power, names, bench limits, which unit.

Machine-readable: [`schemas/device-definition.schema.json`](../schemas/device-definition.schema.json)
(for editors: `#:schema ../../../schemas/device-definition.schema.json` as the first line). desk
validates more than the schema can express: `desk library check`.

## Example

```toml
[device]
id       = "fnirsi/ips3608"
name     = "FNIRSI IPS3608"
requires = "desk-operator >= 0.1"
status   = "tested"
tested   = ["firmware V1.1 / hardware V1.2"]
sources  = ["https://github.com/hamsternz/fnirsi_psu"]

[source]
script  = "../shared/dps150_protocol.lua"
port    = { usb_vid = 0x2e3c, usb_pid = 0x5740 }
ratings = { max_voltage = 36.0, max_current = 8.0 }

[source.params]
name      = "IPS3608"
settle_ms = 1000
status    = { voltage_set = 4, current_set = 8, voltage = 12, current = 16, power = 20, output = 107 }
```

## `[device]`

| Field | Required | Type | Meaning |
|---|---|---|---|
| `id` | **yes** | `"vendor/model"` | lowercase letters, digits, `-`, `_`; must match the folder |
| `name` | **yes** | string | human-readable model name; default name of devices using it |
| `requires` | **yes** | `"desk-operator >= x.y"` | oldest desk the driver works with (checked) |
| `status` | **yes** | `"community"` \| `"tested"` | `tested` only after a smoke test on real hardware |
| `tested` | if `tested` | list of strings | hardware/firmware versions it was tested with |
| `sources` | no | list of strings | manuals, protocol docs, community work it's based on |

No other fields. What kind of device it is follows from its capabilities.

v2 adds declared capabilities, channels and units; every v1 definition is a valid v2 definition.

## Capabilities

Every top-level table other than `[device]` is a capability; the table's name is its key. Each is
a **script binding**: definitions never use `provider = …` (providers such as Home Assistant are set
up per bench).

| Table | Capability | Driver functions (see [`docs/scripting.md`](scripting.md)) | Required |
|---|---|---|---|
| `[power]` | `power_switch` (mains), well-known | `on`, `off`; optional `state` | nothing |
| `[source]` | `voltage_source` (a bench power supply), well-known | `set_voltage`, `set_current`, `set_output`, `read`; optional `open`, `close`, `info` | `ratings` with `max_voltage`, `max_current` |
| any other name (`[meter]`, `[heater]`, …) | a **declared** capability | `read`, `set`, `action` as its declarations need; optional `open`, `close` | at least one of `values`, `settings`, `actions` |

Names: lowercase letters, digits, `_`; not `device` or `mains`. A table without declarations
(e.g. a typo like `[sorce]`) is an error, not a new capability.

The well-known capabilities have names fixed by the core, so every power supply looks the same:
`source` has the settings `voltage`, `current`, `output` and the values `measured_voltage`,
`measured_current`, `measured_power`, `mode`, `protection`, `temperature`, `input_voltage`,
`charge`, `energy` (whatever the driver's `read()` reports), and the core's stricter rules
(setpoints before output, protection handling). `source` may declare `channels`, but no extra
values, settings or actions yet.

### Declarations (declared capabilities)

```toml
[heater]
script = "iron.lua"
port   = { usb_vid = 0x1234, usb_pid = 0x5678 }

[heater.values]                    # what the device reports
temperature = { type = "number", unit = "°C", doc = "tip temperature" }

[heater.settings]                  # what it reports and you can change, applied in this order
setpoint = { type = "number", unit = "°C", min = 50, max = 450, tolerance = 2 }
mode     = { type = "choice", choices = ["normal", "boost"] }

[heater.actions]                   # what it does on request
standby = { switches = true, doc = "heater to standby temperature" }
boost   = { switches = true, args = { seconds = { type = "number", unit = "s", min = 1, max = 60 } } }
```

| Field | For | Meaning |
|---|---|---|
| `type` | all | `number`, `boolean`, `choice`, `string` (required) |
| `unit` | numbers | the unit (see *Units*); a number without one is unitless |
| `units` | number **values** | the possible units of a value whose unit changes (a multimeter's reading); the driver returns `{ value = 4.98, unit = "V" }` |
| `min`, `max` | numbers | the device's range, **enforced by desk** before anything is sent |
| `tolerance` | numbers | how far a read-back may differ and still count as confirmed (default 0.5 % or 0.011) |
| `choices` | choices | the allowed strings |
| `doc` | all | what it means |
| `switches` | settings, actions | a physical effect (heats, outputs, moves). Settings default to `true`; actions must say it; values can't switch |
| `args` | actions | the action's arguments, declared like settings; every argument is required |

`channels = 2` (or `channels = ["ch1", "ch2"]`) on a capability gives every value, setting and
action once per channel: the driver gets `ctx.channel`, users address `<device>:<channel>`.

Names are unique within a capability; where two capabilities of a device use the same name, users
write `<capability>.<name>` (`heater.setpoint`).

### Units

Definitions use these symbols (`desk library check` rejects `"C"` for `"°C"`); users may type the
spellings in the middle column, with SI prefixes where allowed. Prefixes are case-sensitive (`m`
milli, `M` mega), unit letters are not.

| Unit | Typed as | Prefixes |
|---|---|---|
| `V`, `A`, `W` | `v`, `a`, `w` | yes |
| `Ω` | `ohm`, `r`, `Ω` | yes |
| `Hz`, `F`, `H`, `g`, `Pa`, `Ah`, `Wh` | lowercase | yes |
| `s` | `s`, also `min`, `h` | yes |
| `°C` | `c`, `°c`, `degc` | no |
| `K` | `k` | yes |
| `%` | `%` | no |

Other units (`ppm`, `%RH`, `dBm`) are allowed: typed exactly as declared (any case), no prefixes.
Values always come out in the declared unit, without prefixes.

Fields of a capability table:

| Field | Required | Meaning |
|---|---|---|
| `script` | **yes** | Lua driver, relative to this file (often `../shared/<protocol>.lua`) |
| `ratings` | per capability (above) | the device's maximums in the capability's standard form. **Enforced by desk**, not by the driver |
| `params` | no | data for the driver, **validated against the driver's `PARAMS`** (see below) |
| `port` | no | `usb_vid`, `usb_pid` of the model. Per-unit values (`serial_number`, `path`) belong in the user's config |
| `timeout` | no | per command, e.g. `"10s"` (defaults: 30 s power, 10 s source) |

### Ratings (`voltage_source`)

| Field | Required | Unit |
|---|---|---|
| `max_voltage` | **yes** | V, > 0 |
| `max_current` | **yes** | A, > 0 |
| `max_power` | no | W, > 0 (informational) |

desk refuses anything above the ratings before the driver is called (`limit_exceeded`). If a driver
also reports maximums from the device (`info()`), the stricter value wins. Only state what you
know: leave `max_power` out rather than guess.

### Driver params (`PARAMS`)

Every library driver declares what it accepts (may be empty):

```lua
PARAMS = {
  name      = { type = "string", doc = "expected model name" },
  settle_ms = { type = "number", default = 300, doc = "wait before measuring after a command" },
  address   = { type = "integer", required = true, doc = "bus address" },
}
```

`type`: `string`, `number`, `integer`, `boolean`, `table`, `array`. desk rejects unknown params
(typos), wrong types and missing required ones, and fills in defaults, so drivers can rely on
`ctx.params.settle_ms` being set. `desk library show <id>` lists a definition's driver params.

## On the user's side

```toml
[devices.psu]
model  = "fnirsi/ips3608"
power  = { provider = "ha", entity = "switch.desk_outlet_2" }  # how this unit gets mains
source = { port = { serial_number = "0123456789AB" } }         # which unit (merged into [source])
limits = { source = { voltage = 12, current = 2 } }            # bench safety, per capability

[devices.iron]
model  = "acme/iron-t12"
heater = { port = { serial_number = "…" } }
limits = { heater = { setpoint = 380, ch2 = { setpoint = 300 } } }   # a number = maximum, or
                                                                       # { min, max }; per channel
```

Limits are per capability and setting: a number is the maximum, `{ min = …, max = … }` both. A
table named like a channel overrides them for that channel. `source` limits are maximums.

The device's tables **merge** into the definition's key by key (tables merge, values replace). A
device table with its own `script` or `provider` replaces the definition's binding entirely.

## Tooling

```sh
desk library new vendor/model --capability voltage_source --name "Vendor Model" \
    --max-voltage 30 --max-current 5 [--driver ../shared/x.lua] [--dir ./devices]
desk library new vendor/model --capability heater --name "Vendor Model"   # a declared capability
desk library check [vendor/model | path/to/device.toml]
desk library show vendor/model
```

`new` creates `device.toml`, a driver skeleton with every required function and an empty `PARAMS`,
and a vendor `README.md` if there's none. It then runs `check`, which names whatever is still
missing (e.g. ratings left at `0.0`).
