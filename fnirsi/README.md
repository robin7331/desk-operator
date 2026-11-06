# FNIRSI power supplies (DPS-150 protocol family)

| Definition | Model | Rating | Status |
|---|---|---|---|
| `fnirsi/ips3608` | IPS3608 | 36 V, 8 A, 285 W | **tested** (hardware V1.1, firmware V1.2) |
| `fnirsi/dps-150` | DPS-150 | 30 V, 5.1 A, 150 W | community (protocol verified on hardware by others, not with desk) |

Both use one driver, [`shared/dps150_protocol.lua`](shared/dps150_protocol.lua): FNIRSI ships a
single PC application for "DPS-150 / IPS3608", and both speak the same protocol. They connect over
USB as a CDC-ACM serial port (`/dev/ttyACM*` on Linux).

What desk reads from them: output state, setpoints, measured V/A/W, regulation mode (CV/CC),
protection state, temperature, input voltage, and the Ah/Wh counters.

## Protocol

Binary frames `dir, type, cmd, len, data…, checksum` with `checksum = (cmd + len + Σdata) & 0xFF`,
`dir` = `0xF1` (to the device) or `0xF0` (from the device), float32 little-endian values.

Types: `A1` query/response, `B1` write, `C1` session. **Never send type `C0`**: it switches the
device into its firmware-update mode (USB mass storage, id `2e3c:5720`, the serial port
disappears). Type `B0` sets a baud index with unknown effect. The driver sends neither.

| cmd | | Meaning | Data |
|---|---|---|---|
| `00` | `C1` | connect (1) / disconnect (0); the front panel is locked while connected | u8 |
| `C0` | query | input voltage | f32 V |
| `C1` / `C2` | write | voltage / current setpoint (no reply) | f32 |
| `C3` | pushed, query | measured V, A, W | 3 × f32 |
| `C4` | pushed, query | temperature | f32 °C |
| `C5`–`D0` | write | presets M1–M6 (V, A pairs; stored on the device) | f32 |
| `D1`–`D5` | write | OVP V / OCP A / OPP W / OTP °C / LVP V (input under-voltage) thresholds (stored) | f32 |
| `D6` / `D7` | write | brightness / volume (stored) | u8 |
| `D8` | write, query | metering (Ah/Wh counting) on/off | u8 |
| `D9` / `DA` | pushed | charge Ah / energy Wh | f32 |
| `DB` | write, query | output on/off. **`DB=1` also clears a latched protection** | u8 |
| `DC` | query | protection: 0 none, 1 OVP, 2 OCP, 3 OPP, 4 OTP, 5 LVP | u8 |
| `DD` | query | mode: 1 CV, 0 CC (DPS-150; **not** on the IPS3608, see below) | u8 |
| `DE` / `DF` / `E0` | query | model name / **hardware** version / **firmware** version (ASCII) | |
| `E1` | query | device address (1) | u8 |
| `E2` / `E3` | query | maximum settable V / A right now | f32 |
| `E4`–`E8` | query | ceilings of the protection settings | f32 |
| `FF` | query | full status, 139 bytes (layout below) | |

Queries are `F1 A1 <cmd> 01 00 <cmd+1>`. No command exists for the push rate, key lock, preset
recall, counter reset or a soft reset. **The devices don't validate written values** (a DPS-150
accepts 31 V or −1 V), so desk's ratings and limits are the only guard. A write is **dropped when
the next frame follows within a few ms**: the driver waits `gap_ms` (50) after every write.

**Full status** (`FF`), offsets in bytes, float32 LE unless noted (same on both models):

| Offset | Field | Offset | Field |
|---|---|---|---|
| 0 | input voltage (IPS3608: constant 48.0) | 96 / 97 / 98 | brightness / volume / metering (u8) |
| 4 / 8 | **voltage / current setpoint** | 99 / 103 | **charge Ah / energy Wh** |
| 12 / 16 / 20 | measured V / A / W | 107 | **output on** (u8) |
| 24 | **temperature °C** | 108 | **protection** (u8, codes as `DC`) |
| 28–75 | presets M1–M6 (V, A pairs) | 109 | **mode** (u8, 1 CV / 0 CC) |
| 76–92 | OVP / OCP / OPP / OTP / LVP thresholds | 110 | address (u8) |
| | | 111 / 115 | max V / A (`E2`/`E3`) |
| | | 119–135 | ceilings of the five protection settings |

Protections: OVP, OCP, OPP and OTP **latch** (output off until cleared); LVP clears by itself.
The host can only clear a latched trip with `DB=1`, which **switches the output on at the same
time**. desk therefore refuses `output on` while a protection is tripped, unless the user passes
`--clear-protection`.

Sources (best first): [mmgalushka/dps150](https://github.com/mmgalushka/dps150) (DPS-150
systematically probed on hardware), [Puyodead1/fnirsi-psu](https://github.com/Puyodead1/fnirsi-psu)
and [brandonjank/ips3608-control](https://github.com/brandonjank/ips3608-control) (decompiles of
the official app plus hardware tests; the latter on IPS3608s),
[cho45/fnirsi-dps-150](https://github.com/cho45/fnirsi-dps-150) (the original reverse
engineering), [hamsternz/fnirsi_psu](https://github.com/hamsternz/fnirsi_psu),
[Philipp-IoT/fnirsi_protocol](https://github.com/Philipp-IoT/fnirsi_protocol) (raw captures are
useful; several interpretations are wrong), [FNIRSI IPS3608](https://www.fnirsi.com/products/ips3608)
(ratings), [FNIRSI software](https://www.fnirsi.com/pages/software-downloads).

## Observed on the IPS3608 (hardware V1.1, firmware V1.2, Nov 2026)

Read-only probes with `desk port probe`, output switched only through desk, nothing connected:

- **Pushed** while connected, about every **550 ms**: `DA`, `D9`, `C3`, `C4`. Unlike the DPS-150,
  no `C0`, `E2`, `E3` pushes. The Ah/Wh counters are pushed with the output off, too.
- **Every documented query answers** (`DE` … `E8`, `C0`–`C4`, `DB`, `DC`, `DD`, `FF`). The full
  status replies within about 20 ms, also at 10 queries per second.
- **Mode: use status offset 109.** With the output on and no load, offset 109 reads 1 (CV) while
  query `DD` reads 0 in every state. CC (0) is from the sources; not yet seen on this unit (needs a
  load).
- **Input voltage: use query `C0`** (44.7–46.2 V, moves with load); status offset 0 stays 48.0.
- Protection thresholds as shipped: OVP 36 V, OCP 8 A, OPP 150 W, OTP 60 °C, LVP 40 V. Ceilings:
  36 V, 8.2 A, 288 W, 99 °C, 48 V. `E2` answers input − 0.2 V (44.5 V) while status offset 111
  says 36.0; desk uses the ratings either way.
- **After a mains cut it comes back with the output off** and keeps its setpoints.
- **Right after mains on**, the USB port appears within about a second, but the first writes fail
  ("broken pipe") until the device has booted (under 2 s). The driver retries for `boot_ms`.
- The output **ramps up** for several hundred ms after switching on (1.36 V of 3.3 V after 300 ms),
  so the definition waits `settle_ms = 1000` before measuring.
- It does **not echo** the output command; the output state comes from the full status.
- **Watching** (`desk watch --interval 0`): about **18 readings per second** (two queries per
  reading, full status and input voltage, ~25 ms each); 200 in a row without a failure, and the
  measurements change from one reading to the next.

## Known gaps

- **USB id `2e3c:5740` is Artery's generic AT32 id**, not FNIRSI-specific: other gadgets share it.
  Both definitions check the model name (`DE`) before sending anything; bind by `serial_number` in
  your own config.
- **Not exposed by desk yet:** protection thresholds, presets, metering on/off and counter reset
  (turning metering off and on resets it per the DPS-150 manual; unverified), brightness, volume.
  All of these write settings stored on the device.
- **DPS-150:** not yet run with desk on hardware. Its output state at power-up isn't documented:
  check before connecting a load.

## Tests

`harness/tests/fnirsi_dps150.rs` runs the driver against a fake device that speaks this protocol
over a pseudo-terminal: connect/disconnect, setpoints, output echo (or none), pushed telemetry,
garbage and resynchronization, a silent device, the name check, the IPS3608 status layout with
every field, the quiet gap after writes, and a latched protection.
