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

## 6c. HIL — pnio itself as the IO-Device (2026-08-28)

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

## 6d. HIL — cyclic exchange with pnio (2026-08-28)

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
- **Decoded capture** (`captures/hil-rt4-2026-08-28-135506.pcapng`, full 10 min): **18717**
  of our `0x8000` frames, cycle-counter step always **1024**, max inter-frame interval
  **32.4 ms**. CPU STOP window at **t = 277.6–293.5 s** (data status `0x25` throughout),
  then back to `0x35` with no AR abort. Final stats at end of run: **tx 17783 / rx 17784**,
  `watchdog 0`, `missed_ticks 0`, `max_tick_lateness 395 µs`, **zero RPC after setup**. One
  ERR-RTA at **t = 602 s** — the CPU's own watchdog firing after our `SIGINT`, same expected
  teardown pattern as Run 1. Confirms Plan 4 §1's acceptance criteria are all met.

### What a 1 ms run needs (Plan 7)
Today's 32 ms cycle stays well inside budget (`max_tick_lateness` < 0.4 ms) on a plain
Debian 13 kernel, no RT tuning. A 1 ms update time needs, per the design's Plan 7 scope:
`PREEMPT_RT` kernel on the edge, `isolcpus` + IRQ affinity to keep the RT thread's core free
of other interrupts, `mlockall` to avoid page-fault jitter, and a real jitter measurement
campaign — none of this was exercised in Plan 4's HIL runs. `output_publish_deferred` (19 in
6 min at 32 ms) is application-level double-buffer contention and is harmless at this period;
it should be watched at 1 ms and only needs a lock-free seqlock if it becomes significant
(see `FOLLOWUPS.md`).

### 2026-08-28 — Connect with Output CR FrameID 0xFFFF
The S7-1500 (TIA project recreated today, ARProperties `0x40000011`, RTClass 2) sent the
Output CR with `FrameID = 0xFFFF` ("the IO device selects the FrameID"), which `validate()`
rejected as out of `FRAME_ID_RANGE`, keeping the AR stuck in Idle (`Connect: Faulty
IOCRBlockReq`, `Error in Parameter LT`). Fixed by accepting `0xFFFF` on the Output CR and
having the device select `0x8001`, returned in the IOCRBlockRes.

## 6e. HIL — 1 ms on PREEMPT_RT (2026-08-28)

With Plan 7 implemented (`rt::sched`, `eth::bpf`, `rt::hist`, zero-allocation `recv_into`,
`examples/rt_bringup` CSV/verdict, `bench/` campaign scripts), the edge `lab-server` moved
from the stock Debian 13 kernel of §6d (32 ms) to `PREEMPT_RT`, TIA's update time moved to
1 ms, and the device was run through the campaign described in the spec (§11), at idle and
under load.

### Edge configuration

- Kernel: `6.12.105+deb13-rt-amd64` (`/sys/kernel/realtime` = `1`).
- GRUB cmdline (`/etc/default/grub`, spec §5.1):
  ```
  GRUB_CMDLINE_LINUX_DEFAULT="quiet isolcpus=domain,managed_irq,3 nohz_full=3 rcu_nocbs=3 irqaffinity=0-2 intel_idle.max_cstate=1 processor.max_cstate=1 nosoftlockup"
  ```
- Services disabled: Docker/containerd, `wpa_supplicant`, bluetooth, and other periodic-timer
  services (the NAT script creates its own `DOCKER-USER` chain, so Docker itself does not need
  to run).
- `bench/edge-rt-tune.sh` results: governor `performance` on all CPUs; `ethtool -L eno2
  combined 1` succeeded — igb renamed its queue vector from `eno2-TxRx-0` to
  `eno2-rx-0`/`eno2-tx-0`, both pinned to CPU 3 (the misc `eno2` vector to CPUs 0-2), IRQ
  threads set to `SCHED_FIFO 90`; coalescing `rx-usecs`/`tx-usecs` = `0`/`0`; EEE off;
  `gro`/`lro` off; `kernel.sched_rt_runtime_us = -1`, `kernel.timer_migration = 0`,
  `vm.stat_interval = 120`.
- Cache topology (`lscpu`/sysfs): Intel Atom E3940, 4 cores, L1d 24 KiB / L1i 32 KiB private
  per core, **L2 1 MiB unified shared by CPUs 2-3, no L3**.
- `rt_bringup` flags used throughout the campaign: `--iface eno2 --ip 172.16.2.10
  --rt-priority 80 --cpu 3 --app-cpus 0-2 --lock-memory --stats-every 5`, plus
  `--duration`/`--csv` per run (`--app-cpus 0-1` for the discriminating run below).

### Bench facts learned today

(a) **igb renames its IRQ vectors after `ethtool -L combined 1`**: before the call the queue
vector is named `eno2-TxRx-0`; after, it becomes `eno2-rx-0`/`eno2-tx-0` (no more `TxRx`).
`edge-rt-tune.sh` steps 4/5 originally matched on `*TxRx*` and found nothing — the IRQ was left
unpinned, silently. Fixed to match any vector whose name starts with `${PLC_IF}-` and to warn
when the match finds nothing.

(b) **The CPU sent Output CR `FrameID = 0xFFFF`** ("the IO device selects the FrameID") once
today's TIA project was recreated, where yesterday's project had assigned `0x8001` itself;
`cm::connect::validate` rejected it as out of range and the AR stuck in Idle. Fixed (commit
`5c394aa`) to accept `0xFFFF` on the Output CR only and select `0x8001` (unchanged from Plan
4's goldens and BPF ranges), returned in the `IOCRBlockRes`.

(c) **Phantom missed tick and a blind lateness grid, visible only at 1 ms**: the `timerfd` was
armed in `spawn`, before the RT thread's own setup (affinity, `SCHED_FIFO`, `mlockall`, stack
pre-fault). At 32 ms that setup cost is noise; at 1 ms it can exceed a whole period, so the
first `read` reports ≥ 2 expirations and the engine counted a missed tick that never really
happened. The lateness grid also anchored on `start = now` with `ticks = expirations` at that
same first read, which put "expected" a whole period ahead of reality and reported 0 lateness
for the entire run. Fixed (commit `2ce31e2`): the timer is armed inside the thread after setup,
and a dedicated `TickGrid` anchors on the first wake (extra expirations on that first read are
not counted as missed).

(d) **`ulimit -l` is 8 MiB on the edge by default**: `mlockall` needs `cap_ipc_lock` on the
binary (or a raised `RLIMIT_MEMLOCK`) to actually lock the process's pages — documented in
`bench/README.md`.

### Control run — 32 ms (120 s), non-regression

Done by hand before switching TIA to 1 ms, to confirm Tasks 1-5 did not regress Plan 4's
behaviour. The first attempt hit finding (b) above and never reached `Data`; after the fix:

| | |
|---|---|
| Binary | `5c394aa` |
| Flags | `--lock-memory --cpu 3 --app-cpus 0-2 --rt-priority 80 --duration 120` |
| tx = rx | 3714 |
| missed_ticks / watchdog_expirations | 0 / 0 |
| reused / deferred | 0 / 3 |
| memory_locked | yes |
| tick_lateness p50 / p99 / p99.99 / max | 6 / 22 / 60 / 60.9 µs |
| cycle_work max | 61.7 µs |
| rx_interval max | 32.067 ms |
| verdict | FAIL — only on the 1 ms-specific `rx_interval` threshold (1.5 ms), expected at a 32 ms period; the run should have passed `--max-rx-interval-us 40000` |

Non-regression otherwise confirmed: no missed ticks, no watchdog expirations, lateness
comparable to §6d's 32 ms run.

### 1 ms smoke runs (60 s × 2)

- **Smoke #1** (binary `5c394aa`, before the runner fix): `tx=59076 rx=59077`,
  `missed_ticks=1` (the phantom tick of finding (c)), `reused=3 deferred=34` (0.06 %),
  `cycle_work` p50 7 / p99 8 / p99.99 44 / max 63.4 µs, `rx_interval` p50 1000 / p99 1028 /
  p99.99 1069 / max 1082 µs, `tick_lateness` all `0` (the blind grid of finding (c)).
  **VERDICT: FAIL** (`missed_ticks=1`).
- **Smoke #2** (binary `2ce31e2`, after the fix): `tx=59271`, `missed_ticks=0`,
  `watchdog_expirations=0`, `tick_lateness` p50 0 / p99 0 / p99.99 2 / max 10.0 µs,
  `cycle_work` max 60.4 µs, `rx_interval` p99.99 1065 / max 1073.9 µs, `reused=13 deferred=63`
  (0.13 %). **VERDICT: PASS**.

`2ce31e2` is the binary used for the whole campaign below.

### Campaign

Load = `stress-ng --cpu 3 --vm 1 --vm-bytes 512M` pinned to CPUs 0-2 (spec §1) unless noted
otherwise. The "load, no capture" and "load on CPUs 0-1" rows are follow-up runs launched after
the main campaign, to separate `tcpdump`'s own cost from `stress-ng`'s and to test the
L2-cache-sharing hypothesis discussed in the verdict below. Directory:
`captures/plan7-20260828-173511/` (copied from the edge, git-ignored).

| Run | Duration | Missed ticks | Watchdog | Tick lateness p99 / p99.99 / max (µs) | Cycle work p99.99 / max (µs) | RX interval p99.99 / max (µs) | Reused+deferred | Verdict |
|---|---|---|---|---|---|---|---|---|
| `cyclictest` idle | 600 s | — | — | min 5 / avg 8 / **max 95** | — | — | — | rc=0 |
| `cyclictest` + load | 600 s | — | — | min 4 / avg 8 / **max 173** | — | — | — | rc=0 |
| `rt_bringup` idle | 600 s | 0 | 0 | 10 / 48 / 111.0 | 61 / 86.0 | 1072 / 1103.7 | 0.10 % (128+469 / 598320) | PASS |
| `rt_bringup` load + `tcpdump` | 600 s | 0 | 0 | 63 / 147 / 255.4 | 178 / 262.6 | 1126 / 1199.2 | 0.11 % (101+551 / 598181) | FAIL (lateness p99.99 147 µs ≥ 100 µs) |
| `rt_bringup` load, no capture | 600 s | 0 | 0 | 63 / 203 / 283.8 | 84 / 157.6 | 1155 / 1252.3 | 0.12 % (124+600 / 599715) | FAIL (lateness p99.99 203 µs ≥ 100 µs) |
| `rt_bringup` load on CPUs 0-1 | 300 s | 0 | 0 | 18 / 92 / 147.7 | 97 / 154.6 | 1092 / 1155.3 | 0.08 % (31+217 / 298970) | PASS |
| `rt_bringup` load on CPUs 0-1 | 600 s | 0 | 0 | 17 / 86 / 158.4 | 88 / 156.1 | 1087 / 1130.6 | 0.07 % (84+321 / 599663) | PASS |

`cyclictest` reports its own wake-up-latency histogram as min/avg/max, not percentiles — its
figures sit in the same table column for comparison, and its own exit code is a plain 0/1 (no
PASS/FAIL threshold in that tool). The "load on CPUs 0-1" row at 300 s was the first,
discriminating run of the L2-sibling hypothesis; the 600 s row is the confirmation run launched
afterwards (`rt-load-cpu01-600.log`), both kept in the table — the prose below (verdict, README)
cites the 600 s figures as the L2-sibling-free result.

### pcap inter-arrival percentiles (`rt-load.pcapng`, load + `tcpdump` run)

Computed with `tshark.exe` (Windows Wireshark, since the edge has no analysis tooling), two-pass
mode so `frame.time_delta_displayed` measures the delta between *displayed* frames of the
filtered stream:

CPU → device (`0x8001`):
```
"/mnt/c/Program Files/Wireshark/tshark.exe" -2 -r rt-load.pcapng -Y "pn_rt.frame_id == 0x8001" -T fields -e frame.time_delta_displayed \
  | sort -n \
  | awk '{a[NR]=$1} END{n=NR; p=int(n*0.9999); if(p<1)p=1; printf "n=%d p99.99=%.6fs max=%.6fs\n", n, a[p], a[n]}'
```
Device → CPU (`0x8000`):
```
"/mnt/c/Program Files/Wireshark/tshark.exe" -2 -r rt-load.pcapng -Y "pn_rt.frame_id == 0x8000" -T fields -e frame.time_delta_displayed \
  | sort -n \
  | awk '{a[NR]=$1} END{n=NR; p=int(n*0.9999); if(p<1)p=1; printf "n=%d p99.99=%.6fs max=%.6fs\n", n, a[p], a[n]}'
```

| Direction | n | p50 | p99 | p99.99 | max |
|---|---|---|---|---|---|
| CPU → device (`0x8001`) | 598035 | 1000 µs | 1017 µs | 1058 µs | 1116 µs |
| device → CPU (`0x8000`) | 597999 | 1000 µs | 1030 µs | 1156 µs | 1245 µs |

Both stay well inside the 1.5 ms `rx_interval` threshold (criterion 3). `tcpdump` itself
(`taskset -c 0-2 tcpdump -i eno2 -B 65536`, kept off the isolated CPU 3) counted 1231537
packets captured, 0 dropped by the kernel.

### STOP→RUN test and TIA observations

`rt-stoprun2` (180 s, 16:52Z): the user cycled the CPU to STOP roughly 15 s into the run; the
device's `freshness` reported `Stopped` for three consecutive 5 s samples, then back to `Fresh`
on RUN — the AR stayed at `Data` throughout, no abort. Tick lateness during the stop window
peaked at 9.6 µs; whole-run summary: `tx=178642 rx_accepted=178644`, `missed_ticks=0`,
`watchdog_expirations=0`, `tick_lateness` max 64.3 µs, **VERDICT: PASS**. The TIA diagnostic
buffer stayed empty and the device stayed green throughout the whole campaign (confirmed by the
user); the watch table mirrored correctly (`%IB0 == %QB0`, `%ID2 == %QD2`, `%ID6 == %QD6`) at
both 32 ms and 1 ms.

### Verdict per spec §1 criterion

1. `missed_ticks == 0` and `watchdog_expirations == 0`: **met in every run with the final
   binary (`2ce31e2`)** — campaign idle, load + `tcpdump`, load no capture, load on CPUs 0-1
   (300 s and 600 s), smoke #2, and the STOP→RUN run — zero missed ticks and zero watchdog
   expirations across **2.9 million cycles** of 1 ms operation (598320 + 598181 + 599715 +
   298970 + 599663 + 59271 + 178642 = 2 932 762). Smoke #1, on the pre-fix binary `5c394aa`, is
   excluded from that total: it is the run that surfaced the phantom-missed-tick artifact fixed
   in `2ce31e2` (finding (c) above), not a genuine missed cycle.
2. Tick lateness — **2a, p99.99 < 100 µs**: met at idle (48 µs) and with the load kept off the
   L2 sibling (86 µs, 600 s confirmation run); **not met** under the spec's own load on CPUs 0-2
   (147 µs with `tcpdump` capturing, 203 µs without). **2b, max < 300 µs**: met in every run
   (idle 111.0 µs; load + capture 255.4 µs; load, no capture 283.8 µs; load on CPUs 0-1,
   600 s, 158.4 µs).
3. CPU→device inter-arrival max < 1.5 ms: met in every run (idle 1103.7 µs; load + capture
   1199.2 µs; load, no capture 1252.3 µs; load on CPUs 0-1, 600 s, 1130.6 µs).
4. Watch table unchanged, diagnostic buffer clean: met (see STOP→RUN above).

At idle, and with the load kept off the L2-sharing sibling (CPUs 0-1), all four criteria are
met. Under the spec's specified load (`stress-ng` pinned to CPUs 0-2, sharing CPU 3's L2 cache
from CPU 2), criteria 1, 2b, 3 and 4 are met and only criterion 2a (the p99.99 lateness budget)
is missed. **The loop never lost a cycle in 2.9 million (final binary); the p99.99 budget is a
tuning choice on this SoC, not a correctness failure** — `cyclictest`'s own max under load
(173 µs, no crate code involved) tracks the same effect. Recommended edge configuration going
forward: **isolate the whole L2 pair (CPUs 2-3), housekeeping on CPUs 0-1** — the discriminating
runs above (load pinned to 0-1 instead of 0-2, both at 300 s and confirmed at 600 s) already
show this holds the p99.99 budget under load; this is also the next step (Plan 7bis).

### Seqlock decision (spec §9)

`input_snapshot_reused + output_publish_deferred` as a fraction of ticks, at 1 ms: idle 0.10 %,
under the spec's own load (CPUs 0-2) 0.11-0.12 % (0.11 % with `tcpdump`, 0.12 % without), with
the load kept off the L2 sibling 0.07-0.08 % (0.08 % at 300 s, 0.07 % at 600 s). Spec §9's rule
is a hard line: **< 0.1 % → `Mutex` stays; otherwise a seqlock is Plan 7bis.** Under the spec's
own load the campaign measured 0.11-0.12 %, over that line by 0.01-0.02 points.

**Decision: keep the `Mutex` + `try_lock` image anyway — a deliberate deviation from the §9
rule.** Rationale: a deferred publish is retried on the very next tick and costs nothing on the
wire — every campaign run above shows `rx_dropped=0` and `missed_ticks=0` (with the final
binary) regardless of the reused/deferred count, and the overshoot itself is small (0.01-0.02
points over the line). The seqlock stays a FOLLOWUP, to be built only if an application needs to
observe every single cycle's outputs rather than the latest one — that is the concrete trigger
for Plan 7bis, not the raw percentage.

### Lessons

- Match IRQ vector names loosely (a prefix, not one hard-coded legacy name) and warn loudly when
  a match finds nothing — a silent no-op tuning step is worse than a script that fails outright.
- A lateness measurement grid must anchor on the thread's first real wake, after setup — not on
  the timer's arm time — or it reports zero lateness for the whole run.
- `cyclictest`'s own max under load (173 µs) tracks `rt_bringup`'s p99.99 lateness under the same
  load (147-203 µs) — both point at the same L2-sibling cache effect, not at anything
  crate-specific.
- Capturing on the same NIC costs cycle work (+~90 µs at p99.99: 178 µs with `tcpdump` vs 84 µs
  without) but not wake-up latency — the no-capture run's lateness p99.99 (203 µs) is *higher*
  than the with-capture run's (147 µs), so the L2-sibling effect under load is not a capture
  cost.
- Keep the 32 ms control run before any 1 ms campaign: it caught the `FrameID 0xFFFF` regression
  (finding (b)) before burning a 10-minute 1 ms run against a broken AR.

## 6f. HIL — 1 ms with the L2 pair isolated (2026-08-28, Plan 7bis)

§6e closed with a recommendation: isolate the whole L2 pair (CPUs 2-3), housekeeping on CPUs
0-1, to hold the p99.99 lateness budget under the spec's own load. This section reruns the
campaign under that recommendation to confirm it.

### What changed vs §6e

Only the edge's GRUB cmdline / `HK_CPUS`, nothing in the crate or the TIA project. The edge
moved from the single-core profile (`isolcpus=domain,managed_irq,3`, `HK_CPUS=0-2`) to the
L2-pair profile (`isolcpus=domain,managed_irq,2,3`, `HK_CPUS=0-1`) — now `bench/`'s default
(`bench/README.md` documents both profiles). Same binary (`2ce31e2`, md5 `3c92901b`, the fixed
build used throughout §6e), same TIA project (1 ms update time, watchdog factor 3), same
`campaign.sh`/`rt_bringup` scripts, just run with the new default `HK_CPUS`/`--app-cpus`.

### Edge state (campaign `plan7-20260828-201858`)

- Kernel `6.12.105+deb13-rt-amd64` (unchanged from §6e).
- GRUB cmdline: `quiet isolcpus=domain,managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3
  irqaffinity=0-1 intel_idle.max_cstate=1 processor.max_cstate=1 nosoftlockup` — housekeeping
  CPUs 0-1, RT CPU 3, isolated set `2-3` (both cores of the shared L2 pair).
- Cache topology (unchanged Atom E3940): L1d 24 KiB / L1i 32 KiB private to CPU 3; L2 1024 KiB
  unified, `shared_cpu_list=2-3`.
- IRQ pinning: irq 128 (`eno2`, misc) → CPUs 0-1; irq 129 (`eno2-rx-0`) → CPU 3; irq 130
  (`eno2-tx-0`) → CPU 3; IRQ threads (pids 898/899) `SCHED_FIFO` 90.
- NIC tuning (`edge-rt-tune.sh`, unchanged from §6e): EEE off, coalescing `rx-usecs`/`tx-usecs`
  0/0, `gro`/`lro` off, `combined 1`, governor `performance`.
- `kernel.sched_rt_runtime_us = -1`.
- `rt_bringup` flags identical to §6e except `--app-cpus 0-1` (was `0-2`).

### Campaign

Load = `stress-ng --cpu 3 --vm 1 --vm-bytes 512M`, same spec §1 load definition as §6e, now
pinned to the profile's own housekeeping set (CPUs 0-1 instead of 0-2). Directory:
`captures/plan7-20260828-201858/` (copied from the edge, git-ignored).

| Metric | single-core profile (§6e) | L2-pair profile (§6f) |
|---|---|---|
| `cyclictest` idle max | 95 µs | 13 µs |
| `cyclictest` load max | 173 µs | 16 µs |
| `rt_bringup` idle — missed / watchdog | 0 / 0 | 0 / 0 |
| `rt_bringup` idle — tick lateness p99 / p99.99 / max | 10 / 48 / 111.0 µs | 9 / 20 / 22.7 µs |
| `rt_bringup` idle — cycle work p99.99 / max | 61 / 86.0 µs | 49 / 49.5 µs |
| `rt_bringup` idle — rx interval p99.99 / max | 1072 / 1103.7 µs | 1046 / 1089.0 µs |
| `rt_bringup` idle — reused+deferred | 0.10 % (128+469 / 598320) | 0.13 % (104+679 / 600078) |
| `rt_bringup` idle — verdict | PASS | PASS |
| `rt_bringup` load + `tcpdump` — missed / watchdog | 0 / 0 | 0 / 0 |
| `rt_bringup` load + `tcpdump` — tick lateness p99 / p99.99 / max | 63 / 147 / 255.4 µs | 0 / 13 / 22.4 µs |
| `rt_bringup` load + `tcpdump` — cycle work p99.99 / max | 178 / 262.6 µs | 44 / 46.7 µs |
| `rt_bringup` load + `tcpdump` — rx interval p99.99 / max | 1126 / 1199.2 µs | 1049 / 1084.9 µs |
| `rt_bringup` load + `tcpdump` — reused+deferred | 0.11 % (101+551 / 598181) | 0.15 % (147+731 / 597969) |
| `rt_bringup` load + `tcpdump` — verdict | FAIL (lateness p99.99 147 µs ≥ 100 µs) | PASS |

`tcpdump` itself (`taskset -c 0-1 tcpdump -i eno2 -B 65536`, kept off the isolated CPUs 2-3)
counted 1194745 packets captured, 0 dropped by the kernel, over the load run.

### pcap inter-arrival percentiles, §6e vs §6f (load + `tcpdump` run)

Computed the same way as §6e (`tshark.exe -2`, two-pass mode, `frame.time_delta_displayed` on
the filtered stream):

| Profile | Direction | n | p50 | p99 | p99.99 | max |
|---|---|---|---|---|---|---|
| single-core (§6e) | CPU → device (`0x8001`) | 598035 | 1000 µs | 1017 µs | 1058 µs | 1116 µs |
| single-core (§6e) | device → CPU (`0x8000`) | 597999 | 1000 µs | 1030 µs | 1156 µs | 1245 µs |
| L2-pair (§6f) | CPU → device (`0x8001`) | 597317 | 1000 µs | 1012 µs | 1049 µs | 1073 µs |
| L2-pair (§6f) | device → CPU (`0x8000`) | 597287 | 1000 µs | 1001 µs | 1021 µs | 1031 µs |

Both directions stay well inside the 1.5 ms `rx_interval` threshold (criterion 3) — and tighter
than §6e's single-core-profile figures in both directions.

### Verdict per spec §1 criterion — L2-pair profile

1. `missed_ticks == 0` and `watchdog_expirations == 0`: met — idle (600078 ticks) and load +
   `tcpdump` (597969 ticks), zero missed ticks and zero watchdog expirations in both, same final
   binary `2ce31e2` as §6e.
2. Tick lateness — **2a, p99.99 < 100 µs**: met at idle (20 µs) and under the spec's own load,
   now pinned to the profile's housekeeping CPUs (13 µs) — both comfortably inside budget, unlike
   §6e's single-core-profile load runs (147-203 µs). **2b, max < 300 µs**: met in both (idle
   22.7 µs; load 22.4 µs).
3. CPU→device inter-arrival max < 1.5 ms: met — `rt_bringup`'s own `rx_interval` max is 1089.0 µs
   (idle) / 1084.9 µs (load); the pcap-derived `0x8001` max is 1073 µs (load, table above).
4. Watch table unchanged, diagnostic buffer clean: carried over from §6e — this campaign changed
   only the GRUB cmdline / CPU pinning, not the binary or the TIA project, so the AR/watch-table/
   diagnostics behaviour already confirmed on this exact binary and TIA project by §6e's
   STOP→RUN test is unaffected; not independently re-exercised in this campaign.

**All four spec §1 criteria are met, at idle and under the spec's own load, with the L2-pair
profile.** This settles §6e's open point: the p99.99 lateness budget the single-core profile
missed under load is a tuning choice for this SoC, not a correctness limit — isolating the whole
L2 pair (CPUs 2-3) removes it.

### Seqlock decision (spec §9) — updated with §6f

`input_snapshot_reused + output_publish_deferred` as a fraction of ticks, L2-pair profile: idle
0.13 % (104+679 / 600078), under the spec's own load (now pinned to CPUs 0-1) 0.15 %
(147+731 / 597969) — both still over spec §9's 0.1 % line, by a slightly wider margin than §6e's
single-core-profile figures (0.10 % idle, 0.11-0.12 % load). `rx_dropped=0` in both runs here
(and `tcpdump` itself counted 0 dropped packets) — **the §9 deviation recorded in §6e stands**:
`Mutex` + `try_lock` stays, the overshoot does not translate into a dropped frame or a missed
tick either way, and the seqlock stays a FOLLOWUP triggered by a consumer needing every single
cycle's output, not by the raw percentage.

### Lessons

- The L2 sibling was the whole story: `cyclictest`'s own max under load fell from 173 µs (§6e,
  single-core profile, load on CPUs 0-2) to 16 µs once CPU 2 was pulled off housekeeping;
  `rt_bringup`'s tick-lateness p99.99 under load followed the same pattern, from 203 µs (§6e's
  load-no-capture run) to 13 µs here.
- `tcpdump`'s own cycle-work cost also shrank once the capture ran on CPUs 0-1 instead of 0-2
  (cycle_work p99.99 178 µs → 44 µs) — observed, not fully explained: a plausible reason is that
  the capture process no longer contends with the RT thread for CPU 3's shared L2 the way it did
  when pinned to CPU 2, but this specific mechanism was not isolated further.
- Keep the L2-pair profile (`isolcpus=domain,managed_irq,2,3`, `HK_CPUS=0-1`) as the default edge
  configuration; the single-core profile (`isolcpus=domain,managed_irq,3`, `HK_CPUS=0-2`) stays
  documented in `bench/README.md` for setups that need a third housekeeping core and can tolerate
  the wider p99.99 budget under load.

## 6g. HIL — our own GSDML and the typed API (2026-08-29, Plan 6)

With Plan 6 implemented (`config`, `gsdml`, `api`), `examples/typed_bringup` replaces
`rt_bringup` as the peer facing the CPU, and — for the first time — the GSDML imported into TIA
is our own, generated by `examples/gen_gsdml` from a `DeviceConfig`, not the rt-labs reference
file borrowed for §6b-§6f. Station `pnio-dev`, identity `0xFFFF`/`0x0001`, 16 `Real` + 32 `Bool`
per direction (slots 1-4).

### Setup

Same edge (`lab-server`), same L2-pair profile as §6f — kernel `6.12.105+deb13-rt-amd64`,
`isolcpus=domain,managed_irq,2,3`, `HK_CPUS=0-1` — unchanged since Plan 7bis. Binary: a musl
build of `typed_bringup` from commit `43da37c` (md5 `d8f0a89e`), `setcap
cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock+eip` (same re-apply-after-`scp` requirement
as §6c/§6d). Every run below was launched from `~/bench` on the edge with the same command line,
only `--duration`/`--stats-every`/`--csv` varying per run:

```bash
./typed_bringup --iface eno2 --ip 172.16.2.10 --station pnio-dev --rt-priority 80 --cpu 3 \
  --app-cpus 0-1 --lock-memory --duration <N> --stats-every <S> --csv logs/<name>.csv
```

| Run | `<name>` | `<N>` (s) | `<S>` (s) |
|---|---|---|---|
| Smoke | `plan6-smoke` | 60 | 10 |
| 1 ms campaign | `plan6-1ms` | 600 | 5 |
| STOP→RUN attempt #1 (no STOP happened) | `plan6-stoprun` | 180 | 5 |
| STOP→RUN attempt #2 | `plan6-stoprun2` | 240 | 5 |

Every run below reports `memory_locked=yes` (`--lock-memory` took effect) and ran at the CPU's
actual send clock, 1 ms.

### GSDML import path

The GSDML that was ultimately accepted, `GSDML-V2.4-CoreEngineering-pnio-20260829.xml`
(`MinDeviceInterval="16"`), took three TIA rejections to get there:

1. **XSD validation failed** (TIA V21's own installed XSD, `docs/gsdml.md`'s recipe): the DAP
   was missing two attributes the v2.4 XSD requires, `CheckDeviceID_Allowed` and
   `NameOfStationNotTransferable`. Fix: declare both (`"true"`/`"false"`, matching the rt-labs
   reference file TIA had already accepted).
2. **TIA's GSD checker (beyond the XSD) rejected `PNIO_Version="V2.4"`**: for
   `PNIO_Version >= "V2.31"` it mandates `CertificationInfo` at the DAP,
   `LLDP_NoD_Supported="true"`, `ResetToFactoryModes="2"`, `PTP_BoundarySupported="true"` and
   `DCP_BoundarySupported="true"` (checks `0x00020020_0/5/6/10/11`) — none of which this device
   implements. Fix: declare `PNIO_Version="V2.3"`, the last version without those mandates,
   still allowing `StartupMode="Advanced"`.
3. **TIA's compile step rejected the module sizes**: *"The amount of input data (including user
   data qualifier) of 75 bytes exceeds the maximum permitted data amount of 68 bytes"* (same
   wording for output; `IOConfigData`'s `MaxInputLength`/`MaxOutputLength` were declared as the
   plain per-direction data sum, `64 + 4 = 68`, while TIA counts the Input/Output CR C-SDU
   length, IOPS/IOCS bytes included). Fix (`43da37c`): render `MaxInputLength`/`MaxOutputLength`
   as `DeviceConfig::input_cr_len`/`output_cr_len` — `75`/`75` for this config —
   `MaxDataLength="150"` their sum.

After fix 3 the file validated against the XSD; handed to the user for re-import. **Import
succeeded after an uninstall + reinstall** of the earlier, same-named file (`Options → Manage
GSD files` doesn't pick up new content under an unchanged file name — see
[`docs/gsdml.md`](gsdml.md#importing-in-tia)). The CPU sends no `Write` (parameter records) for
this device: the AR goes straight `Connect → PrmEnd → AppReady → Data`.

### Device view addresses vs computed

| Slot | Module | Device view | Computed (`gen_gsdml`) |
|---|---|---|---|
| 1 | `in1_1` | `I 0..63` | `%IB0..63` |
| 2 | `in2_1` | `I 64..67` | `%IB64..67` |
| 3 | `out3_1` | `Q 0..63` | `%QB0..63` |
| 4 | `out4_1` | `Q 64..67` | `%QB64..67` |

Exact match — the device view TIA assigned equals the address map `gen_gsdml` prints.

### Runs

All four runs used the command line in [Setup](#setup) above, only `<N>`/`<S>`/`<name>`
varying per the table there. Same summary format as §6e/§6f (`typed_bringup`'s own
`--csv`/verdict banner); L2-pair profile throughout, zero missed ticks and zero watchdog
expirations in every run.

| Run | `<name>` | Duration | Missed / watchdog | Tick lateness p99 / p99.99 / max (µs) | Cycle work p99.99 / max (µs) | RX interval p99.99 / max (µs) | Reused+deferred | Verdict |
|---|---|---|---|---|---|---|---|---|
| Smoke | `plan6-smoke` | 60 s | 0 / 0 | 0 / 0 / 4.6 | 19 / 37.8 | 1052 / 1066.1 | 0.12 % (21+52 / 59761) | PASS |
| 1 ms campaign | `plan6-1ms` | 600 s | 0 / 0 | 2 / 29 / 61.1 | 54 / 76.0 | 1054 / 1087.5 | 0.11 % (158+522 / 599737) | PASS |
| STOP→RUN attempt #1 (no STOP — user away) | `plan6-stoprun` | 180 s | 0 / 0 | 5 / 31 / 45.3 | 62 / 99.2 | 1055 / 1075.6 | 0.11 % (55+138 / 179627) | PASS |
| STOP→RUN attempt #2 | `plan6-stoprun2` | 240 s | 0 / 0 | 0 / 26 / 41.5 | 48 / 76.9 | 1052 / 1074.5 | 0.11 % (65+200 / 238499) | PASS |

`typed_bringup summary` lines: smoke `tx=59761`; 1 ms campaign `tx=599737 rx_accepted=599737`;
STOP→RUN #1 `tx=179627`; STOP→RUN #2 `tx=238499 rx_accepted=238501`. `rx_dropped=0` in every run.

### Watch table (during the 600 s run)

| Write | Read back |
|---|---|
| `%QD0 := 16#3F80_0000` (1.0) | `%ID0 = 16#3F80_0000` |
| `%QD60 := 16#C020_0000` (−2.5) | `%ID60 = 16#C020_0000` |
| `%Q64.0 := TRUE` | `%I64.0 = TRUE`, `%IB64 = 16#01` |
| `%Q67.7 := TRUE` | `%I67.7 = TRUE`, `%IB67 = 16#80` |

Typed round trip (REAL big-endian, BOOL LSB-first) verified on our own GSDML — same encoding
`docs/gsdml.md`'s layout rule and the `q-bits` capture (§6b) predict.

### STOP→RUN

Attempt #1 (`plan6-stoprun`, 180 s): the user was away and no STOP happened; the run still
completed and passed on its own merits. Attempt #2 (`plan6-stoprun2`, 240 s): the CPU went to
STOP between 25 s and 30 s into the run (the 5 s stats samples bracket it: last `Fresh` at
11:09:27, first `Stopped` at 11:09:32); the device reported `Freshness::Stopped` for three
consecutive 5 s stats samples, then back to `Fresh` on RUN. The AR stayed at `Data` the whole
time — one AR for the entire run, no abort — matching the ProviderState=Stop behavior first
observed in §6b. Diagnostic buffer: not recorded for this run.

### Verdict per spec §1 (HIL acceptance criterion)

1. Our GSDML imported in TIA: **met** (after the three fixes above and the uninstall/reinstall).
2. `typed_bringup` reaches `Data`, TIA addresses equal the computed ones: **met** (device view
   table above).
3. Typed round-trips verified in the watch table (`REAL` 1.0 and −2.5, `%Q64.0`, `%Q67.7`):
   **met** (watch table above).
4. 10-minute run at 1 ms with `VERDICT: PASS` (Plan 7 thresholds, L2-pair profile): **met** — the
   600 s campaign run above (`tick_lateness` p99.99 29 µs, max 61.1 µs; 0 missed ticks, 0
   watchdog expirations over 599 737 cycles).

**Bonus, non-blocking, 500 µs: met, on X1.** On X2 (the device-facing segment, also the TIA
NAT leg) TIA's update-time list stopped at 1 ms: the 1515-2 PN's X2 port has a fixed 1 ms send
clock (RT class only; 250/500 µs and IRT are **X1**-only). The user then moved the device cable
from X2 to X1, gave X1 the same `172.16.2.100/24` address (X2 moved to another subnet), set the
X1 send clock to 0.5 ms and the device update time to 0.5 ms — nothing changed on the edge, the
device or the GSDML (`MinDeviceInterval="16"` was already declared and accepted). Same
`typed_bringup` binary and flags, `--duration 60` then `300`:

| Run (500 µs, X1) | Cycles | Missed / WD / dropped | Tick lateness p99 / p99.99 / max | Cycle work p99.99 / max | RX interval p50 / p99.99 / max | Verdict |
|---|---|---|---|---|---|---|
| `plan6-x1-smoke` (60 s) | 115 224 | 0 / 0 / 0 | 8 / 30 / 42.1 µs | 35 / 58.5 µs | 500 / 525 / 592.8 µs | PASS |
| `plan6-x1-500us` (300 s) | 596 899 | 0 / 0 / 0 | 11 / 27 / 46.3 µs | 56 / 85.0 µs | 500 / 531 / 551.8 µs | PASS |

At 500 µs the consumer watchdog is 1.5 ms; the worst CPU→device interval seen (551.8 µs) and
the worst tick lateness (46.3 µs) leave the same order of headroom as at 1 ms. The application
loop still sleeps 1 ms, so it refreshes our inputs every second cycle — fine for a mirror, and
the reason `reused`/`deferred` stay at the 1 ms level.

### Lessons

- **XSD validation is necessary, not sufficient.** TIA's own GSD checker applies further,
  version-dependent rules the XSD says nothing about (the `PNIO_Version` mandates below) — a
  file that validates cleanly can still be rejected at import or compile time.
- **Declare only what the device implements.** Claiming `PNIO_Version >= "V2.31"` (even just to
  match the reference file's `"V2.4"`) drags in mandatory `LLDP_NoD_Supported`/
  `PTP_BoundarySupported`/`DCP_BoundarySupported`/`ResetToFactoryModes`/`CertificationInfo`
  claims the device can't back up; `"V2.3"` is the honest ceiling until that support lands.
- **TIA counts the IOPS/IOCS bytes, not the plain module data sum**, in `IOConfigData`'s
  `MaxInputLength`/`MaxOutputLength` — a GSDML declaring the plain sum (68 here) undercounts and
  is rejected at compile time with an exact byte-count message, even though it's XSD-valid.
- **A same-named GSD file isn't re-read on a plain re-import.** Whenever the declaration changes
  but `file_name()` doesn't (same station/vendor/product-family/date), uninstall the old GSD
  before installing the new one.
- **The controller's own interface can cap the achievable send clock below what the GSDML
  declares.** The 1515-2 PN's X2 port is RT-only, fixed at 1 ms; the GSDML's `MinDeviceInterval`
  is necessary but not sufficient to get a shorter cycle — the CPU's physical port matters too.
  On X1 the same device, binary and GSDML ran at 500 µs (0 missed ticks over 596 899 cycles).

## 6h. HIL — robustness campaign at 500 µs (2026-08-29/30)

After §6g, a campaign designed not to prove the cycle again but to see what breaks it: an
application-level latency loop through a cyclic OB, PLC CPU load in three flavours, a link
loss, a 12 h 51 soak, two unmanaged switches in line, broadcast storms and a DCP storm. CPU X1
port (100 Mbit, send clock 500 µs), same edge and L2-pair profile as §6g. Raw numbers, logs
and captures: `captures/robustness-20260829/` (git-ignored, `notes.md` is the ledger).

### Setup

- **Edge side:** `examples/latency_probe` (this campaign's tool, merged with it). Every
  `--period-us 500` iteration it publishes a free-running `u32` counter as the bit pattern of
  slot 1 `REAL 0` and reads slot 3 `REAL 0` (the echo) and `REAL 1` (the OB's own counter) from
  one consistent snapshot. Two histograms, in probe cycles: **`echo_age`** — our counter minus
  the echoed one, i.e. the complete edge → IO → OB pickup → IO → edge loop — and
  **`ob_period`** — iterations between two changes of the OB counter. Anomaly counters: echo
  frozen > 50 ms (`stalls`), echo going backwards, OB counter advancing by ≥ 2 between two
  sightings (`ob_jumps`, an OB output the wire never carried). Same command line throughout:

  ```bash
  ./latency_probe --iface eno2 --ip 172.16.2.10 --rt-priority 80 --cpu 3 --app-cpus 0-1 \
    --lock-memory --period-us 500 --duration <N> --stats-every <S> > logs/<name>.log
  ```

- **PLC side** (sources in the `test-program` deliverable, `plc-code/PLC_BENCH`, not in this
  repo): global DB `BenchData`; cyclic OB `LatencyEcho` (**5000 µs, priority 8**, process image
  partition **PIP 1** assigned on the device's input and output modules) doing
  `devOut_Echo := devIn_Counter; ob30Count += 1; devOut_Ob30 := ob30Count` then an optional
  `FOR` load loop (`load30Iter`, ≈ 12 µs per iteration, SQRT/SIN in SCL); cyclic OB
  `Preemptor` (**1000 µs, priority 15**, `load31Iter` loop only); OB1 with a `loadIter` loop and
  a slow self-check. Tag table `pnioDev` mirrors the 16 `REAL` + 32 `BOOL` per direction of the
  §6g GSDML. The PIP assignment lives on the module I/O addresses, not on the OB — see Lessons.

- **Load generators** (`bench/`): `bcast_storm.py` — unprivileged UDP datagrams to the subnet
  broadcast address (each is an Ethernet `ff:ff:ff:ff:ff:ff` frame the switch floods to the
  CPU's port), 1514-byte frames paced to a wire bit rate, catch-up burst capped at 2 ms;
  `dcp_storm.py` — raw DCP Identify-All requests at a paced rate (needs `cap_net_raw`, run
  through a `python3` copy carrying it). Both sent from the edge's `eno2`, through the switch.
  `parse_rt.py` — RT FrameID gap finder on raw pcap/pcapng bytes (the PN dissector gives up on
  truncated snaplens).

### Runs

| # | Run | Condition | Duration | `echo_age` p50 / p99 / p99.99 / max (ms) | `ob_period` max (ms) | stalls / jumps | Device watchdog / missed ticks |
|---|---|---|---|---|---|---|---|
| 1 | Baseline | direct cable, all loads 0 | 518 s (1 035 810 samples) | 3.00 / 5.00 / 5.50 / 5.50 | 5.00 | 0 / 0 | 0 / 0 |
| 2 | OB1 load ramp | `loadIter` 1000 → 8000 (OB1 cycle 11.7 → 94.5 ms) | ~10 min | 3.00 / 5.00 / 5.50 / 5.50 | 5.00 | 0 / 0 | 0 / 0 |
| 3 | OB30 self-load | `load30Iter` 0 → 200 (OB exec ≈ 2.3 ms) | 396 s | p50 3 → 4, max 5.50 → **7.50** | 5.50 | 0 / 0 | 0 / 0 |
| 4 | OB31 priority steal | `load31Iter` 0 → 60 (≈ 24 → 72 % of CPU time at prio 15) | 362 s | 3.00 / 5.00 / 5.50 / 5.50 | 5.50 | 0 / 0 | 0 / 0 |
| 5 | Link loss | X1 cable pulled ≈ 13 s | 300 s | — | — | 0 after recovery / 1 | 1 (the loss) / 0 |
| 6 | **Soak** | direct, loads 0 | **46 266 s** (79 655 343 samples) | **3.00 / 5.00 / 5.00 / 5.50** | **5.00** | **0 / 0** | **0 / 0** |
| 7 | GS105 quiet | Netgear GS105 in line | 300 s | 3.00 / 5.00 / 5.00 / 5.50 | 5.00 | 0 / 0 | 0 / 0 |
| 8 | GS105 + broadcast 10 Mbit/s | 813 pps | 300 s | 3.00 / 5.00 / 5.00 / 5.50 | 5.00 | 0 / 0 | 0 / 0 |
| 9 | GS105 + broadcast 40 Mbit/s | 3250 pps | 300 s | 3.00 / 5.00 / 5.50 / 6.00 | 5.00 | 0 / 0 | 0 / 0 |
| 10 | **GS105 + broadcast 80 Mbit/s** | 6502 pps, two runs | 2 × 300 s | 3.00 / 5.00 / 5.5-8.0 / **18-19** | 18-19 | 0 / 4 | **3 then 4 AR aborts** / 0 |
| 11 | DGS-1008P + broadcast 40 Mbit/s | D-Link DGS-1008P in line | 300 s | 3.00 / 5.00 / 5.50 / 5.50 | 5.00 | 0 / 1 | 0 / 0 |
| 12 | **DGS-1008P + broadcast 80 Mbit/s** | 6502 pps | 300 s | **3.00 / 5.00 / 5.50 / 5.50** | 5.00 | 0 / 1 | **0** / 0 |
| 13 | DCP Identify-All 1000/s | DGS-1008P | 300 s | 3.00 / 5.00 / 5.00 / 5.50 | 5.00 | 0 / 0 | 0 / 0 |
| 14 | DCP Identify-All 5000/s | DGS-1008P | 300 s | 3.00 / 5.00 / 5.50 / 5.50 | 5.00 | 0 / 0 | 0 / 0 |

Device-side stats over the soak (run 6): `tx=92 530 954 rx_accepted=92 531 099`,
`rx_ignored/dropped/invalid/reordered=0`, `max_tick_lateness` 36.6 µs, `max_cycle_work`
117.9 µs, `max_rx_interval` 588 µs. Across all 14 runs the device itself never dropped,
ignored or reordered a frame and never missed a tick; the only watchdog expirations are the
link loss and the seven GS105 aborts, all initiated on the wire, not by the stack.

### What the loop measures

`echo_age` is dominated by the OB period, not by transport: the OB samples the input image
every 5 ms, so a counter written just after a pickup waits up to 5 ms; add one 500 µs cycle each
way and the hard bound is 5.5 ms, the median 3 ms. The transport itself is the §6g figure
(`rx_interval` max ≈ 0.6 ms). The 12 h 51 soak never exceeded that bound (p99.99 = 5.00 ms,
max = 5.50 ms over 79.7 M samples, `ob_period` = 5.00 ms at every percentile including max).

### PLC CPU load (runs 2-4)

OB1 load up to a 94.5 ms cycle (OB80 trips at 150 ms) has **zero** effect on either the 5 ms OB
or the 500 µs exchange — the PN interface and cyclic OBs run above OB1. Loading the echo OB
itself shifts the loop by exactly its execution time (`max` 5.5 → 7.5 ms at ≈ 2.3 ms of load:
the echo is written at OB start but the output image ships at OB end), which is the relation
one would design for. A higher-priority 1 ms OB stealing up to ≈ 72 % of the CPU does **not**
move the loop at 0.5 ms resolution: `LatencyEcho` is µs-light, so being preempted costs it
nothing visible. Only the echo OB's own work moves the loop.

### Link loss (run 5)

Cable pulled ≈ 13 s (15.07 s of wire silence). The device aborted on its RT watchdog (AR
`Data → Idle`) and sat in `Idle`. Once the link came back: CPU DCP Identify at t₀, our
`Ident Ok` **+84 µs**, `Connect` request +2.9 ms, response +3.2 ms, first cyclic CPU → device
frame at **t₀ + 7.4 ms**, probe `Fresh` again, `stalls=0` afterwards, one `ob_jump` (the OB
kept counting through the outage — expected). Recovery is bounded by the CPU's re-probe, not by
the stack. The same probe also flags a CPU in STOP as `freshness=Stopped` while the AR stays in
`Data` (seen when the loads were reset with the CPU stopped).

### Switches and broadcast storms (runs 7-12)

Both unmanaged gigabit switches are transparent at rest (run 7 = baseline). Under broadcast
load the two behave differently, and the capture says why. At **80 Mbit/s through the GS105**
the AR aborted 3 then 4 times in 240 s (recovering in ≈ 1.5 s each time, the CPU's DCP re-probe
delay). `storm80-rt.pcapng` (RT frames only, `parse_rt.py`): the CPU's frames reached the edge
every 500 µs up to the very last one; **the CPU stopped first**, we kept sending for
1.7-2.0 ms (watchdog + processing), then the CPU sent an `ERR-RTA-PDU` (FrameID `0xFE01`):
`ErrorCode 0xCF / ErrorDecode 0x81 / ErrorCode1 253 RTA_ERR_CLS_PROTOCOL / ErrorCode2 5 "AR
consumer DHT/WDT expired"`. The CPU's diagnostic buffer logs the same event as "IO device
failure – Watchdog time expired" (coding `81 81 FD 05`). So the CPU did not receive our frames
within its 3 × 500 µs watchdog even though they left the edge 0.48-0.52 ms apart: they queued
behind broadcasts in the switch's egress queue toward the **100 Mbit X1 port**. At 80 Mbit/s
that port is ρ ≈ 0.8 busy — mean queue ≈ 4 × 121 µs frames, rare tails > 1.5 ms — while at
40 Mbit/s (ρ = 0.4) the tail never reaches the watchdog: 0 events in run 9, consistent. Both
directions carry the RT VLAN tag with priority 6 (`TCI 0xC000`, verified in the capture), so
the GS105 simply does not enforce 802.1p on them. The **DGS-1008P does**: at 80 Mbit/s it is
indistinguishable from the baseline (run 12), the RT frames overtake the storm. The stack
itself was never the bottleneck: 0 dropped/ignored frames, 0 missed ticks, tick lateness max
276 µs and cycle work max 408 µs under the 80 Mbit/s storm (the `send` path shares the NIC
queue with the storm generator), well inside the 500 µs budget.

### DCP storm (runs 13-14)

Identify-All at 1000/s and 5000/s from the edge, DGS-1008P in line: the RT exchange is
untouched (0 watchdog, `echo_age` max 5.50 ms, 0 `ob_jumps`); tick lateness max rose from
≈ 60 µs to 319 µs at 5000/s because `eno2`'s interrupt vectors sit on the RT core and now
carry 5000 pps of requests plus the CPU's responses — still 0 missed ticks. The CPU answered
239 979 / 239 979 requests at 1000/s and 1 199 820 / 1 199 822 at 5000/s. **Our own DCP
responder was not exercised:** a storm emitted on `eno2` reaches our sockets only as
`PACKET_OUTGOING`, which `recv_into` discards by design; we answered exactly the one genuine
Identify the CPU sent. Our acyclic socket did ingest the CPU's 1.2 M Identify responses without
effect. Loading our responder on HIL needs a second host on the segment — open follow-up.

### Lessons

- **The application loop is bounded by the OB period, not the transport.** For an edge ↔ PLC
  loop the number that matters is OB period + 2 cycles (5.5 ms here); shortening the cycle
  below the OB period buys freshness, not latency.
- **A cyclic OB's PIP assignment lives on the module I/O addresses, not on the OB.** A TIA
  source re-import can silently drop it (echo dead, OB counter still running); re-attach it on
  the module (`I/O addresses → Organization block / Process image`). Likewise OB events lost on
  source import must be re-attached, and OBs written as SCL sources cannot carry `VAR_TEMP` —
  loop indexes go in a DB.
- **An unmanaged switch is only transparent if it honours 802.1p.** With a 100 Mbit port in the
  path, non-RT load above ≈ 50 % of that port turns queueing tails into controller watchdog
  aborts unless the switch prioritises the RT frames (DGS-1008P yes, GS105 no). Use a
  PROFINET-conformant / strict-802.1p switch when RT shares a slow port with anything else.
- **Attribute an abort from the wire before touching code.** "RT consumer watchdog expired" on
  our side looked like our bug; the capture showed the CPU stopped first and its RTA said why.
  Truncated captures (`-s 96`) break the PN dissector — parse the FrameID from raw bytes.
- **A storm sent from the device's own interface does not load the device's receivers.** Any
  test of *our* acyclic responders needs a second host.

## 6i. HIL — alarms, diagnosis and I&M (Plan 5)

Procedure for the Plan 5 acceptance criteria (spec §6, `docs/superpowers/specs/2026-08-30-pnio-alarm-diag-im-design.md`):
the six checks below, to be run once against a real S7-1500 with our own GSDML. This section is
the checklist and the exact commands only — **results are filled in after the HIL session**, in
the empty table at the end.

### Setup

- TIA project `PLC_BENCH` with device `pnio-dev` restored (§6g/§6h's project); the GSDML
  regenerated by `cargo run --example gen_gsdml` re-imported after **uninstalling** the previously
  installed one first (`Options → Manage GSD files` does not pick up new content under an
  unchanged file name — see [`docs/gsdml.md`](gsdml.md#importing-in-tia)) — the Plan 5 GSDML adds
  `Writeable_IM_Records="1 2 3"` and the I&M0 `ModuleInfo`, both new content under the same file
  name if the station/vendor/product-family/date haven't changed.
- A musl build of `examples/typed_bringup` from this branch (`feat/alarm-diag-im`), `scp`'d to
  `~/bench` on the edge, then:
  ```bash
  sudo setcap cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock+eip ~/bench/typed_bringup
  ```
  (capabilities do not survive `scp`, same requirement as every prior HIL run — §6c/§6d/§6g).
- Same L2-pair profile as §6f/§6g/§6h (`isolcpus=domain,managed_irq,2,3`, `HK_CPUS=0-1`), X1 or
  X2 per the run's target update time.

### Checklist

1. **Diagnosis in/out.** Start the device with a diagnosis raised from the command line and clear
   it on shutdown:
   ```bash
   ./typed_bringup --iface eno2 --ip 172.16.2.10 --station pnio-dev --rt-priority 80 --cpu 3 \
     --app-cpus 0-1 --lock-memory --diag 1:0:line-break --im-store ~/bench/im.bin \
     --duration 600 --stats-every 5 --csv logs/plan5-diag.csv
   ```
   Capture the run in parallel:
   ```bash
   ~/bench/capture.sh plan5-diag 60
   ```
   In TIA: *Online & diagnostics → Diagnostic buffer* shows an entry reading "diagnostic
   entering, line break, channel 0, slot 1" at startup and the device turns red in the project
   tree/topology view; SIGINT the process (Ctrl-C) before it reaches `--duration` and the
   diagnostic buffer gains a matching "outgoing" entry, device back to green. In the capture,
   check the `ProblemIndicator` bit lands in the cyclic data status the way
   `crate::rt::frame::DataStatus::RUN_PRIMARY_VALID_PROBLEM` (`crates/pnio/src/rt/frame.rs`)
   encodes it (`0x15` while the diagnosis is active, `0x35` (`RUN_PRIMARY_VALID_OK`) once it
   clears):
   ```bash
   "/mnt/c/Program Files/Wireshark/tshark.exe" -r captures/plan5-diag-<ts>.pcapng \
     -Y "pn_rt.frame_id == 0x8000" -T fields -e pn_rt.data_status
   ```
2. **I&M1-3 write + persistence.** With the device running (any of the runs below, `--im-store`
   set), in TIA: *Online & diagnostics → General → Identification & Maintenance* → set *Plant
   designation* and *Location*, download. Check the capture for a `Write` request/response on
   index `0xAFF1` (Plant designation → `IM_Tag_Function`) answering OK. Stop `typed_bringup`,
   restart it with the **same** `--im-store ~/bench/im.bin` path, and confirm TIA's *Online &
   diagnostics* page still shows the values written before the restart (a `Read` on `0xAFF1` in
   the new run's capture returns them).
3. **I&M0 read.** In TIA: *Online & diagnostics* on the DAP, the interface submodule and each
   plugged module → the identity fields shown (order number, serial number, hardware/software
   revision) equal the `Im0` values the `DeviceConfig` builder declared (`gen_gsdml.rs`'s
   `sample_config`, or the config `typed_bringup` starts with). Confirm in the capture that
   `0xAFF0` answers OK on the DAP, on **both** interface subslots (`0/0x8000` and `0/0x8001` —
   §4.4 of the spec notes TIA reads both), and on each module subslot.
   Also **confirm what TIA does with `IM_Supported` on the interface submodule, and record it**:
   we answer `0x000E` there because p-net does (golden `im0_read_res_if`), but TIA's own reaction
   to a non-DAP submodule claiming I&M1-3 is untested — does it offer the *Identification &
   Maintenance* tab on the interface submodule too, and does a Write to `0xAFF1` on `0/0x8000`
   behave? Note the answer in the results table; it decides whether the "same mask everywhere"
   ruling stands or has to become DAP-only after all.
4. **Device stop.** SIGINT `typed_bringup` mid-run (no `--diag` needed for this check). In the
   capture, confirm an ERR-RTA frame (`FrameID 0xFE01`) leaves the device before the socket
   closes; in TIA, the diagnostic buffer logs "IO device failure" (or equivalent CPU wording) with
   a timestamp within ~10 ms of the ERR-RTA's capture timestamp — not the ~1.5-3× watchdog-interval
   delay a silent disappearance would cost (compare against run 5's link-loss recovery numbers in
   §6h, which is what "no ERR-RTA" looks like).
5. **Replay on reconnect.** With a diagnosis active (`--diag` set, as in check 1), force the CPU
   to lose and regain the AR — either a TIA STOP→RUN cycle (as in §6g's STOP→RUN test) or an X1
   cable pull/replug (as in §6h run 5) — and confirm the diagnosis reappears in the CPU (same
   diagnostic-buffer entry as check 1, device red again) without restarting `typed_bringup`.
6. **RT non-regression.** The check-1 run *is* this check: 10 minutes at 1 ms (or 500 µs, per
   the target update time) with an active diagnosis for the whole run, `typed_bringup`'s own
   verdict banner reading `VERDICT: PASS` and 0 missed ticks — same thresholds and CSV/histogram
   format as §6e/§6f/§6g/§6h.

### Results

| Check | Expected | Observed | Verdict |
|---|---|---|---|
| 1. Diagnosis in/out | Diagnostic buffer entries + red/green device + `0x15`/`0x35` data status | | |
| 2. I&M1-3 write + persistence | `0xAFF1` Write OK, survives a restart with the same `--im-store` | | |
| 3. I&M0 read | `0xAFF0` OK on DAP/both interface subslots/modules, fields = builder values | | |
| 4. Device stop | ERR-RTA on the wire, CPU logs the failure within ~10 ms, no watchdog wait | | |
| 5. Replay on reconnect | Diagnosis re-announced and visible in the CPU after STOP→RUN/cable pull | | |
| 6. RT non-regression | 10-minute run, active diagnosis, `VERDICT: PASS`, 0 missed ticks | | |

## 7. Next steps
Plan 3 (`cm`/AR), Plan 4 (`rt`, cyclic exchange), Plan 7 (1 ms determinism) and **Plan 7bis
(L2-pair isolation) are all done**. `examples/rt_bringup` holds a 1 ms PROFINET update time
against the real S7-1500, idle and under load, with zero missed ticks and zero watchdog
expirations across HIL campaigns on `PREEMPT_RT` with the final binary `2ce31e2` (§6e, §6f); the
one spec §1 criterion the single-core profile missed under the spec's own load (tick lateness
p99.99, CPUs 0-2 sharing CPU 3's L2 cache) is met with the L2-pair profile (isolate CPUs 2-3,
`HK_CPUS=0-1`), now the `bench/` default (§6f). `PACKET_MMAP`/busy-poll stays deferred, needed
only if a future campaign under the original CPU-0-2 load layout still needs the p99.99 budget
without changing the CPU layout.

**Plan 6 (`config`/GSDML/typed API) is also done** (§6g): our own GSDML (station `pnio-dev`,
`0xFFFF`/`0x0001`) imports and compiles in TIA after three fixes (XSD-required DAP attributes,
`PNIO_Version="V2.3"`, `IOConfigData` counting IOxS), device-view addresses match
`gen_gsdml`'s computed map, and `typed_bringup` held the 10-minute 1 ms criterion
(`tick_lateness` p99.99 29 µs, 0 missed ticks over 599 737 cycles, L2-pair profile) with typed
`REAL`/`BOOL` round trips verified in the watch table. The 500 µs bonus was then met on the
CPU's X1 port (X2 is RT-only, fixed at 1 ms): 5 minutes at 500 µs, `PASS`, p99.99 lateness
27 µs — no code, edge or GSDML change, only the cable and the CPU's send clock.

**The robustness campaign (§6h) is done**: 12 h 51 soak at 500 µs with 0 anomalies over 79.7 M
application samples, PLC CPU load and a link loss without effect on the stack, and the one
failure mode found — controller watchdog aborts under an 80 Mbit/s broadcast storm through a
switch that does not honour 802.1p on a 100 Mbit port — attributed on the wire to the switch,
not to the stack. Open from it: loading our DCP responder needs a second host on the segment.

**Plan 5 (alarms + I&M/diagnosis) is implemented on `feat/alarm-diag-im`**: the `alarm` RTA codec
and channel state machine, `diag`'s raise/clear API and `ProblemIndicator` bit, and I&M0-3
records all pass their unit and replay tests byte-exact against the 2026-08-30 p-net capture
goldens — but none of it has run against the real S7-1500 yet. HIL is pending: §6i above is the
procedure, to be executed and its results table filled in next. Once §6i passes, the natural
follow-on is the V2.31+ GSDML profile (`LLDP_NoD_Supported`, `PTP_BoundarySupported`/
`DCP_BoundarySupported`, `ResetToFactoryModes`, `CertificationInfo`) and process alarms
(`AlarmType 0x0002`, `MayIssueProcessAlarm`, OB40) — see `FOLLOWUPS.md`'s Plan 5 section for the
full out-of-scope list. §6g's jitter-headroom observation still stands: 500 µs is now
demonstrated on X1 (§6g); 250 µs (design doc §10) would need the busy-poll / `PACKET_MMAP` work
listed in `FOLLOWUPS.md` and a faster application loop.

## Pitfalls
- **Never use CPL/PowerLine** on the segment (HomePlug `0x88e1` → jitter → RT watchdog expires, AR drops).
- p-net **must run on native Linux with L2 access** (the edge) — **not in WSL2** (NATed network).
- `pn_dev` needs `cap_net_raw`/`cap_net_admin` (or root); the storage dir must be an **absolute** path.
- The p-net device has the **rt-labs Vendor/Device ID** (not ours) — expected: we are capturing the
  **structure** of the frames, identical to what our stack will produce.
- The supervision Docker stack on the same edge is a jitter source — fine at 32 ms, not for < 2 ms.
