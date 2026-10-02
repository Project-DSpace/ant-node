#!/usr/bin/env bash
# Runs NODE_COUNT storage nodes on consecutive UDP ports from PORT_START, each
# with its own folder under /data, as the unprivileged PUID:PGID. A node that
# exits is restarted after 10 seconds; stopping the container stops them all.
set -euo pipefail

: "${REWARDS_ADDRESS:?set REWARDS_ADDRESS to the wallet that should receive storage fees}"
if [[ ! "$REWARDS_ADDRESS" =~ ^0x[0-9a-fA-F]{40}$ ]]; then
    echo "REWARDS_ADDRESS is not a wallet address: $REWARDS_ADDRESS" >&2
    exit 1
fi
if [[ ! "$NODE_COUNT" =~ ^[1-9][0-9]*$ ]]; then
    echo "NODE_COUNT must be a whole number from 1 up" >&2
    exit 1
fi
# Every chunk is stored on 7 nodes. While the network runs in testnet mode,
# nothing keeps all 7 off one machine, so at most 6 nodes per machine keeps at
# least one copy of everything elsewhere.
if (( NODE_COUNT > 6 )) && [[ "$ANT_NETWORK_MODE" == testnet ]]; then
    echo "NODE_COUNT is limited to 6 per machine while the network is in testnet mode" >&2
    exit 1
fi

export ANT_REWARDS_ADDRESS="$REWARDS_ADDRESS" ANT_EVM_RPC_URL="$RPC_URL"
bootstrap=()
for peer in $BOOTSTRAP_PEERS; do bootstrap+=(--bootstrap "$peer"); done
options=(--metrics-port 0 --disable-webrtc-direct --enable-logging "${bootstrap[@]}")
[[ "$IPV4_ONLY" == true ]] && options+=(--ipv4-only)

mkdir -p /data
chown "$PUID:$PGID" /data

run_node() {
    local port=$1 dir=/data/node-$1 child=
    mkdir -p "$dir"
    chown "$PUID:$PGID" "$dir"
    trap '[[ -n "$child" ]] && kill -TERM "$child" 2>/dev/null; wait; exit 0' TERM
    while true; do
        HOME="$dir" setpriv --reuid="$PUID" --regid="$PGID" --clear-groups \
            /opt/ant/ant-node --root-dir "$dir" --port "$port" "${options[@]}" \
            > >(sed -u "s/^/[node $port] /") 2>&1 &
        child=$!
        wait "$child" || true
        echo "[node $port] stopped; restarting in 10 seconds"
        sleep 10 &
        wait $! || true
    done
}

nodes=()
shutdown() {
    echo "Stopping nodes"
    kill -TERM "${nodes[@]}" 2>/dev/null || true
    wait
    exit 0
}
trap shutdown TERM INT

for ((i = 0; i < NODE_COUNT; i++)); do
    run_node $((PORT_START + i)) &
    nodes+=($!)
done
echo "Started $NODE_COUNT node(s) on UDP ports $PORT_START-$((PORT_START + NODE_COUNT - 1)); storage fees go to $REWARDS_ADDRESS"
wait
