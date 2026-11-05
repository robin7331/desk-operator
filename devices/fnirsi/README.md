# FNIRSI power supplies (DPS-150 protocol family)

| Definition | Model | Maximum | Status |
|---|---|---|---|
| `fnirsi/ips3608` | IPS3608 | 36 V, 8 A | **tested on hardware** (firmware V1.1, hardware V1.2) |
| `fnirsi/dps-150` | DPS-150 | 30 V, 5.1 A | community, not yet tested on hardware |

Both use one driver, [`shared/dps150_protocol.lua`](shared/dps150_protocol.lua): FNIRSI ships a single
PC application for "DPS-150 / IPS3608", and the IPS3608 frames match the documented DPS-150 ones.
They connect over USB as a CDC-ACM serial port (`/dev/ttyACM*` on Linux).

## Protocol

Binary frames `dir, type, cmd, len, data…, checksum` with `checksum = (cmd + len + Σdata) & 0xFF`,
`dir` = `0xF1` (to the device) or `0xF0` (from the device), float32 little-endian values.

| | Frame |
|---|---|
| Connect / disconnect | `F1 C1 00 01 01 02` / `F1 C1 00 01 00 01` (the front panel is locked while connected) |
| Set voltage / current | `F1 B1 C1 04 <float>` / `F1 B1 C2 04 <float>`, no reply |
| Output on / off | `F1 B1 DB 01 01 DD` / `F1 B1 DB 01 00 DC`; echoed by the DPS-150, **not** by the IPS3608 |
| Measurements | pushed by the device, about every 0.5–0.6 s: `F0 A1 C3 0C <V> <A> <W>` |
| Model name | query `F1 A1 DE 01 00 DF` → ASCII, e.g. `IPS3608` (`DF` firmware, `E0` hardware version) |
| Full status | query `F1 A1 FF 01 00 00` → 139 bytes (layout below) |

**IPS3608 full status** (offsets in bytes, float32 LE unless noted), found on hardware by diffing
snapshots with the output off, on, off:

| Offset | Field |
|---|---|
| 0 | input/bus voltage (48 V) |
| 4 / 8 | **voltage / current setpoint** |
| 12 / 16 / 20 | measured voltage / current / power |
| 24 | internal rail voltage (~30.3 V, also pushed as `C4`) |
| 28–75 | six presets (V, A pairs) |
| 76 / 80 / 84 | maximum voltage (36), current (8), power (150) |
| 99 / 103 | counters, also pushed as `D9` / `DA` (meaning unknown) |
| 107 | **output on/off** (byte, 1 = on) |

The IPS3608 also pushes `C4` (rail voltage) and `D9`/`DA` (counters) with every measurement cycle.

Sources: [hamsternz/fnirsi_psu](https://github.com/hamsternz/fnirsi_psu) (a working C tool for this
power supply), [DPS-150 protocol reference](https://philipp-iot.github.io/fnirsi_protocol/protocol/reference),
[FNIRSI software downloads](https://www.fnirsi.com/pages/software).

## Known gaps

- **USB id `2e3c:5740` is Artery's generic AT32 id**, not FNIRSI-specific: other gadgets share it.
  The IPS3608 definition checks the model name (`DE`) before sending anything; bind by
  `serial_number` in your own config.
- **DPS-150:** USB id, status layout and the name query are from documentation only, not verified
  on hardware. Without a status layout the driver can't read setpoints back (reported as *not
  confirmed*) and knows the output state only from the echo.
- **DPS-150 output state at power-up** is not documented. Check before connecting a load.

## Behavior observed on the IPS3608 (firmware V1.1)

- **After a mains cut it comes back with the output off** and keeps its setpoints.
- **Right after mains on**, the USB port appears within about a second, but the first writes fail
  ("broken pipe") until the device has booted (under 2 s). The driver retries for `boot_ms`.
- The output **ramps up** for several hundred ms after switching on (1.36 V of 3.3 V after 300 ms),
  so the definition waits `settle_ms = 1000` before measuring.
- It does **not echo** the output command; the output state comes from the full status.

## Tests

`tests/device_fnirsi_dps150.rs` runs the driver against a fake device that speaks this protocol
over a pseudo-terminal: connect/disconnect, setpoints, output echo (or none), pushed telemetry,
garbage and resynchronization, a silent device, the name check and the IPS3608 status layout.
