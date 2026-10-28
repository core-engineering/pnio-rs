#!/usr/bin/env bash
# Runs crates/pnio/tests/veth_e2e.rs: the device on one end of a veth pair, a scripted
# controller on the other end in its own network namespace.
#
# Everything happens in a throwaway user + network + mount namespace: no root needed,
# nothing is left on the host. Extra arguments go to the test binary (e.g. a test name,
# --nocapture). Needs unprivileged user namespaces; on Ubuntu 24.04 (AppArmor):
#   sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
set -euo pipefail

if [ "${1:-}" != "--inside" ]; then
    cd "$(dirname "$0")/.."
    # Build outside: the namespace has no network to fetch crates.
    cargo test -p pnio --test veth_e2e --no-run
    exec unshare --user --map-root-user --net --mount --propagation private \
        "$0" --inside "$@"
fi
shift

# Private /run for `ip netns` (bind-mounts the namespace under /run/netns), and a
# /sys showing this network namespace (the device reads its MAC and link from sysfs).
mount -t tmpfs none /run
mount -t sysfs none /sys
ip link set lo up
ip netns add pnio-cpu
# The bench MACs: the controller's is the initiator MAC of the golden Connect request.
ip link add veth-dev address 8c:f3:19:cd:19:f8 type veth \
    peer name veth-cpu address ec:1c:5d:61:e7:3f netns pnio-cpu
ip addr add 172.16.2.10/24 dev veth-dev
ip link set veth-dev up
ip -n pnio-cpu link set veth-cpu up
# The controller side has no IP stack: answer ARP statically.
ip neigh replace 172.16.2.100 lladdr ec:1c:5d:61:e7:3f dev veth-dev nud permanent

cargo test -p pnio --test veth_e2e --offline -- --ignored "$@"
