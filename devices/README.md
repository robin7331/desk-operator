# Device library

Device **definitions**: what's true for every unit of a model (driver, USB ids, maximums, docs,
tests). Nothing in here is about a particular bench: how *your* device gets mains power, your
names, your bench `limits` and which unit you have stay in your own `desk.toml`.

These definitions are bundled into desk at build time (`desk library list`). This folder lives in
the main repository while the driver contract still changes; it's meant to become its own
repository (`desk-operator-devices`) later, so every definition stays self-contained.

## Using a definition

```toml
[devices.psu]
model  = "fnirsi/ips3608"                                    # everything model-specific
power  = { provider = "ha", entity = "switch.desk_outlet_2" } # yours: how this unit gets mains
source = { port = { serial_number = "0123456789AB" } }        # yours: which unit (merged in)
limits = { voltage = 12, current = 2 }                        # yours: bench safety
```

Your own values **merge** into the definition's bindings key by key (tables like `port` and
`params` merge, other values replace). A binding of your own with `script` or `provider` replaces
the definition's binding entirely.

Definitions are looked up in this order, first match wins:

1. `devices/` next to your `desk.toml` — your own or unpublished definitions
2. directories in `$DESK_DEVICES` — e.g. a checkout of this library while working on it
3. the definitions bundled into desk

## Layout and format

```
devices/<vendor>/
  README.md                  protocol notes, sources, known gaps, observed behavior
  shared/<protocol>.lua      drivers shared by several models (protocol families)
  <model>/device.toml        one definition per model
```

```toml
[device]
id       = "vendor/model"          # must match the folder
name     = "Vendor Model"
kind     = "bench power supply"
requires = "desk-operator >= 0.1"  # the oldest desk the driver works with
status   = "tested on hardware"    # or "community" (not yet tested on hardware)
tested   = ["firmware V1.1 / hardware V1.2"]

[source]                           # one table per capability: power, source
script = "../shared/protocol.lua"  # relative to this file
port   = { usb_vid = 0x1234, usb_pid = 0x5678 }

[source.params]                    # data for the driver: maximums, offsets, model name, timings
max_voltage = 36.0
max_current = 8.0
```

Rules:

- **Scripts only.** A definition may not use `provider = …`: providers (Home Assistant, …) are
  set up per bench.
- **Model facts only.** No entity ids, no bench limits, no serial numbers.
- **Prefer data over code.** If two models differ only in numbers (maximums, byte offsets),
  share one driver and put the numbers in each definition's `params`.
- **Identify the model** before sending anything when the USB id isn't unique to it (many
  microcontroller vendors use one generic id for all their customers' products).
- **Honest status:** `tested on hardware` only after a real smoke test, with firmware versions in
  `tested`. Drivers report only what the device really tells them (desk shows the rest as *not
  confirmed*).

Drivers implement a capability's script contract, see [`docs/scripting.md`](../docs/scripting.md).

## Checking a definition

```sh
desk library check                       # all definitions on the search path
desk library check vendor/model          # one by id
desk library check path/to/device.toml   # one that isn't on the search path yet
```

`check` validates the format and loads every driver in the sandbox (required functions present),
without touching hardware. Protocol tests against a fake device currently live in the main
repository's `tests/device_*.rs` (e.g. `tests/device_fnirsi_dps150.rs`); they'll move with the
library when it becomes its own repository.

## Contributing a definition

1. Find out how the device talks: manuals, community protocol work, a capture of the vendor's
   software. Note sources in the vendor `README.md`.
2. Reuse a family driver if one fits; otherwise write one against the script contract.
3. Write the definition, run `desk library check`, add a fake-device test.
4. Smoke-test on real hardware with nothing connected to the outputs, at low values. Record what
   you observed (boot time, ramps, quirks) in the vendor `README.md`, and set `status`/`tested`.
