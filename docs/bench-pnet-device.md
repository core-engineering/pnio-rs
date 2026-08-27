# Bench — p-net as a real IO-Device (generating AR/RT ground truth)

## Why
PLCSIM Advanced **does not support real-time PROFINET IO** (see `captures/README.md`: Connect
rejected with `nca_unk_if`). Capturing **Connect/AR + cyclic RT + alarm** (Plans 3-5) requires a
**real IO-Device** facing the S7-1500. No ET200 is available → we use the sample app
**`pn_dev` from p-net (rt-labs)** on the **Debian edge** as the test peer.

- **License**: p-net is GPLv3 — acceptable here because it is a **test peer that is not shipped** (nothing
  from p-net enters our crate). It is also an **inspectable reference implementation**.
  Its GSDML is kept out of git as well (`captures/.gitignore`).
- **Bonus**: the controller's startup sequence produces a real **DCP-Set** → golden frame for DCP Set
  (deferred from Plan 2).

## Topology (as commissioned 2026-08-27)
```
 laptop TIA ──LAN 192.168.1.0/24──► edge "lab-server" ──eno2 172.16.2.0/24──► S7-1500
 (192.168.1.x)   (.200 = NAT 1:1      eno1 .21 / .200          .10            .100
                  to the PLC)         pn_dev = IO-Device on eno2
                                      tcpdump on eno2 (sees DCP + AR UDP 34964 + RT L2)
```
- The edge is **also the TIA gateway**: DNAT `.200 → 172.16.2.100`, MASQUERADE out of `eno2`,
  forward opened in `DOCKER-USER`. None of this touches EtherType `0x8892` (L2) nor the inbound
  RPC on UDP 34964 (`INPUT`, not `FORWARD`). Host firewall: none (`nftables` inactive,
  `/etc/nftables.conf` = empty accept chains).
- `eno2`: Intel `igb`, 100 Mb/s full duplex, **not bridged**, `rx-vlan-offload: on`
  (fine for `tcpdump`; our own `AfPacketTransport` must read the tag via `PACKET_AUXDATA`).
- Kernel: Debian 13, `6.12 PREEMPT_DYNAMIC` (not `PREEMPT_RT`). `linux-image-rt-amd64` is
  available in the distro for later. Use a **32 ms update time** in TIA until then.
- No port mirroring needed: the edge is the device, so its own interface carries everything.

## Hard rules (the edge stays a gateway)
1. **Device IP in TIA = `172.16.2.10`, exactly**, name fixed. The controller sends a DCP Set IP
   at AR startup; with an identical address the Set is idempotent. Any other address would
   re-address `eno2` and kill the TIA path.
2. **`set_network_parameters` is replaced by a guard** (`~/p-net/build/set_network_parameters`,
   original kept as `.orig`): no-op if the requested IP equals the current one, **refuses (rc=1)**
   otherwise. The stock script does `ip address flush` + `ip route add default` — never run it here.
3. **TIA cannot do DCP through the NAT** (L2). "Accessible devices", "Assign device name" and
   LED flashing from the laptop will not reach the device. Not blocking: the name comes from
   `pn_dev -s`, and the **CPU itself** emits Identify (NameOfStation filter) + Set IP on `eno2`.

## 1. Edge prerequisites (Debian 13, user `maintenance`)
```bash
sudo apt install -y build-essential cmake tcpdump   # done 2026-08-27
```
Root (or capabilities) is required for raw AF_PACKET sockets, and `pn_dev` (built with
`USE_SCHED_FIFO`) also creates a `SCHED_FIFO` thread → needs `cap_sys_nice`, otherwise it dies
with "Failed to start" right after "Start sample application main loop". One-off, as root:
```bash
sudo setcap cap_net_raw,cap_net_admin,cap_sys_nice+eip /home/maintenance/p-net/build/pn_dev
sudo setcap cap_net_raw,cap_net_admin+eip /usr/bin/tcpdump
```
After that, everything below runs unprivileged over SSH.

## 2. Clone + build p-net — **use tag `v0.2.0`**
The upstream `public` branch has been stripped of `CMakeLists.txt` and `sample_app/` since
2022-06 (build system and sample moved behind rt-labs' login-gated docs). The last complete
standalone release is **`v0.2.0`** (2022-04-19): CMake + `sample_app` + Linux port + GSDML.
```bash
git clone https://github.com/rtlabs-com/p-net.git && cd p-net
git checkout v0.2.0
git submodule update --init --recursive        # cmake/tools (rtlabs-com/cmake-tools)
cmake -B build -S . -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTING=OFF   # fetches rtlabs-com/osal @88784fc
cmake --build build -j4
```
Built 2026-08-27 on the edge: `~/p-net/build/pn_dev` (revision `0.2.0+v0.2.0`).

## 3. `pn_dev` flags (confirmed from `--help`)
| Flag | Meaning |
|---|---|
| `-i IFACE` | interface (default `eth0`) → **`eno2`** |
| `-s NAME` | station name (default **`rt-labs-dev`**), only used if not already in storage |
| `-p PATH` | **absolute** storage directory → `/home/maintenance/pnet-data` |
| `-b FILE` / `-d FILE` | Button1 (input data) / Button2 (alarm → diagnosis → logbook cycle) text files, `1` = pressed |
| `-v` (repeat) | verbosity; `-vvv` shows DCP → Connect → PrmEnd → ApplicationReady → cyclic |
| `-g` | dump stack details and exit |
| `-f` / `-r` | factory reset / remove storage files |

GSDML: `sample_app/GSDML-V2.4-RT-Labs-P-Net-Sample-App-20220324.xml` (copied to
`captures/`, git-ignored). VendorID `0x0493`, DeviceID `0x0002`, `DNS_CompatibleName="rt-labs-dev"`,
`MinDeviceInterval=32` (= 1 ms send clock supported). Modules:
- `0x30` I8 (`Unsigned8`, `UseAsBits`) — 8 DI → **BOOL bit-order check**
- `0x31` O8 — 8 DO
- `0x32` I8O8
- `0x40` Echo — `Unsigned32` + `Float32` in and out → **REAL big-endian codec check**

## 4. Session scripts (`~/bench/` on the edge)
| Script | Does |
|---|---|
| `pnet-start.sh` | starts `pn_dev -vvv -i eno2 -s rt-labs-dev -p ~/pnet-data -b/-d button files` in background, log in `~/bench/logs/` |
| `pnet-stop.sh` | `SIGINT` → `SIGTERM` → `SIGKILL` to `pn_dev`, waits for exit (SIGINT alone is ignored) |
| `capture.sh <phase> [seconds]` | `tcpdump -i eno2 -s 0 -w ~/bench/captures/<phase>-<ts>.pcapng`, **no capture filter** (the `vlan` keyword breaks `udp port 34964`, see `bench-capture-protocol.md`) |
| `capture-stop.sh` | `SIGINT` to `tcpdump` (flushes the file) |

Button files: `echo 1 > ~/pnet-data/button1.txt` (input bit), `echo 1 > ~/pnet-data/button2.txt`
then back to `0` (each press advances alarm → diag → logbook).

## 5. TIA side (the S7 = IO-Controller)
1. **Options → Manage GSD files** → import the GSDML above → install.
2. Catalog: *Other field devices → PROFINET IO → RT-Labs → P-Net Sample App* → drag onto the
   S7's IO system, subnet `172.16.2.0/24`.
3. Device properties: **PROFINET name `rt-labs-dev`**, **IP `172.16.2.10`**, router unchecked.
4. Plug modules: slot 1 `I8`, slot 2 `O8`, slot 3 `I8O8`, slot 4 `Echo` (any subset works; keep
   `I8` and `Echo` at least for the two codec checks).
   Resulting addresses (TIA project `PLC_BENCH`, 2026-08-27):

   | Slot | Module | CPU inputs | CPU outputs | Bench use |
   |---|---|---|---|---|
   | 1 | DI 8xLogicLevel | `%IB0` | — | bit order: `button1.txt` → which bit of `%IB0` |
   | 2 | DO 8xLogicLevel | — | `%QB0` | bit order: force `%Q0.0` alone → `pn_dev` log must show `0x01` |
   | 3 | DIO 8xLogicLevel | `%IB1` | `%QB1` | second sample |
   | 4 | Echo Module | `%IB2..9` | `%QB2..9` | REAL codec: `%QD2` (UDInt) + `%QD6` (Real) echoed to `%ID2`/`%ID6` (order to confirm on the wire) |

   = 10 data bytes per direction + IOPS/IOCS per submodule.
5. **Update time 32 ms** (send clock 1 ms × reduction ratio 32), watchdog factor default.
6. Compile + download to the physical S7 via `192.168.1.200` (usual NAT path).

→ In RUN the CPU discovers the device by DCP on `eno2`, opens the AR, the device turns green in
the TIA device view; `pn_dev` log shows the cyclic exchange.

## 6. Capture scenario (1 pcapng per phase, run on the edge)
```bash
~/bench/capture.sh ar-connect &   # start capture FIRST
~/bench/pnet-start.sh              # then the device → CPU: Identify, Set IP, Connect, PrmEnd, AppReady
# wait for "cyclic" in the log, then:
~/bench/capture-stop.sh
~/bench/capture.sh rt-cyclic 10    # 10 s stable RUN (PPM/CPM, IOPS/IOCS, data status)
~/bench/capture.sh alarm 20 &  echo 1 > ~/pnet-data/button2.txt; sleep 2; echo 0 > ~/pnet-data/button2.txt; wait
~/bench/capture.sh release 15 &  # put the CPU in STOP during the window
```
The `ar-connect` file also contains the **DCP Identify (name filter) + DCP Set IP** from the CPU.

Then copy `~/bench/captures/*.pcapng` into `203-profinet-rt/captures/` (git-ignored) and decode
with `tshark.exe` via WSL interop (`/mnt/c/Program Files/Wireshark/tshark.exe`).

## 6b. Observed on the wire (2026-08-27)
- **AR bring-up in 11 ms**: Ident Ok → Set Req IP / Set Ok → ARP → Connect req (699 B: ARBlockReq,
  2× IOCRBlockReq, 5× ExpectedSubmoduleBlockReq, AlarmCRBlockReq) → Connect res → Write
  `MultipleWrite` (PDInterfaceAdjust + module params 0x7b/0x7c/0x7d) → PrmEnd → **ApplicationReady
  (device → CPU)** → RTC1. CPU LLDP: `6ES7 515-2AM02-0AB0`, HW 3, FW V2.9.4.
- **RTC1**: device→CPU frame ID `0x8000`, CPU→device `0x8001`, VLAN prio 6, 40 data bytes
  (padded), cycle counter step 1024 (= 32 ms). CPU data status `0x35` (Primary). p-net sends
  `0x36` (State=Backup, Redundancy=1) **on purpose** (a Button2 cycle step: "Setting cyclic data
  to backup and to redundant") and the CPU accepts it — our stack sends `0x35`.
- **CPU STOP does NOT release the AR** (`captures/release-*.pcapng`): no RPC, no DCP; the CPU keeps
  its cyclic frames and only flips data status `0x35 → 0x25` (bit 4 ProviderState Run→Stop).
  Release only happens on hardware-config download / power-off. A consumer must treat
  ProviderState=Stop as "outputs invalid, AR alive".
- **Cyclic payload layout** (device→CPU, C-SDU bytes):

  | Bytes | Content |
  |---|---|
  | 0-2 | IOPS of the 3 DAP submodules (0x1, 0x8000, 0x8001) |
  | 3, 4 | slot 1 DI8 data (sample-app counter, step 2; **bit 7 = Button1**), IOPS |
  | 5 | slot 2 DO8 IOCS (output module → only its IOCS travels in this direction) |
  | 6, 7, 8 | slot 3 DIO data (counter+1), IOPS, IOCS |
  | 9-16, 17, 18 | slot 4 Echo data (UDInt + Real), IOPS, IOCS |
  | 19-39 | padding to the 40-byte RT minimum |

  CPU→device is the mirror: DAP IOCS ×3, DI IOCS, `QB0`+IOPS, DIO IOCS/`QB1`/IOPS, Echo IOCS/`QB2..9`/IOPS.
- **BOOL bit order settled (both directions)**: `%Q0.0 := TRUE` alone → `QB0 = 0x01` on the wire
  (`captures/q-bits-*.pcapng`); device byte `0x80` (Button1) → `%I0.7 = TRUE` in TIA. Bit `i` =
  `1 << i` within the byte, `.0` = LSB. `data::get_bit`/`set_bit` confirmed.
- **Process type encoding settled**: `%QD2 := 16#12345678`, `%QD6 := 1.5` → `QB2..9 =
  12 34 56 78 3f c0 00 00` (`captures/echo-*.pcapng`): declaration order preserved (UDInt then
  Real), big-endian, IEEE-754 single. The Echo module's reply (`12 b4 56 78 7f 80 00 00`) is the
  sample app's own gain arithmetic (module parameter written at index 0x7d), not an identity echo.
- **Device loss** (`pn_dev` killed, `captures/device-loss-*.pcapng`): ~96 ms (= 3 × 32 ms watchdog)
  after the last device frame the CPU sends an **ERR-RTA on the alarm channel**
  ("AR consumer DHT/WDT expired", `RTA_ERR_ABORT`), then resumes DCP Identify (name filter)
  every 2.6 s. **Reconnect with the CPU in STOP** works: same AR sequence, CPU cyclic frames
  carry `0x25` from the first one.
- **Alarm** (Button2): frame ID `0xfc01` Alarm High, Data-RTA (Alarm Notification, Process, slot 1)
  → CPU ACK-RTA → CPU Alarm Ack → device ACK-RTA.

## 7. Next steps
With `ar-connect.pcapng` + `rt-cyclic.pcapng` we can **scope and execute Plan 3 (`cm`/AR)**:
DCE-RPC over UDP 34964, ARBlockReq/IOCRBlockReq/ExpectedSubmodule/AlarmCR blocks, AR state machine
up to DATA, then CControl ApplicationReady from the device side — on real ground truth.
`rt-cyclic.pcapng` + the `I8` module settle the **BOOL bit-order** follow-up; the `Echo` module
settles the REAL codec on the wire.

## Pitfalls
- **Never use CPL/PowerLine** on the segment (HomePlug `0x88e1` → jitter → RT watchdog expires, AR drops).
- p-net **must run on native Linux with L2 access** (the edge) — **not in WSL2** (NATed network).
- `pn_dev` needs `cap_net_raw`/`cap_net_admin` (or root); the storage dir must be an **absolute** path.
- The p-net device has the **rt-labs Vendor/Device ID** (not ours) — expected: we are capturing the
  **structure** of the frames, identical to what our stack will produce.
- The supervision Docker stack on the same edge is a jitter source — fine at 32 ms, not for < 2 ms.
