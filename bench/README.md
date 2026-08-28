# `bench/` — 1 ms PROFINET RT campaign

Edge tuning, load generator and campaign driver for the Plan 7 goal: hold a 1 ms
update time against the S7-1500 (1515-2 PN), at idle and under load, on the
PREEMPT_RT edge `lab-server` (Intel Atom E3940, 4 cores, `eno2` = `172.16.2.10`
facing the PLC — never re-address it, it is also a NAT gateway leg).

## Prerequisites

- Debian 13 with the `linux-image-rt-amd64` PREEMPT_RT kernel installed and booted
  (verify with `uname -r` — it must end in `-rt-amd64` — and `cat /sys/kernel/realtime`
  must print `1`).
- GRUB cmdline (spec §5.1), in `/etc/default/grub`:

  ```
  GRUB_CMDLINE_LINUX_DEFAULT="quiet isolcpus=domain,managed_irq,3 nohz_full=3 rcu_nocbs=3 irqaffinity=0-2 intel_idle.max_cstate=1 processor.max_cstate=1 nosoftlockup"
  ```

  then:

  ```
  sudo update-grub
  sudo reboot
  ```

  After reboot, verify:
  - `cat /sys/devices/system/cpu/isolated` = `3`
  - `cat /sys/devices/system/cpu/nohz_full` = `3`
  - `cat /sys/kernel/realtime` = `1`
  - `ip -4 addr show eno2` = `172.16.2.10/24`
  - the NAT gateway still answers (`ping 192.168.1.200` from the Windows side, TTL 254)

- Docker/containerd, `wpa_supplicant`, bluetooth and any other periodic-timer
  service that isn't needed for the campaign disabled (`systemctl disable --now`)
  — they compete for the housekeeping CPUs and add jitter sources unrelated to the
  RT path.
- Packages: `apt install rt-tests stress-ng tcpdump ethtool`.

## Install

From the workstation, with the edge reachable as `maintenance@192.168.1.21`:

```
scp bench/*.sh bench/*.service maintenance@192.168.1.21:~/bench/
```

On the edge:

```
chmod +x ~/bench/*.sh
sudo cp ~/bench/profinet-rt-tune.service /etc/systemd/system/ && sudo systemctl enable --now profinet-rt-tune
```

Capabilities (each binary needs them set directly on the file — `sudo`-running the
campaign is not a substitute, and the campaign is meant to run unprivileged):

```
sudo setcap cap_net_raw,cap_net_admin,cap_sys_nice,cap_ipc_lock+eip ~/bench/rt_bringup
sudo setcap cap_net_raw,cap_net_admin+eip /usr/bin/tcpdump
sudo setcap cap_sys_nice,cap_ipc_lock+eip /usr/bin/cyclictest
```

`setcap` capabilities are stored on the file's inode, not the path: **repeat the
three commands above after every copy of a binary onto the edge** (a fresh `scp` of
`rt_bringup`, a package upgrade of `tcpdump`/`cyclictest`, etc. all reset them).

## Build the edge binary

The edge is `musl`-only (no glibc toolchain maintained there):

```
cargo build --release --target x86_64-unknown-linux-musl --example rt_bringup
scp target/x86_64-unknown-linux-musl/release/examples/rt_bringup maintenance@192.168.1.21:~/bench/
```

Remember to re-run the `setcap` line for `rt_bringup` above after this `scp`.

## Run

```
~/bench/campaign.sh [DURATION_SECONDS]   # default 600 (10 min)
```

Run it from `~/bench` on the edge. It refuses to start unless the kernel is
PREEMPT_RT and CPU 3 is isolated, and requires `~/bench/rt_bringup` to exist and be
executable.

Directory layout, one directory per run, timestamped:

```
~/bench/logs/plan7-<YYYYmmdd-HHMMSS>/
  env.txt                 kernel, cmdline, isolated CPUs, profinet-rt-tune unit status
  cyclictest-idle.txt      cyclictest, idle
  load-cyclictest.txt      stress-ng output while cyclictest ran under load
  cyclictest-load.txt      cyclictest, under load
  rt-idle.log              rt_bringup stdout/stderr, idle (summary + VERDICT at the end)
  rt-idle.csv              rt_bringup per-interval stats, idle
  rt-idle.csv.hist.csv     rt_bringup latency histograms, idle
  load-rt.txt              stress-ng output while rt_bringup ran under load
  tcpdump.txt              tcpdump stdout/stderr
  rt-load.pcapng           packet capture on eno2 during the load run
  rt-load.log              rt_bringup stdout/stderr, under load (summary + VERDICT)
  rt-load.csv              rt_bringup per-interval stats, under load
  rt-load.csv.hist.csv     rt_bringup latency histograms, under load
  summary.txt              the four files above, condensed (see below)
```

`summary.txt` holds: the campaign timestamp and duration, the cyclictest max-latency
line for idle and load, and — for both the idle and the load `rt_bringup` run — the
run's return code and everything from the `rt_bringup summary` banner onward (the
three histogram lines, the counters, and the `VERDICT: PASS`/`FAIL` line). The
campaign's own exit code is the logical AND of the two `rt_bringup` verdicts (0 only
if idle and load both passed).

The `lat_max_us`, `lat_p9999_us`, `work_max_us` and `rxint_max_us` columns in
`rt-idle.csv`/`rt-load.csv` are **cumulative** running maxima/percentiles since the
run started, not per-interval values — a spike at t=10s still shows in the row for
t=590s. Read them at the last row for the whole-run figure, or diff consecutive rows
only if you specifically want to bound where in the run a regression appeared.

## Thresholds

Success criteria (spec §1), checked over the full run duration, at idle **and**
under load:

1. `missed_ticks == 0` and `watchdog_expirations == 0` — no `RtWatchdog` abort, the
   device stays green in TIA. Not overridable (always required for PASS).
2. Tick lateness (timer wake-up − scheduled expiry): p99.99 < 100 µs, max < 300 µs
   (the consumer watchdog is 3 cycles = 3 ms). Override with `--p9999-lateness-us`
   and `--max-lateness-us`.
3. CPU→device inter-arrival interval (`0x8001` frames, measured by the RT thread):
   max < 1.5 ms. Override with `--max-rx-interval-us`.
4. Watch table unchanged from Plan 4 (`%IB0 == %QB0`, `%ID2 == %QD2`,
   `%ID6 == %QD6`) and the TIA diagnostic buffer clean — checked by hand in TIA, not
   by `rt_bringup`.

`rt_bringup` defaults match points 2 and 3 above
(`--max-lateness-us 300 --p9999-lateness-us 100 --max-rx-interval-us 1500`);
`campaign.sh` does not override them, so pass the flags on the `rt_bringup` command
line (edit `RT_ARGS` in `campaign.sh`, or run `rt_bringup` by hand) to try a
different threshold.

## Post-processing the capture

From Windows/WSL, against the campaign directory's `rt-load.pcapng`, using Wireshark
for Windows (`tshark.exe`) since the edge has no analysis tooling installed:

CPU → device (`0x8001`):

```
"/mnt/c/Program Files/Wireshark/tshark.exe" -r rt-load.pcapng -Y "pn_rt.frame_id == 0x8001" -T fields -e frame.time_delta_displayed \
  | sort -n \
  | awk '{a[NR]=$1} END{n=NR; p=int(n*0.9999); if(p<1)p=1; printf "n=%d p99.99=%.6fs max=%.6fs\n", n, a[p], a[n]}'
```

Device → CPU (`0x8000`):

```
"/mnt/c/Program Files/Wireshark/tshark.exe" -r rt-load.pcapng -Y "pn_rt.frame_id == 0x8000" -T fields -e frame.time_delta_displayed \
  | sort -n \
  | awk '{a[NR]=$1} END{n=NR; p=int(n*0.9999); if(p<1)p=1; printf "n=%d p99.99=%.6fs max=%.6fs\n", n, a[p], a[n]}'
```

`frame.time_delta_displayed` is the inter-arrival time from the previous *displayed*
frame of the same filtered stream, in seconds; the p99.99/max pair from each command
is what goes into the campaign report next to the `0x8001` threshold (spec §1.3) and
the `0x8000` figure kept for reference.

## TIA

Before step 3 of `campaign.sh` (the first `rt_bringup` run):

- Update time: **1 ms** (down from the Plan 4 baseline of 32 ms).
- Watchdog factor: **3** (3 ms consumer watchdog — the tick-lateness thresholds
  above are sized against it).
- Download the changed hardware configuration to the CPU.
- Do this **between** the 32 ms control run (a `campaign.sh --duration 120`-scale
  non-regression check at the old update time, done by hand first) and the actual 1
  ms campaign — never change the update time mid-campaign.
