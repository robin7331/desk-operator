# sigrok devices

[sigrok](https://sigrok.org) drives many logic analyzers, oscilloscopes, multimeters and other
instruments. To desk, sigrok is a **connection kind**, like a serial port: a model's binding names
a sigrok driver, and its Lua driver captures, takes readings and changes settings through it
(`ctx.sigrok.acquire`, `measure`, `get` and `set`, see [`scripting.md`](scripting.md)).

desk never links sigrok's library. It runs a `sigrok-cli` program as a separate process and reads
the file it writes or what it prints. sigrok is GPLv3+ and desk is MIT OR Apache-2.0, so desk
doesn't bundle sigrok or download it: you install it yourself.

```sh
desk do la capture duration=2s samplerate=10MHz    # a file in the capture folder
desk get dmm                                       # a meter's reading
desk set psu voltage 5v                            # a supply's setpoint, read back
desk stream la samples samplerate=10MHz | my-tool  # live samples for another program
```

## Getting sigrok

- **Stock sigrok:** install `sigrok-cli` from your package manager, or from
  [sigrok.org](https://sigrok.org). desk finds it on `PATH` as the backend `sigrok`, with no
  configuration.
- **A vendor build:** some devices are only supported by a sigrok build from their vendor. Add it
  as a provider of its own, next to stock sigrok:

  ```toml
  [providers.vendor]
  kind    = "sigrok"
  program = "/opt/vendor/sigrok-cli"   # relative paths are relative to desk.toml; ~/ works
  timeout = "10s"                      # for the short queries (version, drivers, scan, show)
  ```

  `program` can be a shell wrapper script or an AppImage: desk runs it with its arguments, without
  a shell. A wrapper script must start the program with `exec`, because desk stops only the
  process it started: a wrapper that keeps running a child of its own can leave sigrok running
  after a timeout or Ctrl-C, with the device still held. Without `program`, a `kind = "sigrok"` provider runs `sigrok-cli` from `PATH`.

A model whose driver isn't in stock sigrok may say where a build comes from (`build = "…"` in the
model). desk only shows that link in its error message; it never downloads anything.

## Which backend runs a device

```toml
[devices.la]
model   = "acme/la-8"                                  # the model gives sigrok's `driver`
capture = { sigrok = { conn = "1.23", backend = "vendor" } }   # both optional, per bench
```

1. The device's `backend`, if it names one (a `kind = "sigrok"` provider, or `sigrok`).
2. Otherwise, if there is only one backend, that one.
3. Otherwise, the first backend whose driver list has the driver: `sigrok` first, then the
   providers in config order. desk asks each backend for its drivers once per process.

`conn` is sigrok's connection string for the unit: a USB bus address such as `1.23`, or a serial
port such as `/dev/ttyUSB0`, maybe with options (`/dev/ttyUSB0:serialcomm=9600/8n1`). It is passed
as `-d <driver>:conn=<conn>`. Without it, sigrok picks the device it finds.

## Commands

| | |
|---|---|
| `desk provider list` | Your providers, and `sigrok` when `sigrok-cli` is on PATH |
| `desk provider test <backend>` | The program, its version and how many drivers it has |
| `desk provider endpoints <backend>` | What sigrok's scan finds, as `{ driver, conn }` to paste into a binding. A plain scan finds USB devices; serial ones need their `conn` |
| `desk sigrok drivers [--backend <id>]` | Every backend with its drivers (a backend whose program is missing is listed with its error) |
| `desk sigrok show <driver> [--conn …] [--backend <id>]` | sigrok's own description of the device (`--show`): channels, config keys, their ranges. Unparsed, for writing a model |

`desk sigrok show` and `desk provider endpoints` run in their own process, not through the
daemon, and `show` opens the device: while a capture runs on it, sigrok reports it busy.

While the daemon runs (`desk watch` starts it), commands for one device wait for each other, so a
second capture waits for the first. Without it, a second process meets a busy device.

## Permissions and platforms

- **Linux:** a USB device needs permission for your user. sigrok's packages, or the vendor, ship
  udev rules for that. Without them the device is not found, or found only as root.
- **AppImages** need FUSE to run.
- **Windows:** sigrok's USB devices need a WinUSB driver for each device (for example installed
  with Zadig). desk itself has not been tried with sigrok on Windows yet.

## Captures

A capture is an action that produces a file (`produces = "file"`). desk writes it in a private
temporary folder in the device's capture folder, and moves it to its name when sigrok succeeded.
A failed, timed-out, cancelled (Ctrl-C) or oversized capture leaves nothing behind, including
files sigrok wrote next to it. The size cap (`--max-size`, `[captures] max_size`, default 1 GiB)
is checked while the file grows.

sigrok's warnings on stderr (stock builds print Python decoder warnings on every run) never fail
a capture: only its exit code does. When it fails, its last lines become the error message.

## Streams

A logic analyzer's model can declare a stream (`[capture.streams.samples]`, see
[`model-format.md`](model-format.md)), and its driver's `stream()` runs
`ctx.sigrok.acquire{ continuous = true, into = ctx.output }`. desk then runs
`sigrok-cli … --continuous -O binary` without `-o` and reads its stdout; the daemon passes the samples to
everyone who runs `desk stream` (see [`streaming.md`](streaming.md)). One `sigrok-cli` serves all
of them, and it is killed when the last one stops.

- With `-O binary` sigrok writes one byte per sample for up to 8 channels, and packs the enabled
  channels in the order they were enabled: with only D3 and D7 enabled, D3 is bit 0 and D7 bit 1.
  The driver describes `channels` in that order.
- sigrok's stdin stays open: `sigrok-cli --continuous` stops at once when its stdin ends.
- The `demo` driver at 100 MHz delivers about 94 MB/s into a pipe, and desk passes that on (the
  measurements are in [`streaming.md`](streaming.md)). A real device is limited by its USB link.

## Values and settings

Multimeters, power supplies and loads that sigrok drives work like any other device: `desk get`,
`desk set` and `desk watch`. Their driver's `read()` takes readings with `ctx.sigrok.measure`
and reads config keys with `ctx.sigrok.get`; its `set()` changes them with `ctx.sigrok.set`.
`desk sigrok show <driver>` lists the channels, channel groups and config keys a device has.

- **One process per call.** Every `measure`, `get` and `set` starts `sigrok-cli` and opens the
  device again, so a reading takes as long as sigrok needs to start and connect (0.1 to 0.2 s
  for the `demo` driver, more for a real device). `desk watch` on a sigrok device is slower
  than on a serial device.
- **Settings only for devices that keep them.** desk remembers nothing: after `desk set`, the
  driver's `read()` reads the setting back with `get`, and desk confirms it or reports that it
  was not confirmed. A device that forgets its config when `sigrok-cli` exits (sigrok's `demo`
  device does) should not declare settings: its `get` reads values only.
- **Readings come in the base unit.** sigrok prints `0.0 mV` or `3.3 kΩ`; the driver gets `0` V
  and `3300` Ω, so a model declares `V` and `Ω` (or `units = [...]` for a meter whose unit
  changes).

For debugging by hand: `sigrok-cli -d <driver> --get a:b` prints the values of several keys in
an order of its own, not the order asked for. desk passes one `--get <key>` per key, which keeps
the order: `sigrok-cli -d demo -g A0 --get amplitude --get pattern`.

## Troubleshooting

| Error | Meaning | What to do |
|---|---|---|
| `provider_unreachable`: sigrok-cli not found | No `sigrok-cli` on PATH, or not at the provider's `program` | Install sigrok, or set `program` |
| `config_invalid`: no sigrok backend has the driver | No backend lists the model's driver | `desk sigrok drivers`; add a build that has it (the message names the model's `build` link if it has one) |
| `endpoint_unavailable`: found no such device | sigrok found nothing for this driver (and `conn`) | Plugged in, powered, the right `conn` (`desk provider endpoints`), udev rules |
| `endpoint_unavailable`: the device is busy | Another program holds the device ("Unable to claim USB interface") | Close it (PulseView, another desk), then try again |
| `endpoint_unavailable`: the device sent no readings | `measure` printed no reading for its channels | The channel names (`desk sigrok show`); a device that needs more `samples` or `time` |
| `timeout` | The capture took longer than the driver's `timeout` (a trigger that never came, say), or a reading or setting longer than its own | Check the trigger and the duration; for readings, the backend's `timeout` |
| `limit_exceeded` | The file grew over the size cap | Capture less, or `--max-size` |
| `script_failed` with sigrok's text | Anything else sigrok refused, e.g. `unknown channel 'Z9'.`, `Unknown option 'x'` or `Invalid channel group 'X'` | Usually the driver's request; `desk sigrok show <driver>` lists what the device takes |

## Trying it without hardware

Stock sigrok has a `demo` driver that needs no device:

```sh
desk --config examples/sigrok/desk.toml do la capture duration=200ms
desk --config examples/sigrok/desk.toml do la capture duration=1s samplerate=100khz format=binary
desk --config examples/sigrok/desk.toml get dmm      # the demo device's analog channels
desk --config examples/sigrok/desk.toml watch dmm --count 5
desk --config examples/sigrok/desk.toml stream la samples --discard --duration 3s
cargo run --example stream_edges -- --config examples/sigrok/desk.toml la samples
```
