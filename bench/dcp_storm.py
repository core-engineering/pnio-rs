#!/usr/bin/env python3
"""DCP Identify-All storm: raw PROFINET DCP requests at a paced rate.

Every station on the segment must answer each Identify-All, so this loads the
DCP responders of the device under test *and* of the controller. Needs
cap_net_raw (run through a python3 copy that carries it, or as root).

    dcp_storm.py --iface eno2 --rate 1000 --seconds 240 [--all | --station pnio-dev]
"""
import argparse
import socket
import struct
import time

p = argparse.ArgumentParser()
p.add_argument("--iface", required=True)
p.add_argument("--rate", type=float, required=True, help="requests per second")
p.add_argument("--seconds", type=float, required=True)
p.add_argument("--station", help="Identify this NameOfStation instead of Identify-All")
a = p.parse_args()

s = socket.socket(socket.AF_PACKET, socket.SOCK_RAW)
s.bind((a.iface, 0))
src = s.getsockname()[4]
dst = bytes.fromhex("010ecf000000")  # PN-MC DCP Identify multicast

if a.station:
    name = a.station.encode()
    blk = struct.pack(">BBH", 0x02, 0x02, len(name)) + name  # Device / NameOfStation
    if len(name) % 2:
        blk += b"\x00"
else:
    blk = struct.pack(">BBH", 0xFF, 0xFF, 0)  # All / All

def frame(xid: int) -> bytes:
    dcp = struct.pack(">BBIHH", 0x05, 0x00, xid, 0x0001, len(blk)) + blk  # Identify, Request, xid, ResponseDelay 1, DCPDataLength
    f = dst + src + b"\x88\x92" + b"\xfe\xfe" + dcp
    return f + b"\x00" * max(0, 60 - len(f))

period = 1.0 / a.rate
n = 0
t0 = time.perf_counter()
deadline = t0 + a.seconds
next_t = t0
while True:
    now = time.perf_counter()
    if now >= deadline:
        break
    if now < next_t:
        d = next_t - now
        if d > 0.0002:
            time.sleep(d - 0.0001)
        continue
    try:
        s.send(frame(n & 0xFFFFFFFF))
        n += 1
    except OSError:
        pass
    next_t += period
    if next_t < now - 0.002:
        next_t = now
el = time.perf_counter() - t0
print(f"sent {n} DCP Identify requests in {el:.1f} s = {n / el:.0f}/s")
