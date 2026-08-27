# Bench Protocol — PROFINET Captures (ground truth for Plans 2-5)

Goal: capture a complete PROFINET session **on the IO-Device side** with a conformant peer,
to feed the test vectors for Plans 2-5 and resolve open questions
(`FOLLOWUPS.md`, notably the BOOL bit order LSB-first).

## Topology

```
   S7-1500 physical  ───cable───  PC Windows NIC
   (IO-Controller)                (TIA + PLCSIM Advanced
    192.168.1.60                   = virtual PLC as I-Device
    "io-controller"                = IO-Device = OUR model
                                    192.168.1.61 "i-device")
                                        │
                            Wireshark capture on
                            "Siemens PLCSIM Virtual Ethernet Adapter"
```

Advantage: the Siemens virtual adapter sees all its L2 traffic → captures DCP + AR + RT
**without a TAP or managed switch**.

⚠️ PLCSIM timing/jitter is NOT representative (simulation). The **frame structure is
accurate** = sufficient for Plans 2-5. Determinism validation < 2 ms (Plan 7) will require a
100% physical bench.

## Address Plan

| Role | IP | NameOfStation |
|---|---|---|
| IO-Controller (S7-1500 1515-2 PN, FW V2.9) | 192.168.1.60 | `io-controller` |
| I-Device (virtual PLC PLCSIM Advanced) | 192.168.1.61 | `i-device` |
| Capture PC (NIC) | 192.168.1.10 | — |

Send clock: **1 ms**. Example mapping: **16 REAL + 32 BOOL per direction** (= 68 bytes/direction).

## Capture

- **Capture filter**: **none** (capture everything, filter at analysis). ⚠️ Do **not** use
  `ether proto 0x8892 or vlan or udp port 34964`: in libpcap the `vlan` keyword shifts the
  offsets of every primitive after it, so `udp port 34964` then only matches *tagged* UDP and
  the untagged DCE-RPC Connect is silently dropped (bitten on 2026-08-27).
- **Wireshark display filter**: `pn_dcp or pn_rt or pn_io or udp.port == 34964`
- Wireshark has a native PROFINET dissector → used as oracle to validate our decoding.
- Edge equivalent (later, once the Rust stack is running):
  ```
  sudo tcpdump -i eno2 -s 0 -w capture.pcapng      # no filter, see above
  ```

## Scenario (1 .pcapng per phase)

- [ ] **1. DCP Identify** — TIA → *Accessible devices*. Capturable on the PC NIC **without
      I-Device**. → Plan 2. File: `dcp-identify.pcapng`
- [ ] **2. DCP Set** — right-click device → *Assign PROFINET device name*. → Plan 2.
      File: `dcp-set.pcapng`
- [ ] **3. Connect / AR** — transition to RUN, AR establishment: RPC Connect / Write Record /
      Dcontrol / Ccontrol (UDP 34964). → Plan 3. File: `ar-connect.pcapng`
- [ ] **4. Cyclic RT** — stable RUN ~5-10 s: PPM/CPM at 1 ms, IOPS/IOCS/data-status.
      → Plan 4. **Validate BOOL bit order (LSB-first) here.** File: `rt-cyclic.pcapng`
- [ ] **5. Alarm** — force a fault (module removed / channel forced). → Plan 5.
      File: `alarm.pcapng`
- [ ] **6. Disconnect** — clean STOP. File: `release.pcapng`

Also keep **short excerpts** (1-2 frames/type) for lightweight unit tests.

## Metadata to Record (clean-room reference)

- [ ] Send clock + reduction ratio + device update time
- [ ] NameOfStation on both sides (confirmed above)
- [ ] Vendor ID / Device ID observed on the wire
- [ ] VLAN tag presence + priority (typically 6 for RT)
- [ ] **GSDML export** of the example I-Device → reference for declaration mapping → (byte, bit)

## Repository

Captures in `203-profinet-rt/captures/`. Replayable via `capture::PcapFrames`.
