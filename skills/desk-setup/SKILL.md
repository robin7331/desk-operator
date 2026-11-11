---
name: desk-setup
description: Set up a whole electronics bench with the `desk` CLI, step by step: find out what's on it, connect the mains switching, and bring in every device one at a time with its limits and a read-only check. Use when someone is new to desk, wants to set up their bench, or wants to add several devices.
---

# Setting up a bench with `desk`

You are guiding someone through their bench, probably for the first time. Keep it short: say in one
sentence what each step is for, ask one thing at a time, and finish one device before starting the
next. Pass `--json` to every `desk` command. For operating the bench later, follow the
`desk-operator` skill (`desk skill desk-operator`).

## Ground rules

1. **Nothing connected to the outputs** of a power supply or load while setting it up. Ask the user
   to confirm this before the first command that reaches that device.
2. **Every command that switches something needs the user's go-ahead for that step.** Say what will
   happen first: an iron starts heating when it gets mains, and a power supply or load may come up
   with its previous output setting. `desk get`, `desk port list` and the other listings are safe.
3. **Never pick limits or settings yourself, and don't guess facts about a device.** The user
   decides their limits. If you don't know something, ask or leave it out.
4. **Never ask for, read or print access tokens.** The user puts them in place themselves.
5. **The config belongs to the user.** Show each block before you write it, only add to the file, and
   leave what's already there alone unless they ask. It stays on their machine: nothing from it goes
   into a model or a pull request.

## 1. See what's there

```sh
desk schema --json       # data.config.path: where desk.toml goes; data.devices: what's set up
desk model list --json   # the products desk already knows
desk port list --json    # USB serial ports right now
desk sigrok drivers --json   # sigrok backends (sigrok-cli) and their drivers, if sigrok is installed
```

`data.config.ok` is `false` until there's a valid `desk.toml`. If the file is missing, you'll create
it in step 4. If it's invalid, show the message and fix that first. If devices are already set up, say
so and continue with what's missing, so this skill can be run again at any time.

## 2. Make the list

Ask what's on the bench, including the exact product names, and for each device: what should desk do
with it? Then show a plan with one row per device, and let the user correct it:

| Device | Short id | Path |
|---|---|---|
| FNIRSI IPS3608 | `psu` | **known model**: `fnirsi/ips3608` |
| Soldering iron without a data port | `iron` | **mains only**: on/off through an outlet |
| Some meter desk doesn't list | `dmm` | **new model**: `desk-add-device` |

Use a known model only when `desk model list` has that exact product. A similar name isn't enough.
If one seems to be missing, check `desk update --check --json`: a newer desk may include it.
Say what the status means: `tested` was smoke-tested on real hardware, `community` wasn't fully
verified, and even `tested` is only one unit (`desk model show <vendor/model> --json`).

Suggested order: known models first, measuring devices before anything that outputs power or heat,
and new models last, because each of those is a project of its own.

## 3. Mains switching (optional)

Ask how the devices get mains power and whether desk should switch it. Without this, desk can still
talk to a device, but the user switches it on by hand. Skip this step if they don't want it.

**Home Assistant:** add a provider to `desk.toml`, and ask the user to create a long-lived access
token and either export `HA_TOKEN` or save it to a file next to `desk.toml`, readable only by them:

```toml
[providers.ha]
kind       = "home-assistant"
url        = "http://homeassistant.local:8123"   # ask for theirs
token_file = "ha.token"                          # or leave this out and use HA_TOKEN
```

```sh
desk provider test ha --json
desk provider endpoints ha --search <text> --json   # the entity for each outlet
```

**A USB relay board or an RF dongle** on a serial port: a Lua script binding, documented in
`docs/scripting.md` of the desk-operator repository.

**A network smart plug without Home Assistant** is not supported yet, because scripts have no
network access. Use Home Assistant for it, or tell the user it needs a new provider and suggest
opening an issue.

## 4. One device at a time

**a. Find the unit.** For a device with a data port, ask the user to plug it in and switch it on by
hand, then run `desk port list --json` and compare with the first list. Skip this for a device that
is mains only.

**b. Write its block.** Create `desk.toml` at `data.config.path` if it's missing. The capability key
(`source`, `meter`, ...) is the one under `bindings` in `desk model show <vendor/model> --json`.

```toml
[devices.psu]
name    = "Bench supply"
aliases = ["supply"]
model   = "vendor/model"                                        # from `desk model list`
power   = { provider = "ha", entity = "switch.desk_outlet_1" }  # optional: from step 3
source  = { port = { serial_number = "…" } }                    # which unit, from `desk port list`
limits  = { source = { voltage = 12, current = 2 } }            # the user's numbers

[devices.iron]                                                  # mains only: no model needed
name  = "Soldering iron"
power = { provider = "ha", entity = "switch.desk_outlet_2" }
```

Identify the unit with `serial_number` when the port has one. Without one, the model's USB ids are
enough as long as only one such device is connected. With several, use `path` (on Linux, the stable
one under `/dev/serial/by-path`).

**A device that sigrok drives** (the model's binding has `sigrok = { driver = … }` in `desk model
show`) needs `sigrok-cli`, which the user installs themselves (package manager or
https://sigrok.org; desk never downloads it). Find the unit with
`desk provider endpoints sigrok --json`: each endpoint has `params` with `driver` and maybe `conn`.
Write only what identifies this unit, and which build runs it:

```toml
[devices.la]
model   = "vendor/model"
capture = { sigrok = { conn = "1.23" } }     # `conn` from `desk provider endpoints`, if needed

[providers.vendor]                           # only for a vendor's own sigrok build
kind    = "sigrok"
program = "/path/the/user/gave/sigrok-cli"   # a wrapper script must `exec` the program
```

A wrapper script as `program` must start sigrok with `exec`: desk stops only the process it
started, so a wrapper that keeps a child running can leave the device held after a timeout.

With a vendor build, add `backend = "vendor"` to the device's `sigrok` table if stock sigrok has the
same driver. If no backend has the model's driver, desk says so (`config_invalid`) and names the
model's `build` link: tell the user where the build comes from and let them install it. On Linux,
a device that isn't found may need sigrok's or the vendor's udev rules. `desk provider test
<backend> --json` checks a backend; see `docs/sigrok.md` in the project.

**c. Ask for limits.** Whatever can output power or heat gets the user's own limits, which desk then
enforces on every command. `desk device show <device> --json` lists each setting's range. A device
that only measures doesn't need any, and the model's maximums always apply anyway.

**d. Check the config:** `desk device show <device> --json`. A `config_invalid` error names what's
wrong.

**e. Check the device, read-only.** If it has mains switching, look first with
`desk power status <device> --json`, and switch it on only with the user's OK. Then:

```sh
desk get <device> --json
```

Compare what it reports with the device's display, and ask the user whether it looks right. Some
devices lock their front panel while connected. Don't send settings as a test: that's the user's
call, after setup, or part of the next step if they choose it.

**f. Offer to improve the model.** The user has the real device, which is what a model needs most.
Both offers are optional, and declining is fine: note it in the plan and move on.

- **The model is `community`:** say what its vendor README lists as unverified, and offer the smoke
  test from the `desk-add-device` skill. If it passes, the model can become `tested`.
- **A reading looks wrong, or something the user needs is missing:** offer the skill's *Improving an
  existing model* section instead of only reporting it.

Both happen in a clone, under that skill's ground rules: its go-ahead for every step that switches
something applies here too, and so does its step 10, which leaves the improved model in `models/`
next to `desk.toml`. Note any model that changed in the plan, for step 6.

**g. Leave it as you found it.** Offer to switch off whatever you switched on, then show the plan
again with this device done.

**When a check fails,** report the message, and don't work around it. `device_off`: it has no mains.
`endpoint_unavailable`: often the wrong port, the port is in use, or missing permissions on Linux
(for a sigrok device: not found, the wrong `conn`, or busy in another program).
If a device can't be fixed now, leave its block out or comment it out, mark it in the plan, and go on.

## 5. Devices desk doesn't know yet

Tell the user what this takes before starting: researching the protocol, writing a driver, testing it
against a fake device, and a smoke test together. Let them choose:

- **Add it now:** follow the `desk-add-device` skill (`desk skill desk-add-device`), then come back to
  the plan. It leaves a copy of the model in `models/` next to `desk.toml`, which must be there before
  the device goes into the config: desk stops for the whole bench when it can't find a model.
- **Mains only for now:** if it's on a switched outlet, give it a block with just `name` and `power`.
- **Later:** leave it on the list you hand over at the end.

## 6. Finish

```sh
desk status --json
```

Run this in a shell without `DESK_MODELS`, and ask the user to run `desk status` in a new terminal of
their own: that's how they'll use it, and a model only your shell can find breaks their whole bench.

Show the result: what's set up with its limits, what was only partly set up and why, and which
devices are still on. Point out `desk get`, `desk set`, `desk do`, `desk watch` and
`desk completions <shell>` for Tab completion.

**Ask about sharing.** List every new or improved model that isn't in a pull request yet, and ask
about each one: should it be contributed? Then follow step 11 of the `desk-add-device` skill, which
shows the exact files and checks them for bench details first. Nothing leaves the machine without
the user's OK, and keeping a model to themselves is fine.
