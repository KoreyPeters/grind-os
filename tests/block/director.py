# SPDX-License-Identifier: AGPL-3.0-or-later
"""Acts on the block test's timeline from outside the VM.

    director.py LOG QMP_SOCKET

Follows the VM's serial log (LOG, written live by vm/run.sh) for the
markers grind-host prints as it enters and leaves phases, and changes the
VM through QEMU's monitor (QMP) at those moments:

- `{"event":"block-phase","phase":"slow-device"}`: throttle the NVMe disk
  to 1 I/O per second (`block_set_io_throttle` on drive `nvm`);
- `{"event":"block-phase-end","phase":"slow-device"}`: lift the throttle.

Prints `BLOCK director ...` lines saying what it did and when. Runs until
killed (tests/block/check.sh kills it when the VM is done).
"""

import json
import os
import socket
import sys
import time

THROTTLE_IOPS = 1


class Qmp:
    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(path)
        self.file = self.sock.makefile("rw")
        greeting = self.file.readline()
        if '"QMP"' not in greeting:
            raise RuntimeError(f"QMP greeting: {greeting.strip()!r}")
        self.command("qmp_capabilities")

    def command(self, name, **args):
        msg = {"execute": name}
        if args:
            msg["arguments"] = args
        self.file.write(json.dumps(msg) + "\n")
        self.file.flush()
        while True:
            line = self.file.readline()
            if not line:
                raise RuntimeError("QMP: the monitor closed the connection")
            reply = json.loads(line)
            if "event" in reply:
                continue
            if "error" in reply:
                raise RuntimeError(f"QMP {name}: {reply['error']}")
            return reply.get("return")

    def throttle(self, iops):
        self.command(
            "block_set_io_throttle",
            device="nvm",
            bps=0, bps_rd=0, bps_wr=0,
            iops=iops, iops_rd=0, iops_wr=0,
        )


def say(what):
    print(f"BLOCK director t={time.monotonic():.3f} {what}", flush=True)


def follow(path):
    """Yield lines appended to `path`, starting from its beginning."""
    while not os.path.exists(path):
        time.sleep(0.05)
    with open(path, "r", errors="replace") as f:
        while True:
            line = f.readline()
            if line:
                yield line
            else:
                time.sleep(0.02)


def main(argv):
    if len(argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    log, qmp_path = argv[1], argv[2]
    qmp = None
    for line in follow(log):
        if '"event":"block-phase"' in line and '"phase":"slow-device"' in line:
            try:
                qmp = qmp or Qmp(qmp_path)
                qmp.throttle(THROTTLE_IOPS)
                say(f"throttled the disk to {THROTTLE_IOPS} I/O per second")
            except Exception as e:  # report, never hide (rule 3)
                say(f"FAILED to throttle the disk: {e}")
        elif '"event":"block-phase-end"' in line and '"phase":"slow-device"' in line:
            try:
                qmp = qmp or Qmp(qmp_path)
                qmp.throttle(0)
                say("lifted the throttle")
            except Exception as e:
                say(f"FAILED to lift the throttle: {e}")


if __name__ == "__main__":
    sys.exit(main(sys.argv))
