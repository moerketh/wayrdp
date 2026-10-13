#!/usr/bin/env python3
# shm_pixsample.py -- sample one frame of the wayrdp hyprland-capture shm
# region and report how much of it is non-black. Used by vmconnect-harness.ps1
# (A7) to gate "frames have real content", which a frames-flowing counter
# cannot prove (an ext-image-copy pump can publish all-black frames while its
# header/seq still advance -- the classic "connects but black" regression).
#
# Prints one line:  OK <nonblack_ratio 0..1> <width> <height> <bank_bytes>
# or:               NONE <reason>
# when there's no live shm region (nobody connected -- expected if mis-timed).

import glob
import struct
import sys

HEADER_SIZE = 384  # v3: FrameHeader(160) + HdrTail pad(96) + HdrCursor(128)
MAGIC = 0x57524343  # "WCRC"


def main() -> int:
    files = sorted(glob.glob("/dev/shm/wayrdp-capture-*"))
    if not files:
        print("NONE no-shm-region")
        return 0
    path = files[0]
    try:
        with open(path, "rb") as fh:
            hdr = fh.read(HEADER_SIZE)
            if len(hdr) < HEADER_SIZE:
                print("NONE short-header")
                return 0
            magic, w, h = struct.unpack_from("<III", hdr, 0)
            if magic != MAGIC or w == 0 or h == 0:
                print("NONE bad-header")
                return 0
            active = struct.unpack_from("<I", hdr, 160)[0] & 1
            bank = w * h * 4
            fh.seek(HEADER_SIZE + active * bank)
            pix = fh.read(bank)
    except OSError as exc:
        print(f"NONE read-error {exc}")
        return 0
    if not pix:
        print("NONE empty-bank")
        return 0
    sample = pix[::64]  # BGRA stride-64: catch any lit channel, cheap on 9MB
    nonblack = sum(1 for b in sample if b != 0)
    ratio = nonblack / max(1, len(sample))
    print(f"OK {ratio:.4f} {w} {h} {len(pix)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
