#!/usr/bin/env python3
"""Log `desk watch` of a power supply to a CSV file, e.g. a LiPo charge.

    python3 watch_to_csv.py psu charge.csv --interval 1s
    python3 watch_to_csv.py psu charge.csv --interval 1s --off-below 0.05

desk only streams and executes; this script decides. With --off-below, it switches the output
off once the supply is in CV and the current stayed below that many amps for --hold readings
(the usual end of a CC/CV charge). Set voltage and current limit yourself first, for example
`desk set psu voltage 4.2v current 1a output on`.

Standard library only. Stop with Ctrl-C.
"""

import argparse
import csv
import json
import subprocess
import sys

FIELDS = ["time", "elapsed", "output", "measured_voltage", "measured_current", "measured_power", "mode", "charge", "energy"]


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("device", help="desk device id, e.g. psu")
    p.add_argument("csv", help="file to write")
    p.add_argument("--interval", default="1s", help="time between readings (default 1s)")
    p.add_argument("--off-below", type=float, metavar="AMPS", help="switch off below this current in CV")
    p.add_argument("--hold", type=int, default=3, help="readings below the threshold before switching off")
    p.add_argument("--desk", default="desk", help="path to the desk binary")
    args = p.parse_args()

    watch = subprocess.Popen(
        [args.desk, "watch", args.device, "--json", "--interval", args.interval],
        stdout=subprocess.PIPE,
        text=True,
    )
    below = 0
    try:
        with open(args.csv, "w", newline="") as f:
            out = csv.writer(f)
            out.writerow(FIELDS)
            for line in watch.stdout:
                msg = json.loads(line)
                if not msg["ok"]:
                    print(f"watch ended: {msg['error']['message']}", file=sys.stderr)
                    return 1
                d = msg["data"]
                r = d["values"]
                out.writerow([d["time"], d["elapsed"]] + [r.get(k) for k in FIELDS[2:]])
                f.flush()
                if d["dropped"]:
                    print(f"{d['dropped']} readings dropped so far", file=sys.stderr)

                if args.off_below is None or not r.get("output"):
                    continue
                done = r.get("mode") == "cv" and r.get("measured_current") is not None and r["measured_current"] < args.off_below
                below = below + 1 if done else 0
                if below >= args.hold:
                    # The decision is ours; desk executes it (and checks it like any command).
                    off = subprocess.run([args.desk, "set", args.device, "output", "off", "--json"],
                                         capture_output=True, text=True)
                    result = json.loads(off.stdout)
                    print("output off:", "ok" if result["ok"] else result["error"]["message"], file=sys.stderr)
                    return 0 if result["ok"] else 1
    except KeyboardInterrupt:
        return 130
    finally:
        watch.terminate()
        watch.wait()
    return watch.returncode or 0


if __name__ == "__main__":
    sys.exit(main())
