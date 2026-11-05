# Streaming readings (`desk source watch`) and the daemon

`desk source watch <device>` streams readings from a power supply until you stop it. Other
programs and `desk` commands can keep controlling the device at the same time. This makes desk
a backend for your own tools, for example a script that logs a LiPo charge and plots it.

desk never acts on a reading by itself. Your program decides (for example "current below 50 mA:
switch off") and sends the command. Limits and safety checks apply to that command as usual.

## From the command line

```sh
desk source watch psu                          # a table, one row per reading, until Ctrl-C
desk source watch psu --json                   # one JSON line per reading
desk source watch psu --interval 1s --count 60 # one reading per second, 60 of them
desk source watch psu --interval 0             # as fast as the device answers
```

Each `--json` line is an envelope, like every other desk command:

```json
{"ok":true,"data":{"device":"psu","time":"2026-11-05T20:56:22.396Z","elapsed":0.104,"reading":{"output":true,"voltage_set":4.2,"current_set":1.0,"voltage":3.91,"current":0.998,"power":3.9,"mode":"cc","protection":"none","temperature":31.5,"input_voltage":44.7,"charge":0.12,"energy":0.47},"dropped":0,"failed":0}}
```

| Field | Meaning |
|---|---|
| `time` | Wall-clock time of the reading (UTC, RFC 3339) |
| `elapsed` | Seconds since this watch started (monotonic: use it for plots) |
| `reading` | The same fields as `desk source read` (`null` = the device doesn't report it) |
| `dropped` | Readings this watcher lost so far because it read too slowly (never hidden) |
| `failed` | Reads that failed so far and were retried (3 failures in a row end the stream; the errors are in the daemon's log) |
| `log` | Driver log lines since the last reading (only when there are any) |

The stream ends after `--count` readings, when you stop it, or with an error. An error is one
more line, `{"ok":false,"error":{"code":…,"message":…}}`, and the exit code is the error's. For
example `device_off` after the device's mains was switched off, `timeout` when the device stops
answering, `daemon_unavailable` when the daemon was stopped.

**Rate.** The interval is the shortest time between two readings (default 100 ms). A reading
takes as long as the device needs to answer: with `--interval 0` you get as many as the device
delivers. If you ask for more than the device can do, you get what it can do.

## The daemon

Only one program can hold a serial port. So while something watches, a background process,
`desk serve`, holds the device's session. It reads for every watcher and runs commands between
two readings.

- **It starts by itself** with the first `watch` and exits when nothing has used it for 30 s
  (`--idle`, or `DESK_DAEMON_IDLE`). You don't need to start it.
- **While it runs**, `desk power …` and `desk source …` go through it, so `desk source set psu
  4.2v` works during a watch. When it doesn't run, those commands work exactly as without it.
- **A mains cut through desk** (`desk power off psu`) ends the watch with `device_off` after
  closing the session cleanly.
- **Devices that lock their front panel while connected** (the FNIRSI IPS3608) stay locked while
  someone watches. The panel is free again when the last watcher stops.
- One daemon per config file and desk build. It inherits the environment of the `desk` that
  started it (for example `HA_TOKEN`), and its log is next to its socket (`desk-<id>.log`).
- Driver log lines go to that log and into the JSON results (`log`), not to your terminal.

## Talking to the socket directly

Programs that don't want to run `desk` as a subprocess can connect to the daemon's socket:

- Unix: `$XDG_RUNTIME_DIR/desk-operator/desk-<id>.sock` (macOS and Linux without
  `XDG_RUNTIME_DIR`: the cache folder; `$DESK_RUNTIME_DIR` overrides). Mode 0600: only your user.
- Windows: the named pipe `\\.\pipe\desk-operator-<id>`, local clients only.

The `<id>` depends on the config file and the desk build. The simplest way to get the daemon
running is to start any `desk source watch` once; for your own tooling, running `desk` as a
subprocess is the stable interface.

The protocol is JSON lines. A request carries the same arguments as the command line:

```json
{"args": ["source", "set", "psu", "4.2v", "1a", "--on"]}
```

The reply is the `--json` envelope plus `human` (the text desk would print) and `exit` (its exit
code). A connection can send several requests, one after the other. Only `desk power …` and
`desk source …` run through the daemon; anything else gets a `usage` error.

A `["source", "watch", …]` request turns the connection into a stream: one envelope per reading
as above, and the connection closes when the stream ends. Closing the connection stops watching.

## Example

[`examples/watch_to_csv.py`](../examples/watch_to_csv.py) logs a watch to a CSV file and can
switch the output off when a charge is done (current below a threshold while in CV).
