# Streaming readings (`desk watch`), binary streams (`desk stream`) and the daemon

`desk watch <device>` streams readings from any device until you stop it. Other
programs and `desk` commands can keep controlling the device at the same time. This makes desk
a backend for your own tools, for example a script that logs a LiPo charge and plots it.

desk never acts on a reading by itself. Your program decides (for example "current below 50 mA:
switch off") and sends the command. Limits and safety checks apply to that command as usual.

## From the command line

```sh
desk watch psu                                 # a table, one row per reading, until Ctrl-C
desk watch psu --json                          # one JSON line per reading
desk watch psu measured_voltage measured_current --interval 1s --count 60
desk watch psu --interval 0                    # as fast as the device answers
desk watch iron mode --changes                 # a line only when the mode changes
desk watch psu:2                               # one channel of a multi-channel device
```

Each `--json` line is an envelope, like every other desk command:

```json
{"ok":true,"data":{"device":"psu","time":"2026-11-05T20:56:22.396Z","elapsed":0.104,"values":{"voltage":4.2,"current":1.0,"output":true,"measured_voltage":3.91,"measured_current":0.998,"measured_power":3.9,"mode":"cc","protection":"none","temperature":31.5,"input_voltage":44.7,"charge":0.12,"energy":0.47},"dropped":0,"failed":0}}
```

| Field | Meaning |
|---|---|
| `time` | Wall-clock time of the reading (UTC, RFC 3339) |
| `elapsed` | Seconds since this watch started (monotonic: use it for plots) |
| `values` | The same names as `desk get` (only the named ones if you named some; absent = not reported) |
| `channels` | Per channel, for multi-channel devices (unless you watch one channel: `psu:2`) |
| `channel` | The channel, when you watch one |
| `dropped` | Readings this watcher lost so far because it read too slowly (never hidden) |
| `failed` | Reads that failed so far and were retried (3 failures in a row end the stream; the errors are in the daemon's log) |
| `log` | Driver log lines since the last reading (only when there are any) |

The stream ends after `--count` readings, when you stop it, or with an error. An error is one
more line, `{"ok":false,"error":{"code":…,"message":…}}`, and the exit code is the error's. For
example `device_off` after the device's mains was switched off, `timeout` when the device stops
answering, `daemon_unavailable` when the daemon was stopped.

**`--changes`** prints a reading only when one of the watched values changed: a number by more
than the tolerance its model declares, or without one by more than 0.5 % of its size (so
measurement noise doesn't count, whether the reading is in volts or in nanofarads), anything else
at all. Without it, there's one line per interval, evenly spaced for plots. Either way the device
is read in full; names only filter what you see.

**Rate.** The interval is the shortest time between two readings (default 100 ms). A reading
takes as long as the device needs to answer: with `--interval 0` you get as many as the device
delivers. If you ask for more than the device can do, you get what it can do.

## The daemon

Only one program can hold a serial port. So while something watches, a background process,
`desk serve`, holds the device's session. It reads for every watcher and runs commands between
two readings.

- **It starts by itself** with the first `watch` and exits when nothing has used it for 30 s
  (`--idle`, or `DESK_DAEMON_IDLE`). You don't need to start it.
- **While it runs**, `desk power`, `get`, `set` and `do` go through it, so `desk set psu voltage
  4.2v` works during a watch. When it doesn't run, those commands work exactly as without it.
- **One command per device at a time.** Commands for the same device wait for each other, so a
  second `desk do la capture …` waits until the first capture is done instead of meeting a busy
  device. That holds for serial ports and sigrok devices alike. Without the daemon, a second
  process gets `endpoint_unavailable` (port busy, or sigrok's "Unable to claim USB interface").
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
running is to start any `desk watch` once; for your own tooling, running `desk` as a
subprocess is the stable interface.

The protocol is JSON lines. A request carries the same arguments as the command line:

```json
{"args": ["set", "psu", "voltage", "4.2v", "current", "1a", "output", "on"]}
```

The reply is the `--json` envelope plus `human` (the text desk would print) and `exit` (its exit
code). A connection can send several requests, one after the other. Only `power`, `get`, `set`,
`do`, `watch` and `stream` run through the daemon; anything else gets a `usage` error.

Keep the connection open until the reply has arrived. Closing it (or only your sending side)
cancels a capture (an action that produces a file) that runs for you: it is stopped and its
partial file removed. Other commands run to their end. `--output` in a request must be an absolute
path.

A `["watch", …]` request turns the connection into a stream: one envelope per reading
as above, and the connection closes when the stream ends. Closing the connection stops watching.

## Example

[`examples/watch_to_csv.py`](../examples/watch_to_csv.py) logs a watch to a CSV file and can
switch the output off when a charge is done (current below a threshold while in CV).

## Binary streams (`desk stream`)

`desk watch` is for readings: a few values, a few times a second. A logic analyzer delivers
millions of samples a second, and a protocol analyzer, a viewer or a recorder wants all of them,
live. That is a **stream**: a device's raw data, at full bandwidth, while desk keeps owning the
device. A model declares its streams (`docs/model-format.md`); the core never looks inside the
bytes.

Use a capture (`desk do la capture duration=2s`) for a bounded window into a file; use a stream
when another program reacts while the data comes in, or when it should run until you stop it.

### From the command line

```sh
desk stream la                                   # what the device streams (a listing, exit 2)
desk stream la samples > samples.bin             # the payload until Ctrl-C
desk stream la samples samplerate=10MHz channels=D0,D1 | my-analyzer
desk stream la samples --header > samples.bin    # the header line first
desk stream la samples --frames | my-program     # frames with seq, offset and drops (below)
desk stream la samples --discard --duration 5s   # read without writing: throughput
desk stream la samples --bytes 100MB > x.bin     # stop after 100 MB (whole samples)
```

- The payload never goes to a terminal: redirect stdout or use `--discard`.
- `--duration` and `--bytes` end it from your side; so does Ctrl-C and a reader of stdout that
  goes away. All of them exit with 0. An error (the source failed, it stalled, the daemon
  stopped) exits with the error's code.
- `--json`: with `--discard`, the header envelope (with `--header`) and the final summary or
  error envelope go to stdout as JSON lines. Without `--discard` stdout carries the payload, so
  the final summary or error envelope goes to stderr. Without `--json` an error is
  `error: …` on stderr, and `--discard` prints a summary line.
- The summary: `bytes`, `units`, `blocks`, `dropped` (blocks this client lost), `first_offset`,
  `seconds`, `mb_per_s`, `ended` (`source`, `duration`, `bytes`, `interrupted`,
  `output_closed`) and `end` (what the end frame said).

### The header

The first thing on the connection (and with `--header`/`--frames` on stdout) is one JSON line,
the usual envelope:

```json
{"ok":true,"data":{"device":"la","capability":"capture","stream":"samples","content":"logic","unit_size":1,"samplerate":10000000.0,"channels":["D0","D1","D2","D3","D4","D5","D6","D7"],"meta":{"driver":"demo"},"args":{"samplerate":10000000.0,"channels":"D0,D1,D2,D3,D4,D5,D6,D7"},"time":"2026-11-11T15:41:16.234Z","framing":"desk-stream-1","block_max":1048576}}
```

| Field | Meaning |
|---|---|
| `content` | What the bytes are. `logic`: samples of logic channels. Any other name is the model's own; its bytes are opaque to desk |
| `unit_size` | Bytes per unit. For `logic` a unit is a sample, little-endian, with `channels[0]` in bit 0 |
| `samplerate`, `channels` | `logic` only: samples per second, and the channel names in bit order |
| `meta` | Everything else the driver said about its data |
| `args` | The stream's arguments, defaults filled in |
| `time` | When the stream started (UTC, RFC 3339). A joiner gets the same header with the same time |
| `framing`, `block_max` | The frame format (`desk-stream-1`) and the largest payload (1 MiB) |
| `channel` | The device channel, when you stream one (`la:2`) |

### Frames

After the header line come binary frames: a fixed 32-byte little-endian header, then `length`
payload bytes.

| Bytes | Field | Type | Meaning |
|---|---|---|---|
| 0..4 | `magic` | u32 | `0x4B534544`: the bytes `D E S K` |
| 4 | `kind` | u8 | 0 data, 1 end |
| 5 | `flags` | u8 | 0 |
| 6..8 | `reserved` | u16 | 0 |
| 8..12 | `length` | u32 | payload bytes that follow |
| 12..20 | `seq` | u64 | the block's number, from 0 |
| 20..28 | `offset` | u64 | its first unit since the start of the stream (samples for `logic`, bytes for `unit_size` 1) |
| 28..32 | `dropped` | u32 | blocks this subscriber lost so far |

A data block holds whole units, at most 1 MiB. It goes out when it is full or 5 ms after its
first byte, so a slow source stays live. The last frame is an **end frame**: its `seq` and
`offset` are the next ones, and its payload is a JSON envelope: `{"ok":true,"data":{"ended":
"source","bytes":…,"units":…,"blocks":…}}`, or `{"ok":false,"error":{…},"exit":…}`. Then the
daemon closes the connection.

### Drops, offsets and joining

- **A slow subscriber never slows the device or the others.** Each one has its own queue
  (64 MiB). When it is full, the oldest blocks go. The next block says how many were lost
  (`dropped`), and `seq` and `offset` jump: the units between the expected and the received
  `offset` are missing. A fast subscriber at the same time loses nothing.
- **Several programs share one stream.** The first subscriber's arguments start the source. A
  subscriber with the same arguments joins: it gets the same header and the blocks from now on,
  so its first `offset` says where it came in. Different arguments are refused
  (`endpoint_unavailable`, naming the running ones), and so is another stream of the same device.
- **The stream holds the device.** Meanwhile `get`, `set`, `do` and `watch` for that device fail at
  once with `endpoint_unavailable` ("busy: it is streaming"); other devices are not affected. A
  stream doesn't start while the device is watched.

### Endings

| How | Subscribers see |
|---|---|
| The source ended (a bounded acquisition, a finite source) | the remaining blocks, then an end frame with `ok: true` |
| The source failed | the blocks before the failure, then an end frame with the error |
| The source sent nothing for 10 s | an end frame with `timeout` |
| `desk power off` of the device | an end frame with `device_off` |
| The daemon stopped | an end frame with `daemon_unavailable` (queued blocks are dropped first) |
| A subscriber closed its connection | nothing: it left. The last one leaving stops the source (a `sigrok-cli` is killed) and frees the device |

A driver error before the data is described is the reply itself: one error envelope instead of
the header, then the connection behaves like any failed command.

### On the socket

A request `{"args":["stream","la","samples","samplerate=10MHz"]}` gets the header line (or an
error envelope), then the frames. Your program leaves by closing the connection. `desk stream
--frames` passes exactly this after the header line, so running it as a subprocess gives you the
same without finding the socket.

### Throughput

Measured on one machine (Intel Core i7-8700K, 6 cores, Linux, a release build, other work running
at the same time), source → daemon → socket → `desk stream`. Each number is the payload a
subscriber received in 3 s, the best of three runs:

| Source | How | MB/s |
|---|---|---|
| Mock source, as fast as it can | `desk stream … --discard --duration 3s` | 3070 |
| The same, two subscribers at once | two of those at the same time | 1930 and 1810 (3740 together) |
| The same, payload written | `desk stream … --duration 3s > /dev/null` | 1890 |
| Fake `sigrok-cli --continuous`, as fast as it writes | through a pipe, `--discard` | 2060 |
| Real `sigrok-cli -d demo`, 8 channels, 100 MHz | `--discard` | 97 (sigrok alone into a pipe: 102) |
| The same, 500 MHz | `--discard` | 481 (alone: 502) |
| The same, 1 GHz (the most the demo driver accepts) | `--discard` | 629 (alone: 657) |
| A USB 3 logic analyzer through its vendor's `sigrok-cli`, 32 channels at 200 MHz (800 MB/s) | `--discard --duration 5s` | 783 (alone, 5 s with its start: 783) |

The fast sources outrun one subscriber, which then loses blocks (that's the rule: the source
never waits); a source at a fixed rate below that (`rate=500000000` on the mock: 499 MB/s, the
sigrok rows) loses nothing. "Alone" is `sigrok-cli` writing into a pipe that a small program
reads, counted from the first byte; desk's numbers include sigrok's start (about 0.1 s of the 3 s).
The numbers varied by up to a factor of three between runs, with the machine's other load.

desk reads sigrok's stdout with blocking reads on a thread of its own. Reading it through the
async runtime, woken after each pipe-full, held that analyzer to about 690 MB/s: sigrok-cli waited
on the full pipe and buffered the rest (its memory grew by about 100 MB a second).

desk passes on everything sigrok's `demo` driver delivers, up to its maximum, and moves about
2 GB/s per subscriber from a synthetic source. A shared-memory transport is not needed for these
rates.

### Example

[`examples/stream_edges.rs`](../examples/stream_edges.rs) runs `desk stream … --frames` as a
subprocess, checks `seq` and `offset`, counts the edges per channel of a `logic` stream and prints
a line per second. It is a starting point for your own analyzer:

```sh
cargo run --release --example stream_edges -- --config examples/sigrok/desk.toml la samples samplerate=10MHz
```
