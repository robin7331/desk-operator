# FNIRSI power supplies (DPS-150 protocol family)

| Definition | Model | Maximum | Status |
|---|---|---|---|
| `fnirsi/ips3608` | IPS3608 | 36 V, 8 A | community, not yet tested on hardware |
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
| Output on / off | `F1 B1 DB 01 01 DD` / `F1 B1 DB 01 00 DC`, echoed by the device |
| Measurements | pushed by the device, about every 0.6 s: `F0 A1 C3 0C <V> <A> <W>` |

Sources: [hamsternz/fnirsi_psu](https://github.com/hamsternz/fnirsi_psu) (a working C tool for this
power supply), [DPS-150 protocol reference](https://philipp-iot.github.io/fnirsi_protocol/protocol/reference),
[FNIRSI software downloads](https://www.fnirsi.com/pages/software).

## Known gaps

- **USB vendor/product ids unknown:** the definitions have no `port` yet. Until then, bind by `path`.
- **Setpoints can't be read back** with the known commands, so desk reports voltage and current as
  *not confirmed*. The DPS-150's full status query (`0xFF`) may contain them, but its layout isn't
  verified (and may differ on the IPS3608).
- **Output state** is only known after desk switched it in the same command (from the echo).
- **Output state at power-up** is not documented. Check before connecting a load.

## Tests

`tests/device_fnirsi_dps150.rs` runs the driver against a fake device that speaks this protocol
over a pseudo-terminal (connect/disconnect, setpoints, output echo, pushed telemetry, garbage and
resynchronization, a silent device).
