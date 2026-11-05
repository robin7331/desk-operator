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
limits = { source = { voltage = 12, current = 2 } }                        # yours: bench safety
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

The format is specified in [`docs/definition-format.md`](../docs/definition-format.md) (with a JSON
Schema for editors in [`schemas/`](../schemas/device-definition.schema.json)). In short: `[device]`
with `id`, `name`, `requires`, `status` (`community` | `tested`), optional `tested` and `sources`;
one table per capability (`[source]`, `[power]`) with a `script`, the capability's **ratings**
(e.g. `max_voltage`, `max_current`, enforced by desk), and `params` for the driver, validated
against the driver's `PARAMS` declaration.

Rules:

- **Scripts only.** A definition may not use `provider = …`: providers (Home Assistant, …) are
  set up per bench.
- **Model facts only.** No entity ids, no bench limits, no serial numbers.
- **Prefer data over code.** If two models differ only in numbers (byte offsets, timings), share
  one driver and put the numbers in each definition's `params`.
- **Identify the model** before sending anything when the USB id isn't unique to it (many
  microcontroller vendors use one generic id for all their customers' products).
- **Honest status and ratings:** `tested` only after a real smoke test, with versions in `tested`.
  Leave a rating out rather than guess it. Drivers report only what the device really tells them.

Start a new one with `desk library new vendor/model --capability voltage_source --name "…"`.

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

1. `desk library new vendor/model --capability … --dir ./devices` (in a checkout of the library).
2. Find out how the device talks: manuals, community protocol work, a capture of the vendor's
   software. Note sources in the vendor `README.md`. `desk port probe` sends documented
   read-only queries and dumps the replies as hex (it checks no limits: never send unknown
   commands).
3. Reuse a family driver if one fits (`--driver ../shared/x.lua`); otherwise implement the skeleton.
4. Fill in ratings, params and sources, run `desk library check`, add a fake-device test.

An agent can walk you through all of this: `desk skill desk-add-device` prints the skill (or
`desk skill install` installs it for Claude Code, globally or for one project).
5. Smoke-test on real hardware with nothing connected to the outputs, at low values. Record what
   you observed (boot time, ramps, quirks) in the vendor `README.md`, and set `status`/`tested`.
