# sigrok devices

[sigrok](https://sigrok.org) drives many logic analyzers, oscilloscopes, multimeters and other
instruments. To desk, sigrok is a **connection kind**, like a serial port: a model's binding names
a sigrok driver, and its Lua driver captures through it (`ctx.sigrok.acquire`, see
[`scripting.md`](scripting.md)).

desk never links sigrok's library. It runs a `sigrok-cli` program as a separate process and reads
the file it writes. sigrok is GPLv3+ and desk is MIT OR Apache-2.0, so desk doesn't bundle sigrok
or download it: you install it yourself.

```sh
desk do la capture duration=2s samplerate=10MHz    # a file in the capture folder
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

## Troubleshooting

| Error | Meaning | What to do |
|---|---|---|
| `provider_unreachable`: sigrok-cli not found | No `sigrok-cli` on PATH, or not at the provider's `program` | Install sigrok, or set `program` |
| `config_invalid`: no sigrok backend has the driver | No backend lists the model's driver | `desk sigrok drivers`; add a build that has it (the message names the model's `build` link if it has one) |
| `endpoint_unavailable`: found no such device | sigrok found nothing for this driver (and `conn`) | Plugged in, powered, the right `conn` (`desk provider endpoints`), udev rules |
| `endpoint_unavailable`: the device is busy | Another program holds the device ("Unable to claim USB interface") | Close it (PulseView, another desk), then try again |
| `timeout` | The capture took longer than the driver's `timeout` (a trigger that never came, say) | Check the trigger and the duration |
| `limit_exceeded` | The file grew over the size cap | Capture less, or `--max-size` |
| `script_failed` with sigrok's text | Anything else sigrok refused, e.g. `unknown channel 'Z9'.` | Usually the driver's request; `desk sigrok show <driver>` lists what the device takes |

## Trying it without hardware

Stock sigrok has a `demo` driver that needs no device:

```sh
desk --config examples/sigrok/desk.toml do la capture duration=200ms
desk --config examples/sigrok/desk.toml do la capture duration=1s samplerate=100khz format=binary
```
