# desk-operator-devices

The device library of [desk-operator](https://github.com/robin7331/desk-operator) (`desk`): device
**definitions**, what's true for every unit of a model (driver, USB ids, ratings, what it can do,
sources, tests). Nothing in here is about a particular bench: how *your* device gets mains power,
your names, your bench `limits` and which unit you have stay in your own `desk.toml`.

desk bundles this library at build time (it is a git submodule of desk-operator), so an installed
desk already knows these models: `desk library list`. A definition added here reaches everyone
with the next desk build; until then, use your clone (see *Working on definitions*).

## Using a definition

```toml
[devices.psu]
model  = "fnirsi/ips3608"                                    # everything model-specific
power  = { provider = "ha", entity = "switch.desk_outlet_2" } # yours: how this unit gets mains
source = { port = { serial_number = "0123456789AB" } }        # yours: which unit (merged in)
limits = { source = { voltage = 12, current = 2 } }           # yours: bench safety
```

Your own values **merge** into the definition's bindings key by key (tables like `port` and
`params` merge, other values replace). A binding of your own with `script` or `provider` replaces
the definition's binding entirely.

desk looks definitions up in this order, first match wins:

1. `devices/` next to your `desk.toml`: your own definitions, or a clone of this library
2. directories in `$DESK_DEVICES`: e.g. a clone of this library somewhere else
3. the definitions bundled into desk

## Layout

```
<vendor>/
  README.md                  protocol notes, sources, known gaps, observed behavior
  shared/<protocol>.lua      drivers shared by several models (protocol families)
  <model>/device.toml        one definition per model
harness/                     Rust tests: definitions against fake devices, run with your desk
  tests/<vendor>_<family>.rs
```

The format is specified in desk-operator's
[`docs/definition-format.md`](https://github.com/robin7331/desk-operator/blob/main/docs/definition-format.md)
(JSON Schema for editors:
[`schemas/device-definition.schema.json`](https://github.com/robin7331/desk-operator/blob/main/schemas/device-definition.schema.json)),
the Lua host API in
[`docs/scripting.md`](https://github.com/robin7331/desk-operator/blob/main/docs/scripting.md).
In short: `[device]` with `id`, `name`, `requires`, `status` (`community` | `tested`), optional
`tested` and `sources`; one table per capability (`[source]` for power supplies, or a declared
capability such as `[meter]` with its `values`, `settings` and `actions`), each with a `script`,
ratings or ranges (enforced by desk), and `params` for the driver.

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
- **No workarounds for missing desk features.** If a device needs something desk can't do (a new
  connection type, a host API function), that's a change to desk-operator, not a driver hack.

## Working on definitions

```sh
git clone https://github.com/robin7331/desk-operator-devices <dir>   # or fork it first
```

Clone it into `devices/` next to your `desk.toml` (path: `desk schema --json`, `data.config.path`)
and desk uses it right away; anywhere else, set `DESK_DEVICES=<dir>`. `desk library list` shows
which copy is used (`origin`), and `git pull` keeps it current.

```sh
desk library new vendor/model --capability <name> --name "…" --dir <dir>
desk library check --json                     # all definitions on the search path
desk library check <dir>/vendor/model/device.toml
cd <dir>/harness && cargo test                 # needs Rust; DESK_BIN=… to test another desk
```

`check` validates the format and loads every driver in the sandbox, without touching hardware.
The harness runs each definition against a fake device over a pseudo-terminal (Linux, macOS) with
the `desk` on your `PATH`, always using the files of this checkout.

## Contributing a definition

An agent can walk you through all of it: `desk skill install` installs the `desk-add-device`
skill (research, probing, fake-device test, smoke test, pull request). By hand:

1. Branch (`device/<vendor>-<model>`), `desk library new …`.
2. Find out how the device talks: manuals, community protocol work, a capture of the vendor's
   software. Note sources in the vendor `README.md`. `desk port probe` sends documented
   read-only queries and dumps the replies as hex (it checks no limits: never send unknown
   commands).
3. Reuse a family driver if one fits (`--driver ../shared/x.lua`); otherwise implement the skeleton.
4. Fill in ratings, declarations, params and sources; `desk library check`; a harness test.
5. Smoke-test on real hardware with nothing connected to the outputs, at low values. Record what
   you observed (boot time, ramps, quirks) in the vendor `README.md`, and set `status`/`tested`.
6. Open a pull request: what the definition supports, tested versions, what the smoke test
   verified, sources, known gaps.
