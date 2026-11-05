# Device library

Device **definitions**: what's true for every unit of a model (driver, USB ids, maximums, docs,
tests). Nothing in here is about a particular bench: how *your* device gets mains power, your
names and your bench `limits` stay in your own `desk.toml`.

```
devices/
  <vendor>/
    README.md               protocol notes, sources, known gaps, status
    shared/<protocol>.lua   drivers shared by several models (protocol families)
    <model>/device.toml     one definition per model
```

**Status: format draft.** desk loads definitions from milestone M5 on (`model = "fnirsi/ips3608"` in
a device). This folder lives in the main repo while the driver contract still changes; it's meant to
become its own repository (`desk-operator-devices`) later, so every definition stays self-contained.

## Using a definition before M5

Copy its bindings into your device by hand, with paths into this folder:

```toml
[devices.psu]
power  = { provider = "ha", entity = "switch.…" }   # yours: how this unit gets mains
source = { script = "/path/to/desk-operator/devices/fnirsi/shared/dps150_protocol.lua",
           port   = { path = "/dev/ttyACM0" },
           params = { max_voltage = 36.0, max_current = 8.0 } }
limits = { voltage = 12, current = 2 }               # yours: bench safety
```

Drivers implement the `voltage_source` script contract in [`docs/scripting.md`](../docs/scripting.md).
