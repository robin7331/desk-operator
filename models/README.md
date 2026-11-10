# Models

A **model** holds what's true for every unit of a product (driver, USB ids, ratings, what it can
do, sources). Nothing in here is about a particular bench: how *your* device gets mains power, your
names, your bench `limits` and which unit you have stay in your own `desk.toml`.

`build.rs` compiles this folder into `desk`, so a model added here reaches everyone with the
next build. The list of devices is in the [main README](../README.md#supported-devices).

## Using a model

```toml
[devices.psu]
model  = "fnirsi/ips3608"                                    # everything model-specific
power  = { provider = "ha", entity = "switch.desk_outlet_2" } # yours: how this unit gets mains
source = { port = { serial_number = "0123456789AB" } }        # yours: which unit (merged in)
limits = { source = { voltage = 12, current = 2 } }           # yours: bench safety
```

Your own values **merge** into the model's bindings key by key (tables like `port` and
`params` merge, other values replace). A binding of your own with `script` or `provider` replaces
the model's binding entirely. A device can tighten what the model states (lower `ratings` and
`max`, higher `min`) but not loosen it: that is `config_invalid`.

desk looks models up in this order, first match wins:

1. `models/` next to your `desk.toml`: your own models
2. directories in `$DESK_MODELS`: e.g. this folder in a clone, to test changes without rebuilding
3. the models bundled into desk

## Layout

```
models/
  <vendor>/
    README.md                  protocol notes, sources, known gaps, observed behavior
    shared/<protocol>.lua      drivers shared by several models (protocol families)
    <model>/model.toml        one folder per model
tests/
  model_<vendor>_<family>.rs  the driver against a fake device, one file per protocol family
```

The format is specified in [`docs/model-format.md`](../docs/model-format.md) (JSON Schema
for editors: [`schemas/model.schema.json`](../schemas/model.schema.json)),
the Lua host API in [`docs/scripting.md`](../docs/scripting.md). In short: `[model]` with `id`,
`name`, `requires`, `status` (`community` | `tested`), optional `tested` and `sources`; one table per
capability (`[source]` for power supplies, or a declared capability such as `[meter]` with its
`values`, `settings` and `actions`), each with a `script`, ratings or ranges (enforced by desk), and
`params` for the driver.

Rules:

- **Scripts only.** A model may not use `provider = …`: providers (Home Assistant, …) are
  set up per bench.
- **Model facts only.** No entity ids, no bench limits, no serial numbers.
- **Prefer data over code.** If two models differ only in numbers (byte offsets, timings), share
  one driver and put the numbers in each model's `params`.
- **Identify the model** before sending anything when the USB id isn't unique to it (many
  microcontroller vendors use one generic id for all their customers' products).
- **Honest status and ratings:** `tested` only after a real smoke test, with versions in `tested`.
  Leave a rating out rather than guess it. Drivers report only what the device really tells them.
- **No workarounds for missing desk features.** If a device needs something desk can't do (a new
  connection type, a host API function), that's a change to desk itself, not a driver hack.

## Contributing a model

An agent can walk you through all of it: `desk skill install` installs the `desk-add-device` skill
(research, probing, fake-device test, smoke test, pull request). By hand, in a clone of this
repository:

1. Branch (`device/<vendor>-<model>`), then
   `desk model new <vendor>/<model> --capability <name> --name "…" --dir models`.
2. Find out how the device talks: manuals, community protocol work, a capture of the vendor's
   software. Note sources in the vendor `README.md`. `desk port probe` sends documented
   read-only queries and dumps the replies as hex (it checks no limits: never send unknown
   commands).
3. Reuse a family driver if one fits (`--driver ../shared/x.lua`); otherwise implement the skeleton.
4. Fill in ratings, declarations, params and sources; `desk model check models/<vendor>/<model>/model.toml`;
   add a fake-device test in `tests/model_<vendor>_<family>.rs` (`#[path = "support/models.rs"] mod harness;`
   gives it this checkout's `desk`).
5. Smoke-test on real hardware with nothing connected to the outputs, at low values, using
   `DESK_MODELS=$PWD/models`. Record what you observed (boot time, ramps, quirks) in the vendor
   `README.md`, and set `status`/`tested`.
6. Add the device to *Supported devices* in the main README, sorted by vendor and model (a test
   checks it).
7. Run `cargo test`, `cargo clippy --all-targets -- -D warnings` and `cargo fmt`.
8. Open a pull request: what the model supports, tested versions, what the smoke test
   verified, sources, known gaps.
