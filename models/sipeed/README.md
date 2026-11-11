# Sipeed

| Model | Name | Status |
|---|---|---|
| `sipeed/slogic32u3` | SLogic32U3 | community: captures in every channel mode, rates, formats, triggers and timeouts, and the live stream in every channel mode, tried on firmware 202610300413 with nothing connected; a real signal not checked |

# SLogic32U3 logic analyzer

32 channels (D0 to D31) over USB 3.2 Gen 2 (10 Gbps), USB id `359f:3032`, powered from USB.

## How desk reaches it

Through sigrok, not a serial port: desk runs `sigrok-cli` for each capture
([`docs/sigrok.md`](../../docs/sigrok.md)). **Stock sigrok has no driver for it.** Sipeed ships
its own sigrok build with the driver `sipeed-slogic-analyzer`, as `sigrok-cli-SLogic-<version>`
for Linux (AppImage), Windows and macOS on the
[releases page of sipeed/SLogic](https://github.com/sipeed/SLogic/releases). Download it and name
it as a backend in your `desk.toml`:

```toml
[providers.sipeed]
kind    = "sigrok"
program = "/path/to/sigrok-cli-SLogic-linux-x86_64.AppImage"

[devices.la]
model   = "sipeed/slogic32u3"
capture = { sigrok = { backend = "sipeed" } }   # with several SLogic analyzers: conn = "359f.3032"
```

```sh
desk do la capture duration=100ms pattern=emulation      # no signal needed: the analyzer's test pattern
desk do la capture duration=2s samplerate=100MHz channels=8 trigger=D0=f
desk stream la samples channels=8 samplerate=100MHz | my-analyzer      # live, until stopped
```

- **Linux:** the analyzer needs a udev rule for your user, see
  [Sipeed's README](https://github.com/sipeed/SLogic#linux). Without it sigrok finds no device.
- **Firmware:** Sipeed's software 2.3.2 and later needs firmware 202610150528 or newer. The
  firmware version is the `S/N` in the device's name (`desk provider endpoints <backend>`). It is
  updated in a browser, see Sipeed's
  [Software Guide](https://wiki.sipeed.com/hardware/en/logic_analyzer/slogic32u3/Software_User_Guide.html).

Sources: [sipeed/SLogic](https://github.com/sipeed/SLogic) (README, release notes,
`slogic/slogic32u3.c` for the rate tables, `build/docs/regression-checklist-slogic32u3.md` and
`sigrok-cli-slogic-plugin/` for how Sipeed drives its sigrok-cli), the wiki's
[Hardware Guide](https://wiki.sipeed.com/hardware/en/logic_analyzer/slogic32u3/Hardware_Specification.html),
and the driver's own description (`desk sigrok show sipeed-slogic-analyzer --backend <backend>`).

## What the driver sends

One `sigrok-cli` run per capture. The arguments of `capture` become sigrok config keys:

| Argument | sigrok | |
|---|---|---|
| `channels` (4, 8, 16, 32) | `logic_channels=<n>` | The analyzer's channel modes: D0 up to D3, D7, D15 or D31. It must come before `samplerate`, which desk's sorted keys give |
| `samplerate` | `samplerate=<Hz>` | 5, 8, 10, 16, 20, 25, 32, 40, 50, 80, 100, 160, 200, 400, 800 or 1400 MHz |
| `threshold` | `voltage_threshold=<V>-<V>` | One switching point for all inputs, 0.0 to 6.0 V in 0.1 V steps. The vendor driver starts at 1.7 V; Sipeed suggests half the logic level |
| `pattern` | `pattern=Normal` or `Emulation` | `Emulation` is a test pattern from the analyzer itself |
| `duration` | `--time <ms>` | Whole milliseconds, rounded up |
| `trigger` | `--triggers <t> --wait-trigger` | `0`, `1`, `r`, `f`, `e` per channel |
| `format` | `-O srzip`, `binary`, `csv`, `vcd` | |

The highest rate depends on the channel mode, on a 10 Gbps link: 1400 MHz with 4 channels, 800 MHz
with 8, 400 MHz with 16, 200 MHz with 32.

## The stream

`desk stream la samples` ([`docs/streaming.md`](../../docs/streaming.md)) runs one `sigrok-cli
--continuous -O binary` and passes its stdout on, until the last subscriber leaves. Its arguments
are the capture's `samplerate`, `channels`, `threshold` and `pattern`, with the same defaults and
the same checks, and the same sigrok config keys; there is no `duration`, `format` or `trigger`.

| `channels` | Header `channels` | `unit_size` (bytes per sample) |
|---|---|---|
| 4 | D0 to D3 | 1 (the upper four bits are 0) |
| 8 | D0 to D7 | 1 |
| 16 | D0 to D15 | 2 |
| 32 | D0 to D31 | 4 |

The unit sizes are measured: bytes per second over the sample rate, at 10 MHz in each mode. The
bit order (D0 in bit 0) is how sigrok numbers a driver's channels, and what the test pattern
agrees with (*Observed behavior*); which physical input lands in which bit has not been checked
with a real signal.

## Quirks of the vendor driver

Observed with Sipeed's sigrok-cli 2.3.3 (sigrok-cli 0.8.0, libsigrok 0.6.0). It rarely refuses a
request: it warns on stderr, captures something else and exits with 0. The driver script therefore
checks everything before sigrok runs.

- A sample rate it doesn't have (12 MHz) becomes **the highest rate of the channel mode**
  ("Reach limit or not supported, wrap to 800MHz").
- A rate above the mode's limit is cut down to the limit.
- `logic_channels` other than 4, 8, 16 or 32 becomes 32.
- A threshold between two steps (`1.75-1.75`) is accepted without a message.
- `--channels D0-D3` fails with "Invalid channel 'D0'", with or without `logic_channels`;
  `--channels D0,D2` works. A channel outside the mode (`D9` with 4 channels) is accepted. The model
  only offers the channel modes.
- A trigger on a channel outside the mode fails properly: "Invalid channel 'D9'".
- With a trigger, sigrok-cli prints "Device only sent 900000 samples" for a capture of 1000000,
  but the file holds all of them. Sipeed's regression checklist puts the trigger at 10 % of the
  capture (the driver's `captureratio`, 10 by default).
- A trigger that never comes makes sigrok-cli wait forever. desk stops it 30 s after the capture
  should have ended.
- An srzip file always lists 32 channels; those outside the channel mode are named by their number.

## Known gaps

- **Slower USB links.** The rate limits above are for a 10 Gbps link. On a 5 Gbps port they are
  halved (4 channels at 800 MHz, 8 at 400, 16 at 200, 32 at 100, says Sipeed's README), and the
  vendor driver then cuts a higher rate down with only a warning: desk reports the rate that was
  asked for. An srzip file carries the real one. The link is in the device's name: `(10G)`.
- A real signal hasn't been captured yet: everything ran on the test pattern or on open inputs,
  so thresholds and triggers on real edges are unchecked.
- Not offered: single channels or other subsets, the position of the trigger (`captureratio`),
  a length in samples, the `USB connection test` pattern (a benchmark of the link), and the
  optional ADC module.
- **The stream with 4 channels above 400 MHz** is more than sigrok-cli converts: it unpacks the
  4-channel samples into bytes at about 600 MB/s, and buffers the rest in its memory, without a
  bound (about 100 MB more each second at 800 MHz, 370 MB at 1400 MHz). The samples come late, not
  lost, until memory runs out. The model allows these rates as the capture does; stream them only
  for a few seconds.
- With the stream at its fastest (800 MB/s), a second subscriber reading at full speed held
  sigrok-cli to about 600 MB/s on the machine below, and it buffered the rest the same way.
- SLogic16U3 (`359f:3031`) and SLogic Combo 8 (`359f:0300`) use the same sigrok driver and would
  need only a model with their rate tables for `../shared/slogic.lua`. Nobody has tried them.

## Observed behavior

Firmware 202610300413, Sipeed's sigrok-cli 2.3.3 as a Linux AppImage, 10 Gbps link, November 2026,
nothing connected to the inputs.

- **Sample counts are exact:** `binary` holds one byte per sample with 4 or 8 channels and four
  with 32, and exactly `duration × samplerate` samples, up to 4 channels at 1400 MHz.
- **Open inputs read 0** on all 32 channels (threshold 1.7 V).
- **The test pattern** counts down on D0 to D3, one step per sample, at every rate.
- **A short capture takes 0.1 to 0.4 s** in all, starting the AppImage included.
- **Writing the file takes longer than capturing** at high rates, since sigrok-cli converts
  slower than samples arrive. Measured on a desktop PC with the test pattern, in channels ×
  samples per second after the capture itself: srzip 80 to 300 million, binary over 1500 million,
  vcd 11 to 44 million, csv 2 million. 32 channels at 200 MHz for 0.5 s took 23 s as srzip
  (213 MB), and 32 channels at 10 MHz for 0.1 s took 15 s as csv (64 MB). The driver's timeout
  allows ten times these.
- **After a timeout** the analyzer was free again right away, and the next capture worked.
- **The stream** (`desk stream la samples --discard`), 3 s each counted from desk's start, and
  Sipeed's sigrok-cli alone into `wc -c` (`timeout 3`, its start included), in MB/s:

  | Channels | Rate | Expected | desk | sigrok-cli alone |
  |---|---|---|---|---|
  | 32 | 10 MHz | 40 | 38.6 | |
  | 32 | 50 MHz | 200 | 193 | |
  | 32 | 100 MHz | 400 | 386 | |
  | 32 | 200 MHz | 800 | 772 | 771 |
  | 16 | 400 MHz | 800 | 725 | 772 |
  | 8 | 800 MHz | 800 | 731 | 772 |
  | 4 | 1400 MHz | 1400 | 557 | 625 |

  Over 5 s desk and sigrok-cli alone delivered the same, 3.90 to 3.92 GB, except with 4 channels
  at 1400 MHz (2.95 and 3.13 GB, see *Known gaps*). No subscriber dropped a block. The test
  pattern streamed at 32 channels and 200 MHz had no gap: in every block a checker read (about
  400 of 2000; it was too slow for the rest), the counter matched the block's offset.
- **Latency:** sigrok-cli hands over its samples about every 10 ms (85 to 95 blocks a second;
  95 % within 16 ms), in blocks of 64 KiB with 8 channels at 5 MHz, 256 KiB with 32. A program
  reacting to the stream sees an edge at the earliest 10 to 20 ms after it happened.
- **The test pattern** in the stream is a binary counter of the sample number with its lowest
  three bits inverted (7, 6, … 0, 15, 14, … 8, 23, …) on all channels up to 16; with 32 channels
  D16 to D31 carry its low byte with each bit as a pair (0 as `01`, 1 as `10`). It starts at 7 with
  the first sample. D0 toggles at every sample, D1 at every second, and so on.
- **Open inputs** read 0 in the stream too (32 channels, 10 million samples).
- Firmware 202609240213 (the first public one) was found and described by sigrok-cli 2.3.3 like
  the newer one. No capture was tried with it.

## Tests

`tests/model_sipeed_slogic.rs` runs the driver against the fake `sigrok-cli`: the exact command
lines of captures and streams, each channel mode's highest rate and its stream header, and that
requests the vendor driver would silently change never reach sigrok.
