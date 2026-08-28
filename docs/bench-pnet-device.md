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

## 6c. HIL — profinet-rt itself as the IO-Device (2026-08-28)

With Plan 3 (`rpc` + `cm`) implemented, `examples/ar_bringup` replaces `pn_dev` as the peer
facing the CPU on `eno2` — same topology, same TIA project `PLC_BENCH`, same p-net GSDML
(station `rt-labs-dev`, `172.16.2.10`, 32 ms update time, CPU indifferent to STOP/RUN).

**Build**: a plain debug build copied to the edge fails to run — the edge's glibc (2.41) is
older than the build host's, so the dynamically-linked binary refuses to start. Built musl
instead:
```bash
. "$HOME/.cargo/env" && rustup target add x86_64-unknown-linux-musl
cargo build --release --example ar_bringup --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/examples/ar_bringup maintenance@192.168.1.21:bench/
```
The AF_PACKET/UDP raw-socket capabilities do not survive a `scp` (they are file
attributes, not part of the file content) — `setcap` must be **re-applied after every
copy**:
```bash
sudo /usr/sbin/setcap cap_net_raw,cap_net_admin+eip /home/maintenance/bench/ar_bringup
```
Run, capture running first:
```bash
nohup ~/bench/capture.sh hil-ar-bringup > ~/bench/logs/capture-hil.out 2>&1 &
RUST_LOG=info ~/bench/ar_bringup --iface eno2 --name rt-labs-dev --ip 172.16.2.10 2>&1 | tee ~/bench/logs/ar_bringup.log
```
Expected log sequence: `device up on eno2 as ...`, then on the CPU's first DCP Identify
`AR state: Connected`, then `AR state: Data` (`AppReadySent` is an internal transition, not
notified).

### Run 1 — 2026-08-28 08:30, binary at `44e60e1` (musl)
Capture `captures/hil-ar-bringup-2026-08-28-082958.pcapng`, log `~/bench/logs/ar_bringup.log`.

- CPU DCP Ident Req (name filter) every 2.6 s from t=0; at t=2.601 s our Ident Ok (untagged,
  IP block already reports `172.16.2.10`). **No DCP Set was sent by the CPU this time** — with
  p-net the Ident Ok reports `0.0.0.0` and draws a Set; our device already carries the
  configured IP, so the CPU has nothing to correct.
- t=2.605 Connect req → our Connect res (OK) → Write `MultipleWrite` → our Write res → PrmEnd →
  our Done → our ApplicationReady sent from UDP port 34964 → CPU's Done from port 56424 at
  t=2.611. **AR reached Data in 6 ms.**
- **Byte-identity vs p-net**: our four response/request PDUs (Connect res, Write res, PrmEnd
  res, ApplicationReady req) are byte-identical to the `docs/cm-golden-frames.md` goldens
  except the RPC activity UUID (5 trailing bytes, per-run) and the session-key byte (the CPU
  picked session 4 here, 2 in the golden capture) — both are protocol state the initiator/
  responder are expected to vary, not codec defects.
- CPU sent cyclic RTC1 (frame ID `0x8001`, data status `0x35`) from t=2.63 for 6.7 s even
  though we sent no cyclic frames ourselves (RT/`rt` is Plan 4). At t=9.302 the CPU issued an
  RPC **Read, index `0xfbff`** ("Trigger index for RPC connection monitoring"). Our device had
  no `Read` handling and answered `service_unsupported` — decoded by the CPU as
  `0x81,0x81,0x05,0` ("Faulty PrmServerBlockReq") — which the CPU treated as a monitoring
  failure: it raised an **ERR-RTA "AR RPC-Read error"** on the alarm channel and dropped the AR.
- The CPU then retried every 1.5 s: Ident Req → our Ident Ok → **Connect with the same ARUUID**
  (`e5e1aecc-…`, stable per configured AR) **and SessionKey incremented by one each time**
  (4 → 5 → 6 → …), each carrying a fresh activity UUID. Our state machine (pre-fix) treated
  this as a retransmission of the first Connect and kept resending the stale cached response,
  so the AR stayed stuck at `Data` without ever completing the new handshake: 323
  `AR Data --Tick--> Data` info-level lines logged over 25 s.

### Fixes (`aca42d9`, `8ab2711`)
- Controller reconnect — same ARUUID with a bumped session key, **or** a new ARUUID from the
  same initiator — now aborts the stale AR (`AbortReason::ControllerReconnect`) and accepts the
  new Connect instead of replaying the cached response.
- `Read`/`ReadImplicit` are refused with a well-formed PNIORW error
  (`0xDE,0x80,0xB0,0x00`, "invalid index") instead of `service_unsupported`.
- No-op `Tick` transitions are logged at `debug`, not `info` (removes the log flood).
- The outgoing RPC call sequence number stays monotonic across an AR takeover (it no longer
  resets and risks colliding with the previous AR's in-flight sequence).

### Run 2 — 2026-08-28 08:51, binary at `aca42d9`
Capture `captures/hil2-reconnect-2026-08-28-085142.pcapng`, log `~/bench/logs/ar_bringup2.log`.

- Same 6 ms bring-up to `Data`. **With the PNIORW error reply, the CPU keeps the AR alive**:
  it re-issues the Read on index `0xfbff` every 6.7 s (t=10.1, 16.8, 23.5, 30.2), cyclic
  `0x8001` runs for the whole 30 s window, no ERR-RTA, no reconnect. Zero `Tick` lines logged.
- **Conclusion**: the CPU's RPC connection-monitoring Read only needs *any* well-formed RPC
  reply on time — not a successful Read of `0xfbff`. Any `PnioStatus` error the CPU can parse
  satisfies it; `service_unsupported` did not (it was not a recognized status combination).

### What a Plan 4 run should expect
Plan 3 exercises the acyclic path only (no RTC1 sent by us). Once `rt` sends cyclic frames
(Plan 4), the CPU's RPC connection monitoring is expected to fall back to the cyclic watchdog
path it uses with a real IO-Device: the periodic `Read 0xfbff` probe seen in both runs above
is specific to an acyclic-only peer and should stop appearing once we produce RTC1 frames at
the negotiated update time. Minimal `Read` support beyond the PNIORW refusal (index `0xfbff`,
I&M reads) stays deferred to Plan 5 (see `FOLLOWUPS.md`).

## 6d. HIL — cyclic exchange with profinet-rt (2026-08-28)

With Plan 4 (`rt`) implemented, `examples/rt_bringup` replaces `ar_bringup` as the peer facing
the CPU: same topology, same TIA project `PLC_BENCH`, same p-net GSDML (station
`rt-labs-dev`, `172.16.2.10`, 32 ms update time), now sending RTC1 (`0x8000`) cyclic frames
instead of relying on the acyclic `Read 0xfbff` keep-alive.

**Build, copy, `setcap`** — same musl cross-build as §6c (edge glibc 2.41 < build host):
```bash
. "$HOME/.cargo/env" && cargo build --release --example rt_bringup --target x86_64-unknown-linux-musl
scp target/x86_64-unknown-linux-musl/release/examples/rt_bringup maintenance@192.168.1.21:bench/
sudo /usr/sbin/setcap cap_net_raw,cap_net_admin,cap_sys_nice+eip /home/maintenance/bench/rt_bringup
```
`cap_sys_nice` is new vs `ar_bringup`: it lets `--rt-priority` set `SCHED_FIFO` without root.
`setcap` must be **re-applied after every copy** (same reason as §6c: it's a file attribute,
not part of the file content, and `scp` does not preserve it).

**Run**, capture running first:
```bash
nohup ~/bench/capture.sh hil-rt > ~/bench/logs/capture-hil-rt.out 2>&1 &
RUST_LOG=info ~/bench/rt_bringup --iface eno2 --name rt-labs-dev --ip 172.16.2.10 \
  --rt-priority 50 --stats-every 30 2>&1 | tee ~/bench/logs/rt_bringup.log
```
Expected log: AR reaches `Data` within ~1 s of the first DCP Identify and **stays there** for
the whole run (no further state change); periodic stats lines with `tx ≈ rx_accepted` growing
at 31/s (32 ms period), `watchdog 0`, `missed_ticks 0`, freshness `Fresh` while the CPU is in
RUN.

### Run 1 — 13:41 CEST, binary `2e1077d`, `--rt-priority 50`, 90 s
Capture `captures/hil-rt-2026-08-28-134148.pcapng`, log `~/bench/logs/rt_bringup.log`.

- AR to `Data` in < 1 s. Log: `WARN RT scheduling warning: SCHED_FIFO priority 50: Function
  not implemented (os error 38)` — **musl's libc stubs `sched_setscheduler` with `ENOSYS`**;
  the runner fell back to normal scheduling priority and kept running (not fatal).
- Stats at 85 s: `tx 2651 = rx_accepted 2651`, `rx_ignored 0`, `dropped 0`, `invalid 0`,
  `reordered 0`, `watchdog 0`, `missed_ticks 0`, `input_snapshot_reused 0`,
  `output_publish_deferred 3`, `max_tick_lateness 36 µs`, freshness `Fresh`.
- Capture: our `0x8000` every 31.8–32.2 ms, cycle counter step always 1024, data status
  `0x35`, VLAN prio 6; CPU `0x8001` data status `0x35`; **zero RPC after AR setup** (no
  `Read 0xfbff` probe — the CPU's cyclic watchdog took over from the acyclic keep-alive seen
  in Plan 3, as predicted in §6c). One ERR-RTA from the CPU at t = 93 s, its own watchdog
  firing after our `SIGINT` (expected teardown).
- **Bench finding — IOCS deadlock**: the CPU's IOxS bytes for its outputs were `0x60` (BAD,
  "detected by controller") for the entire run. Our engine mirrored the received IOPS into
  our own IOCS — i.e. we echoed back "BAD" because the CPU's IOPS said "BAD" — so we never
  told the CPU its outputs were consumed. In TIA: the device stayed **green** (AR alive,
  cyclic data flowing) but the **diagnostics buffer** kept "User data failure of hardware
  component" (HW IDs 257/258/259/262/263) open for the whole run, because the CPU never saw
  IOCS = GOOD from us. The CPU did consume our inputs (our IOPS was correctly GOOD, so its
  IOCS was `0x80`), but the reverse direction was stuck: a real deadlock caused by treating
  "IOCS = last received IOPS" as the rule. **Fix (`de8479b`)**: IOCS is the *consumer's own*
  status, not a mirror of the producer's IOPS — always GOOD for every plugged submodule we
  consume, independent of what the CPU's IOPS says (this is p-net's own behaviour, and matches
  IEC semantics: IOPS/IOCS are two independent per-direction judgments, not a request/ack
  pair). `rx_iops_good` is kept, but only feeds the application's `Validity`, not our IOCS.
  Spec §7 and the Decisions table were updated accordingly.

### Run 2 — 13:44, no `--rt-priority`, 60 s
Capture `captures/hil-rt2-2026-08-28-134426.pcapng`, log `~/bench/logs/rt_bringup2.log`.

Confirms the non-RT fallback: same bring-up, 60 s stable, `tx 1504 = rx 1504`, `watchdog 0`,
`missed_ticks 0`, `max_tick_lateness 137 µs`, freshness `Fresh`, zero RPC probes.

### Fixes between Run 2 and Run 4
- `de8479b` — IOCS deadlock fix, see Run 1 above.
- `7320ed7` — **musl `ENOSYS` on `sched_setscheduler`**: musl's libc stubs
  `sched_setscheduler`/`sched_setaffinity` (they always return `ENOSYS`), so `--rt-priority`
  silently fell back on the edge even though the kernel supports `SCHED_FIFO`. **Fix**: call
  the raw syscalls directly (`SYS_sched_setscheduler`, `SYS_sched_setaffinity`) instead of the
  libc wrappers, bypassing the musl stub.

### Run 4 — 13:55, binary `de8479b`, `--rt-priority 50`, 10 min
Capture `captures/hil-rt4-2026-08-28-…pcapng`, log `~/bench/logs/rt_bringup4.log`.

- **No SCHED_FIFO warning** (syscall fix confirmed working). AR to `Data` in ~1 s, then no
  state change for the whole 10 min run.
- Live sniff at t + 70 s: CPU C-SDU IOxS bytes all `0x80` (output IOPS GOOD at offsets
  5/8/18) — **deadlock resolved**, the CPU's outputs are validated continuously.
- **TIA proof**: watch table after modifying the `%Q` values —
  `%IB0 = %QB0 = 16#12`, `%ID2 = %QD2 = 16#1234_5678`, `%ID6 = %QD6 = 16#8765_4321`
  (mirror + true echo, round-tripped through `IoImage`). Device stayed **green** in the
  topology view; the **diagnostics buffer** showed only the expected "IO device failure -"
  entry at run start (AR establishing) and **no** "User data failure" entry for the rest of
  the run. CPU cycled **STOP (PLC clock 12:19:56) → RUN (12:20:12)**: no device event, no AR
  abort — matches the §6b finding that ProviderState=Stop does not release the AR.
- Stats at 6 min: `tx 11219 / rx 11220`, `watchdog 0`, `missed_ticks 0`,
  `output_publish_deferred 19`, `max_tick_lateness 395 µs`, freshness `Fresh` at every sample
  (the 30 s sampling period missed the 16 s STOP window; the capture itself shows the data
  status `0x25` frames during it).

### What a 1 ms run needs (Plan 7)
Today's 32 ms cycle stays well inside budget (`max_tick_lateness` < 0.4 ms) on a plain
Debian 13 kernel, no RT tuning. A 1 ms update time needs, per the design's Plan 7 scope:
`PREEMPT_RT` kernel on the edge, `isolcpus` + IRQ affinity to keep the RT thread's core free
of other interrupts, `mlockall` to avoid page-fault jitter, and a real jitter measurement
campaign — none of this was exercised in Plan 4's HIL runs. `output_publish_deferred` (19 in
6 min at 32 ms) is application-level double-buffer contention and is harmless at this period;
it should be watched at 1 ms and only needs a lock-free seqlock if it becomes significant
(see `FOLLOWUPS.md`).

## 7. Next steps
Plan 3 (`cm`/AR) and Plan 4 (`rt`, cyclic exchange) are both done. `examples/rt_bringup`
reaches AR state `Data` against the real S7-1500 and holds a full RTC1 exchange (PPM/CPM,
IOPS/IOCS, watchdog) with zero missed ticks and zero watchdog expirations over a 10-minute
HIL run, TIA showing a green device with clean diagnostics and a STOP→RUN cycle producing no
AR event (§6d). Next is **Plan 5 (alarms + I&M/diagnosis)**: ERR-RTA on device stop,
`ProblemIndicator`/diagnosis reporting, and minimal `Read`/`ReadImplicit` support beyond the
PNIORW refusal (index `0xfbff`, I&M reads) — see `FOLLOWUPS.md`. Then **Plan 7 (1 ms
determinism)**: `PREEMPT_RT` kernel, `isolcpus`, IRQ affinity, `mlockall`, and a jitter
measurement campaign at the 1 ms update time.

## Pitfalls
- **Never use CPL/PowerLine** on the segment (HomePlug `0x88e1` → jitter → RT watchdog expires, AR drops).
- p-net **must run on native Linux with L2 access** (the edge) — **not in WSL2** (NATed network).
- `pn_dev` needs `cap_net_raw`/`cap_net_admin` (or root); the storage dir must be an **absolute** path.
- The p-net device has the **rt-labs Vendor/Device ID** (not ours) — expected: we are capturing the
  **structure** of the frames, identical to what our stack will produce.
- The supervision Docker stack on the same edge is a jitter source — fine at 32 ms, not for < 2 ms.
