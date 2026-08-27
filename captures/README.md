# captures/

Bench PROFINET captures (ground truth). **The `.pcapng` files are not versioned**
(large + reproducible; risk of git corruption under WSL/NTFS). The extracted reference
bytes are frozen in [`../docs/dcp-golden-frames.md`](../docs/dcp-golden-frames.md)
and embedded as hex in the `dcp` module tests.

## Provenance
Bench 2026-06-26: **S7-1500 CPU 1515-2 PN (FW V2.9)** = IO-Controller ↔ **PLCSIM
Advanced `i-device`** instance, isolated segment (no CPL), captured via Wireshark/npcap,
decoded with tshark 4.6.6.

| File | Contents |
|---|---|
| `dcp-identify.pcapng` | DCP Identify req/resp (Plan 2 golden frames) |
| `dcp-identify-01.pcapng` | same, cleaned segment (no CPL); also shows AR reject `nca_unk_if` |
| `dcp-set.pcapng` | Identify/connect-retry cycles (no real DCP-Set: PLCSIM does not receive any) |

## Bench 2 — 2026-08-27: real AR/RT/alarm ground truth (p-net on the edge)
**S7-1500 CPU 1515-2 PN (6ES7 515-2AM02-0AB0, HW 3, FW V2.9.4)** = IO-Controller on X2
(172.16.2.100) ↔ **p-net `pn_dev` v0.2.0** = IO-Device on `lab-server`/`eno2` (172.16.2.10,
`rt-labs-dev`), update time 32 ms, captured with `tcpdump` on the edge (no filter), decoded with
tshark 4.6.6. Procedure: `docs/bench-pnet-device.md`.

| File | Contents |
|---|---|
| `ar-connect-2026-08-27-164334.pcapng` | **the reference**: CPU Ident Req (name filter) → device Ident Ok → CPU Set Req IP / Set Ok → ARP → **Connect req/res** (2 IOCR, 5 ExpectedSubmodule, AlarmCR) → Write MultipleWrite (PDInterfaceAdjust + module params) → PrmEnd → **ApplicationReady** (device→CPU) → RTC1 cyclic. Plus TIA S7 traffic (port 102) and LLDP. |
| `ar-connect-2026-08-27-163058.pcapng` | earlier run, **buggy capture filter** (no UDP): only DCP Hello/Ident Ok/Set + RTC1. Keep for the DCP Hello Req (DeviceInitiative). |
| `ar-connect-2026-08-27-164226.pcapng` | AR already up (pn_dev not restarted): cyclic + S7 + LLDP only. Disposable. |
| `rt-cyclic-2026-08-27-164031.pcapng` | 10 s stable RUN: RTC1 `0x8000` (device→CPU) / `0x8001` (CPU→device), 40 data bytes, cycle counter step 1024 (= 32 ms), data status `0x35`. |
| `alarm-2026-08-27-164431.pcapng` | process alarm slot 1 (button2): frame ID `0xfc01` Alarm High, Data-RTA → ACK-RTA → Alarm Ack → ACK-RTA. |
| `io-bits-2026-08-27-164448.pcapng` | button1 pressed 4 s during cyclic: DI byte bit 7 set on the wire ↔ `%I0.7` TRUE in TIA (BOOL bit order, input side). |
| `q-bits-2026-08-27-165102.pcapng` | `%Q0.0 := TRUE` alone → `QB0 = 0x01` on the wire (BOOL bit order, output side: `.0` = LSB). |
| `echo-2026-08-27-165307.pcapng` | `%QD2 := 16#12345678`, `%QD6 := 1.5` → `12 34 56 78 3f c0 00 00`: declaration order, big-endian, IEEE-754 (REAL codec). |
| `release-2026-08-27-165347.pcapng` | CPU RUN → STOP: **no Release RPC**, AR kept, CPU data status `0x35 → 0x25` (ProviderState Stop). |
| `device-loss-2026-08-27-165535.pcapng` | `pn_dev` killed then restarted with CPU in STOP: CPU ERR-RTA "DHT/WDT expired" after ~96 ms, DCP Identify resumes, full AR re-established (CPU frames `0x25`). |

## Known limitation (bench 1)
PLCSIM Advanced **does not perform real-time PROFINET IO** (AR/RT cyclic) on the wire →
no Connect/AR/RT/alarm golden frames in bench 1. **Never use a capture filter containing
`vlan` together with `udp port ...`** (libpcap offset shift drops untagged UDP — this is what
emptied the first ar-connect capture).
