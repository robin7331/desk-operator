# OWON

| Model | Name | Status |
|---|---|---|
| `owon/oel1515` | OEL1515 | community: functions, setpoints, input and mains behavior tried on firmware V1.4.0 with nothing connected; measurements on a real source not checked |
| `owon/xdm1041` | XDM1041 | community: functions, ranges, rate and mains behavior tried on firmware V4.3.0 with open leads; accuracy on a real signal not checked |

# XDM1041 multimeter

## Protocol

SCPI text over USB, through a USB serial adapter in the meter.
115200 8N1 by default, changeable in the meter's Utility menu (the `baud` param). Commands end with
`\n`, replies with `\r\n`. Sources: OWON's
[programming manual](https://static.eleshop.nl/mage/media/downloads/XDM1000_Digital_Multimeter_Programming_Manual.pdf),
TheHWcave's [tested command list](https://github.com/TheHWcave/OWON-XDM1041), and the working tools
[rusty_meter](https://github.com/markusdd/rusty_meter),
[owon-xdm1041_py-util](https://github.com/icrnjavic/owon-xdm1041_py-util) and
[owon-xdm-remote](https://github.com/Elektroarzt/owon-xdm-remote).

| Command | Meaning |
|---|---|
| `*IDN?` | `OWON,XDM1041,<serial>,V4.3.0,3` |
| `FUNC?` | `"VOLT"`, `"VOLT AC"`, `"CURR"`, `"CURR AC"`, `"RES"`, `"CAP"`, `"FREQ"`, `"PER"`, `"DIOD"`, `"CONT"`, `"TEMP"` |
| `MEAS1?` | main display as a number, no unit |
| `MEAS1:SHOW?` | main display as shown, with its unit |
| `AUTO?` / `AUTO` | auto range: 1 on, 0 off / switch it on |
| `RANGE?` / `RANGE <n>` | range as displayed, e.g. `5 V`, `50 MΩ` (sensor type in temperature mode: `Pt100`) / select by number, in the order of the driver's tables |
| `RATE?` / `RATE <S\|M\|F>` | measuring speed |
| `CONF:VOLT:DC AUTO`, `CONF:VOLT:AC AUTO`, `CONF:CURR:DC AUTO`, `CONF:CURR:AC AUTO`, `CONF:RES AUTO`, `CONF:CAP AUTO`, `CONF:FREQ`, `CONF:PER`, `CONF:DIOD`, `CONF:CONT`, `CONF:TEMP:RTD <KITS90\|PT100>` | select a function |

**Never send undocumented commands.** `INT:START`, guessed by a forum user on a rebadged XDM1041,
[started the factory calibration and left wrong calibration factors](https://www.eevblog.com/forum/testgear/owon-xdm1041-the-unknown-multimeter/msg6278436/).
The firmware also knows words like `SOFTWARERESET`, `DOWNLOAD`, `UARTcontrol`, `RESTart` and
`INTernal`, whose effects aren't documented.

The driver doesn't send `SYST:REM`, which would lock the front panel: all commands also work in
local mode. It identifies the model with `*IDN?` first, since a CH340 USB id says nothing about
what's attached.

## Quirks from the sources

- Units in formatted replies aren't UTF-8 on older firmware: `A6 B8` Ω, `AA CC` µ, `A1 E6` °C,
  `A8 48` °F. V4.3.0 sends Ω as UTF-8.
- `RANGE?` isn't answered in continuity and diode mode; the driver also doesn't ask in frequency
  and period mode.
- Firmware below V4.3.0 swaps diode and continuity in the reply to `FUNC?`. The driver corrects that.
- Firmware 20210525 can answer every command with `OK\nOK\nOK\n`. Replies can get lost or doubled;
  the driver retries queries and checks every reply.
- With a K-type sensor (`KITS90`) the meter stalls about 3 s every 10 minutes and queues its replies.
- **A current function on the wrong input socket reads 0 while current flows.** A 0 A reading doesn't
  prove there's no current.
- `TEMP:RTD:SHOW MEAS` freezes the temperature returned by `MEAS1?`; the driver doesn't send it.
- Auto range in capacitance is slow up to about 500 µF and fails above that: use a manual range.
- Settings aren't kept across power cycles: the meter starts in slow rate.

## Known gaps

Accuracy against a real signal hasn't been checked: everything so far ran with open leads. Not
implemented yet: the secondary display (frequency in AC modes, wrongly scaled by the firmware
according to TheHWcave), relative mode and statistics (`CALC:`), the beeper (the sources disagree on
the query) and continuity threshold, choosing the temperature unit, and `TEMP:RTD:SHOW`.

## Observed behavior

Firmware V4.3.0, November 2026, with open leads.

- **Standby:** in standby the meter stays dark until its front power button is pressed. It ignored
  `*IDN?` and `MEAS1?` there (115200 and 9600 baud) and didn't wake up, so no start over USB is known.
  V2.1.2's application image has no serial command for sleep or wake and never calls its standby
  routines, but the bootloader isn't in the update image and V4.3.0 may differ.
  [EEVblog posts](https://www.eevblog.com/forum/testgear/owon-xdm1041-the-unknown-multimeter/200/)
  say a separate ATtiny202 handles the power switch.
- **Mains loss:** the meter survives a cut of under a second and keeps its settings, and stops answering
  about 0.75 to 1 s after mains goes off. It appears to return to the state it had when mains was cut:
  after a cut of 15 s or 30 s while running it started by itself when mains returned, with settings at
  their defaults, and after being turned off with its button and a 30 s cut it stayed dark. So switching
  mains can start it remotely as long as it wasn't turned off with the button. The manual doesn't cover
  this. Boot time wasn't measured, so a command right after mains on can fail until it has booted.
- **USB:** a CH340 (`1a86:7523`) with no serial number. It stayed connected while mains was off for a
  few seconds, so it's probably powered by the USB cable and says nothing about the meter's state.
- **Stray bytes:** the first reply after booting starts with `00 01 00`. In standby, one query after
  mains came back got `00 01 00 00`. What they mean is unknown.
- **False values after changing function, at slow rate.** `FUNC?` changes at once, the reading doesn't:
  - to resistance: `0`, then a stale `86.8` for about 2 s, then `1E+9`, about 2.25 s in total
  - to DC voltage from resistance: `0`, then `1E+9` for about 2 s, valid after about 2.75 s
  - to frequency or period: `0`, then a 50 Hz reading (mains pickup) until about 3.5 s, then `0`

  The driver waits `settle_ms` (4 s) after changing function or range.
- **`1E+9`:** seen in resistance, diode, continuity and Pt100 with open leads (`MEAS1:SHOW?` then says
  `overload`), and in DC voltage during settling. The driver reports it as `overload` in every function.
- **Slow rate** gives about one new reading per second. `RATE S` works, and `RATE?` answers `S`.
- **Open leads:** frequency and period settle at 0, AC voltage reads about 70 mV, and K-type temperature
  shows a value (about 12 °C) that means nothing without a sensor.

# OEL1515 DC load

150 V, 15 A, 150 W; constant resistance 0.05 to 7500 Ω. The `T` version adds battery, OCP and OPP
test functions, with the same ratings.

## Protocol

SCPI text over USB through a CH340 USB serial adapter (`1a86:7523`, no serial number), or RS232 on the
DB9 connector. Commands end with `\n`, replies with `\n`. Commands get no reply. Sources: OWON's
[programming manual and user manual](https://www.owon.com.hk/download/index.asp?S=95) (the
series' download page; `files.owon.com.cn` doesn't send its intermediate certificate, so some
clients refuse it), and notes and working code from
[BenchMesh](https://github.com/BenchMesh/benchmesh/blob/main/ai_context/migration/device-notes/owon_oel.md)
(captures of an OEL1515), [dmmpy](https://github.com/olegv142/dmmpy) (tested with an OEL1515),
[owon-oel-battery-test](https://github.com/al-s/owon-oel-battery-test) and
[owon-dc-load](https://github.com/curtisgalloway/public-skills/blob/main/plugins/hardware-lab/skills/owon-dc-load/SKILL.md)
(an OEL1520).

| Command | Meaning |
|---|---|
| `*IDN?` | `OWON,OEL1515,<serial>,FV:V1.1.0`. Also answers in local mode |
| `SYST:REM`, `SYST:LOC` | remote mode, which locks the front panel / back to local |
| `MEAS:ALL:INFO?` | `<V>,<A>,<W>,<OVP>,<OCP>,<OPP>`, faults as `ON` or `OFF` |
| `FUNC?` / `FUNC <CURR\|VOLT\|RES\|POW\|DYN>` | `current`, `voltage`, `resistance`, `power`, `dynamic` |
| `INP?` / `INP <ON\|OFF>` | input |
| `INP:SHOR?` | input acts as a short circuit |
| `CURR?`, `VOLT?`, `RES?`, `POW?` / `CURR <A>`, `VOLT <V>`, `RES <Ω>`, `POW <W>` | setpoint of each function |

## Quirks from the sources

- **Local mode:** the load starts in local mode, where every query but `*IDN?` gets an empty line. In
  remote mode only SHIFT + On/Off on the panel works, and a long press returns to local. So the driver
  goes to remote at the start of every session and back to local at its end: the panel is locked
  during a command and for the whole of a `desk watch`.
- **Setpoints of another function** are dropped without any reply (seen for `VOLT`, `RES`, `POW`). The
  driver refuses them, and doesn't change the function by itself: that would change what the load does
  to whatever is connected.
- **Replies differ from the manual:** `INP?` and `INP:SHOR?` answer `ON`/`OFF` on real units, not
  `0`/`1`. The driver accepts both.
- **Baud rate:** the user manual says 9600 8N1 and the menu offers 1200 to 115200, but a load on
  firmware V1.4.0 answered at 115200 and not at 9600, as do the tools above. The driver's default is
  115200 (the `baud` param).
- **The sixth field of `MEAS:ALL:INFO?`** is OPP in the manual, while BenchMesh's first driver called it
  OTP. Over-temperature may not be reported at all.
- **Protections** turn the input off, and can only be cleared on the front panel: there's no command.
- Unknown queries answer `ERR`. `*RST` restores defaults, so the driver doesn't send it.

## Observed behavior

Firmware V1.4.0, November 2026, with nothing connected to the input.

- **Rear RS232:** works through the rear DB9 with a Prolific PL2303 USB-to-RS232 adapter, at 115200
  baud, first byte after about 20 ms. With such an adapter, set `usb_vid` and `usb_pid` in your own
  `port`: the model's ids are those of the USB port. The adapter stays connected while the load has no
  mains, so a port that's there says nothing about the load.
- **Remote mode:** the front panel works again after a session. The input stays on when the driver
  returns to local mode.
- **Settings:** all four functions took their setpoints and read them back.
- **Mains loss:** the load keeps answering for about 2.5 s after mains goes off. With its rear power
  button left on, it starts by itself when mains returns. Function and setpoints survived a cut of 15 s,
  and an input that was on came back **off**.
- **Boot:** about 2.7 s after mains returns, the load sends a boot log as text lines
  (`main.c(1016): CPU,CLK=240000000Hz.` and so on), which the driver ignores. Commands fail until it
  has booted: the first read that worked came about 3.8 s after mains on.

## Known gaps

Everything so far ran with nothing connected, so the measured values only ever read 0, and no protection
has tripped: their accuracy and the three trip fields are unchecked. Not implemented: protection levels (`VOLT:PROT`, `CURR:PROT`,
`POW:PROT`), the current limit, start voltage (`VOLT:ON`), slopes, ranges, setting the short circuit,
remote sense, the levels of the dynamic function, the battery, OCP and OPP tests, and Modbus.
