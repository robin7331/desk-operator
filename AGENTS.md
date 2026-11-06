# desk-operator-devices

The device library of desk-operator: one definition per device model (`<vendor>/<model>/device.toml`
plus a Lua driver), bundled into the `desk` binary as a git submodule of
[desk-operator](https://github.com/robin7331/desk-operator). Contributor guide: `README.md`.

## Two repositories

- **desk-operator** (the core, Rust): the CLI, safety rules, the definition format
  (`docs/definition-format.md`), the Lua host API (`docs/scripting.md`), and the agent skills
  (`desk skill list`). It never names a vendor or model.
- **desk-operator-devices** (this one): what's true for each model, and its tests.

Work that belongs in the core never goes into a driver here: a new connection type (anything but
serial today), a missing host API function, a new kind of declaration. Then stop and tell the user
what's missing and why; with their OK, open an issue on desk-operator
(`gh issue create -R robin7331/desk-operator`, check `gh issue list -R robin7331/desk-operator`
first), or a pull request there if they want it built (follow that repo's `AGENTS.md`). Describe the
gap generically, never with bench details.

## Adding or changing a device

Follow the `desk-add-device` skill (`desk skill desk-add-device` prints it; `desk skill install`
installs it). It covers asking, research, read-only probing, the fake-device test, the smoke test
with the user, their config, and the pull request. Its ground rules apply here too:

- Don't guess facts about devices (ratings, USB ids, frame layouts): a source or an observation, or
  leave it out.
- Every step that switches real hardware needs the user's go-ahead for that step; nothing connected
  to the outputs while developing.
- Probe with documented read-only queries only. Never scan or fuzz command bytes (FNIRSI: never
  send type `C0`, it enters firmware-update mode).
- Nothing bench-specific in this repo: no entity ids, serial numbers, the user's limits or paths.

## Checks

```sh
desk library check --json                       # every definition on the search path
cd harness && cargo test                        # fake-device tests with the installed desk
cd harness && cargo clippy --all-targets -- -D warnings && cargo fmt --check
```

Tests run the `desk` on `PATH` (or `$DESK_BIN`) with `DESK_DEVICES` pointing at this checkout, so
they test these files, not the copy bundled into desk. One test file per protocol family:
`harness/tests/<vendor>_<family>.rs`, the fake device on the master side of a pseudo-terminal
(see `harness/tests/fnirsi_dps150.rs`).

## Commits

Commit only when the user asks. One device (or one fix) per branch and pull request.
