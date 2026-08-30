#!/usr/bin/env python3
"""Unprivileged broadcast storm: UDP datagrams to the subnet broadcast address.

Every datagram becomes an Ethernet broadcast frame (ff:ff:ff:ff:ff:ff), so a
switch floods it to every port, including the controller's. Paced to a target
bit rate on the wire (Ethernet + IP + UDP overhead counted).

    bcast_storm.py --bind 172.16.2.10 --target 172.16.2.255 --mbit 80 --seconds 240
"""
import argparse
import socket
import time

p = argparse.ArgumentParser()
p.add_argument("--bind", required=True, help="local IP on the interface to storm from")
p.add_argument("--target", required=True, help="subnet broadcast address")
p.add_argument("--port", type=int, default=9, help="UDP port (9 = discard)")
p.add_argument("--mbit", type=float, required=True, help="target rate on the wire, Mbit/s")
p.add_argument("--payload", type=int, default=1472, help="UDP payload bytes (1472 = full frame)")
p.add_argument("--seconds", type=float, required=True)
a = p.parse_args()

s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)
s.bind((a.bind, 0))
data = b"\x55" * a.payload
wire_bits = (a.payload + 8 + 20 + 14 + 4 + 8 + 12) * 8  # UDP+IP+Eth+FCS+preamble+IFG
period = wire_bits / (a.mbit * 1e6)
n = 0
t0 = time.perf_counter()
deadline = t0 + a.seconds
next_t = t0
while True:
    now = time.perf_counter()
    if now >= deadline:
        break
    if now < next_t:
        # coarse sleep then spin for the last stretch
        d = next_t - now
        if d > 0.0002:
            time.sleep(d - 0.0001)
        continue
    try:
        s.sendto(data, (a.target, a.port))
        n += 1
    except OSError:
        pass  # ENOBUFS etc.: just miss this slot
    next_t += period
    if next_t < now - 0.002:  # fell behind: cap the burst to 2 ms of frames
        next_t = now
el = time.perf_counter() - t0
print(f"sent {n} frames in {el:.1f} s = {n / el:.0f} pps = {n * wire_bits / el / 1e6:.1f} Mbit/s on the wire")
