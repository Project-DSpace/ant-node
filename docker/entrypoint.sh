#!/usr/bin/env bash
# Runs NODE_COUNT storage nodes on consecutive UDP ports from PORT_START, each
# with its own folder under /data, as the unprivileged PUID:PGID. A node that
# exits is restarted after 10 seconds; stopping the container stops them all.
#
# STORAGE_LIMIT_GB caps what the nodes together keep under /data. ant-node has
# no size cap of its own, only a reserve of free disk space it never writes
# into (500 MiB by default), so the limit is applied as that reserve: whatever
# is free on the disk beyond the limit. Other files on the same disk move that
# figure, so it is re-measured every 6 hours, and the nodes are restarted one at
# a time when it has drifted by more than 5% of the limit.
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
if [[ ! "$STORAGE_LIMIT_GB" =~ ^[0-9]+$ ]]; then
    echo "STORAGE_LIMIT_GB must be a whole number of GB, or 0 for no limit" >&2
    exit 1
fi

export ANT_REWARDS_ADDRESS="$REWARDS_ADDRESS" ANT_EVM_RPC_URL="$RPC_URL"
bootstrap=()
for peer in $BOOTSTRAP_PEERS; do bootstrap+=(--bootstrap "$peer"); done
options=(--metrics-port 0 --disable-webrtc-direct --enable-logging "${bootstrap[@]}")
[[ "$IPV4_ONLY" == true ]] && options+=(--ipv4-only)

mkdir -p /data
chown "$PUID:$PGID" /data

GB=1000000000
MIB=1048576
MIN_RESERVE_MIB=500
STORAGE_CONFIG=/tmp/storage.toml

# Sets free (bytes free on the disk holding /data) and used (bytes under /data).
measure_data() {
    free=$(df -B1 --output=avail /data | tail -n 1)
    used=$(du -sxB1 /data | cut -f1)
}

# Free space (MiB) the nodes must leave on the disk for what they hold to stay
# within the limit; never less than ant-node's own default reserve.
limit_reserve() {
    local reserve=$(( (free + used - STORAGE_LIMIT_GB * GB) / MIB ))
    echo $(( reserve > MIN_RESERVE_MIB ? reserve : MIN_RESERVE_MIB ))
}

write_storage_config() {
    printf '[storage]\ndisk_reserve_mb = %s\n' "$1" > "$STORAGE_CONFIG"
    chmod 644 "$STORAGE_CONFIG"
}

# Re-applies the limit when free space outside the nodes has moved, restarting
# the nodes one at a time so the network never loses them all at once.
watch_limit() {
    local applied=$1 latest port i
    local tolerance=$(( STORAGE_LIMIT_GB * GB / 20 / MIB ))
    if (( tolerance < 5 * GB / MIB )); then
        tolerance=$(( 5 * GB / MIB ))
    fi
    trap 'exit 0' TERM
    while true; do
        # STORAGE_RECHECK_SECONDS exists for CI, which cannot wait 6 hours.
        sleep "${STORAGE_RECHECK_SECONDS:-21600}" &
        wait $! || true
        measure_data
        latest=$(limit_reserve)
        if (( latest > applied + tolerance || latest < applied - tolerance )); then
            echo "Free space outside the nodes has changed; restarting them one at a time to keep within ${STORAGE_LIMIT_GB} GB"
            write_storage_config "$latest"
            applied=$latest
            for ((i = 0; i < NODE_COUNT; i++)); do
                port=$((PORT_START + i))
                kill -TERM "$(cat "/tmp/node-$port.pid")" 2>/dev/null || true
                sleep 60 &
                wait $! || true
            done
        fi
    done
}

reserve=
if (( STORAGE_LIMIT_GB > 0 )); then
    echo "Measuring the data folder for the ${STORAGE_LIMIT_GB} GB storage limit"
    measure_data
    reserve=$(limit_reserve)
    write_storage_config "$reserve"
    options+=(--config "$STORAGE_CONFIG")
    if (( used >= STORAGE_LIMIT_GB * GB )); then
        echo "Storage limit: ${STORAGE_LIMIT_GB} GB. The nodes already hold $(( used / GB )) GB, so they keep it but take no new data"
    elif (( reserve == MIN_RESERVE_MIB )); then
        echo "Storage limit: ${STORAGE_LIMIT_GB} GB, more than this disk has room for ($(( free / GB )) GB free), so the nodes will stop when it has 500 MB left"
    else
        echo "Storage limit: ${STORAGE_LIMIT_GB} GB for all nodes together ($(( used / GB )) GB used so far, $(( free / GB )) GB free on the disk)"
    fi
else
    echo "Storage limit: none. The nodes will fill the disk up to its last 500 MB"
fi

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
        echo "$child" > "/tmp/node-$port.pid"
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
if [[ -n "$reserve" ]]; then
    watch_limit "$reserve" &
    nodes+=($!)
fi
wait
