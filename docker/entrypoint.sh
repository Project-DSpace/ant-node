#!/usr/bin/env bash
# Runs our storage nodes with Autonomi's own node manager (`ant node`, the
# same code as in our client fork), set up the way Autonomi's operators run
# theirs: the operator picks how many nodes, nodes use the free space where
# /data lives, and the manager removes a node when that disk is nearly full.
# Added on top: our network's settings and fixed browser (WebRTC Direct)
# ports, so home routers can forward them.
#
# On every start: check the settings, clear process files left by the last
# run, take over nodes from the previous image's layout, move nodes onto a
# newer node binary if the image has one, bring the manager's registry in line
# with these settings, add or retire nodes to match NODE_COUNT, then start the
# manager and the nodes and stream their logs. docker stop stops the nodes
# before the manager.
set -euo pipefail

STATE=/data/manager/ant              # the manager's registry and process files
REGISTRY=$STATE/node_registry.json
NODES=/data/nodes                    # node-<id>: one node's data (its --root-dir)
LOGS=/data/logs                      # node-<id>/logs: one node's daily log files
RETIRED=/data/retired                # nodes removed by lowering NODE_COUNT, kept 3 days
MIN_FREE_PER_NODE=$((20 * 1024 ** 3)) # Autonomi's recommended minimum per node

fail() { echo "$*" >&2; exit 1; }
as_user() { setpriv --reuid="$PUID" --regid="$PGID" --clear-groups "$@"; }
manager() { as_user ant node "$@"; }

# ---------- settings ----------

: "${REWARDS_ADDRESS:?set REWARDS_ADDRESS to the wallet that should receive storage fees}"
[[ "$REWARDS_ADDRESS" =~ ^0x[0-9a-fA-F]{40}$ ]] || fail "REWARDS_ADDRESS is not a wallet address: $REWARDS_ADDRESS"
# Whole numbers only, read as base 10 (bash would read a leading 0 as octal).
for name in NODE_COUNT PORT_START BROWSER_PORT_START MANAGER_PORT; do
    [[ "${!name}" =~ ^[0-9]+$ ]] || fail "$name must be a whole number"
    printf -v "$name" '%d' "$((10#${!name}))"
done
(( NODE_COUNT >= 1 )) || fail "NODE_COUNT must be at least 1"
# Every chunk is stored on 7 nodes. In testnet mode nothing keeps all 7 off
# one machine, so at most 6 per machine keeps a copy of everything elsewhere.
if (( NODE_COUNT > 6 )) && [[ "$ANT_NETWORK_MODE" == testnet ]]; then
    fail "NODE_COUNT is limited to 6 per machine while the network is in testnet mode"
fi
(( PORT_START >= 1024 && PORT_START <= 65536 - NODE_COUNT )) \
    || fail "PORT_START must leave room for $NODE_COUNT UDP ports between 1024 and 65535"
if (( BROWSER_PORT_START != 0 )); then
    (( BROWSER_PORT_START >= 1024 && BROWSER_PORT_START <= 65536 - NODE_COUNT )) \
        || fail "BROWSER_PORT_START must leave room for $NODE_COUNT UDP ports between 1024 and 65535, or be 0 to turn browser access off"
    (( BROWSER_PORT_START >= PORT_START + NODE_COUNT || PORT_START >= BROWSER_PORT_START + NODE_COUNT )) \
        || fail "The browser ports ($BROWSER_PORT_START on) overlap the storage ports ($PORT_START on)"
fi
[[ -z "$PUBLIC_IP" || "$PUBLIC_IP" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "PUBLIC_IP must be an IPv4 address"
if [[ -n "${STORAGE_LIMIT_GB:-}" && "${STORAGE_LIMIT_GB:-0}" != 0 ]]; then
    echo "STORAGE_LIMIT_GB is no longer used: like Autonomi's nodes, ours use the free space where /data lives."
fi
read -ra rpc_urls <<<"${RPC_URLS:-$RPC_URL}"
(( ${#rpc_urls[@]} > 0 )) || fail "Set RPC_URL"
read -ra bootstrap <<<"$BOOTSTRAP_PEERS"

# Settings every node inherits from the manager's environment.
export ANT_EVM_RPC_URL="${rpc_urls[0]}" ANT_METRICS_PORT=0
[[ "$IPV4_ONLY" == true ]] && export ANT_IPV4_ONLY=true
(( BROWSER_PORT_START == 0 )) && export ANT_DISABLE_WEBRTC_DIRECT=true

# The settings that differ per node, kept in the manager's registry. Node with
# storage port P gets browser port P - PORT_START + BROWSER_PORT_START, and the
# RPC providers in RPC_URLS are shared out over the nodes in equal blocks.
node_env() {
    local port=$1 index=$(($1 - PORT_START)) env=()
    if (( BROWSER_PORT_START != 0 )); then
        env+=("ANT_WEBRTC_DIRECT_PORT=$((BROWSER_PORT_START + index))")
        [[ -n "$PUBLIC_IP" ]] && env+=("ANT_WEBRTC_DIRECT_ADVERTISED_ADDR=$PUBLIC_IP:$((BROWSER_PORT_START + index))")
    fi
    env+=("ANT_EVM_RPC_URL=${rpc_urls[index * ${#rpc_urls[@]} / NODE_COUNT]}")
    (IFS=,; echo "${env[*]}")
}

# ---------- disk layout ----------

mkdir -p "$STATE" "$NODES" "$LOGS" "$RETIRED"
chown "$PUID:$PGID" /data /data/manager "$STATE" "$NODES" "$LOGS" "$RETIRED"

# The previous image kept node n in /data/node-<port>. Take those over in
# port order as node-1, node-2, ... so every node keeps its identity, data and
# browser certificate, then register them on the same ports below.
if [[ ! -s "$REGISTRY" ]] && compgen -G "/data/node-[0-9]*" >/dev/null; then
    id=0
    for dir in $(ls -d /data/node-[0-9]* | sort -t- -k2 -n); do
        id=$((id + 1))
        echo "Taking over $(basename "$dir") as node-$id"
        mv "$dir" "$NODES/node-$id"
    done
fi

# Process files from a previous run: process IDs start again from 1 in a new
# container, so a stale one could match an unrelated process.
rm -f "$STATE/daemon.pid" "$STATE/daemon.port" "$NODES"/node-*/node.pid
find "$RETIRED" -mindepth 1 -maxdepth 1 -mtime +3 -exec rm -rf {} +

avail=$(df -B1 --output=avail /data | tail -n 1 | tr -d ' ')
if (( avail < NODE_COUNT * MIN_FREE_PER_NODE )); then
    echo "WARNING: only $((avail / 1024 ** 3)) GiB free under /data, below the $((NODE_COUNT * 20)) GiB recommended for $NODE_COUNT node(s)." \
        "Each node needs at least 20 GB of free disk space; nodes that drop below this minimum are treated as full and risk being shunned by the network."
fi

# A node runs its own copy of the binary (and may upgrade it itself), so give
# it the image's binary only when that is newer than the copy it has.
image_version=$(/opt/ant/ant-node --version | awk '{print $NF}')
for copy in "$NODES"/node-*/ant-node; do
    [[ -x "$copy" ]] || continue
    current=$("$copy" --version 2>/dev/null | awk '{print $NF}') || current=0
    if [[ "$current" != "$image_version" && "$(printf '%s\n%s\n' "$current" "$image_version" | sort -V | tail -n 1)" == "$image_version" ]]; then
        echo "$(basename "$(dirname "$copy")"): node binary $current -> $image_version"
        install -m 755 -o "$PUID" -g "$PGID" /opt/ant/ant-node "$copy"
    fi
done

# ---------- registry: match the settings, then NODE_COUNT ----------

if [[ -s "$REGISTRY" ]]; then
    tmp=$(mktemp)
    jq --arg rewards "$REWARDS_ADDRESS" --argjson boot "$(printf '%s\n' "${bootstrap[@]}" | jq -R . | jq -s 'map(select(. != ""))')" \
        '.nodes |= with_entries(.value.rewards_address = $rewards | .value.bootstrap_peers = $boot)' \
        "$REGISTRY" > "$tmp"
    for id in $(jq -r '.nodes | keys[]' "$tmp"); do
        port=$(jq -r --arg id "$id" '.nodes[$id].node_port' "$tmp")
        env=$(node_env "$port")
        jq --arg id "$id" --arg env "$env" '
            .nodes[$id].env_variables |= (with_entries(select(.key
                | IN("ANT_WEBRTC_DIRECT_PORT", "ANT_WEBRTC_DIRECT_ADVERTISED_ADDR", "ANT_EVM_RPC_URL") | not))
              + ($env | split(",") | map(split("=") | {key: .[0], value: (.[1:] | join("="))}) | from_entries))' \
            "$tmp" > "$tmp.next" && mv "$tmp.next" "$tmp"
    done
    install -m 644 -o "$PUID" -g "$PGID" "$tmp" "$REGISTRY" && rm -f "$tmp"
fi

registered() { if [[ -s "$REGISTRY" ]]; then jq -r "$1" "$REGISTRY"; fi; }
evicted=$(registered '[.nodes[] | select(.eviction != null)] | length')
evicted=${evicted:-0}
if (( evicted > 0 )); then
    echo "$evicted node(s) were removed by the node manager because the disk was nearly full and stay removed:" \
        "free up space, then run 'docker exec <container> nodes clear-evicted' and restart the container to replace them."
fi
target=$((NODE_COUNT - evicted))

# Lowering NODE_COUNT retires the nodes on the highest ports; their data is
# kept in $RETIRED for 3 days in case the change is undone.
for id in $(registered '.nodes | to_entries | map(select(.value.eviction == null)) | sort_by(.value.node_port) | reverse | .[].key'); do
    active=$(registered '[.nodes[] | select(.eviction == null)] | length')
    (( active > target )) || break
    port=$(registered ".nodes[\"$id\"].node_port")
    echo "Retiring node-$id (port $port): NODE_COUNT is $NODE_COUNT"
    manager dismiss "$id" >/dev/null
    stamp=$(date -u +%Y%m%dT%H%M%S)
    if [[ -d "$NODES/node-$id" ]]; then mv "$NODES/node-$id" "$RETIRED/$stamp-node-$id"; fi
    if [[ -d "$LOGS/node-$id" ]]; then mv "$LOGS/node-$id" "$RETIRED/$stamp-node-$id-logs"; fi
done

# Raising it (or a first start) adds nodes on the lowest free ports.
while :; do
    active=$(registered '[.nodes[] | select(.eviction == null)] | length')
    (( ${active:-0} < target )) || break
    used=" $(registered '[.nodes[].node_port] | map(tostring) | join(" ")') "
    for ((index = 0; index < NODE_COUNT; index++)); do
        [[ "$used" == *" $((PORT_START + index)) "* ]] || break
    done
    port=$((PORT_START + index))
    args=(--count 1 --rewards-address "$REWARDS_ADDRESS" --node-port "$port" --path /opt/ant/ant-node
          --data-dir-path "$NODES" --log-dir-path "$LOGS" --env "$(node_env "$port")")
    if (( ${#bootstrap[@]} > 0 )); then args+=(--bootstrap "$(IFS=,; echo "${bootstrap[*]}")"); fi
    manager add "${args[@]}" >/dev/null
    echo "Added node on UDP port $port"
done

# Node folders without a registered node (taken over from the old layout but
# beyond NODE_COUNT) go to $RETIRED too, rather than sitting unused.
for dir in "$NODES"/node-*; do
    [[ -d "$dir" ]] || continue
    id=${dir##*/node-}
    if [[ "$(registered ".nodes | has(\"$id\")")" != true ]]; then
        echo "Retiring unused $(basename "$dir")"
        mv "$dir" "$RETIRED/$(date -u +%Y%m%dT%H%M%S)-$(basename "$dir")"
    fi
done

# ---------- run ----------

as_user ant node daemon run --listen-addr 127.0.0.1 --port "$MANAGER_PORT" --log-path "$LOGS/manager" &
manager_pid=$!
for _ in $(seq 1 60); do
    manager daemon status >/dev/null 2>&1 && break
    kill -0 "$manager_pid" 2>/dev/null || fail "The node manager failed to start; see $LOGS/manager.*.log"
    sleep 1
done

# Stream the newest log file of every node (they rotate daily) and of the
# manager to the container log, re-checking for new files every minute.
follow_logs() {
    declare -A tails
    while :; do
        for dir in "$LOGS"/node-*/logs "$LOGS"; do
            newest=$(ls -t "$dir"/*.log 2>/dev/null | head -n 1 || true)
            [[ -n "$newest" && "${tails[$dir]:-}" != "$newest" ]] || continue
            if [[ "$dir" == "$LOGS" ]]; then label=manager; else label=$(basename "$(dirname "$dir")"); fi
            if [[ -n "${tails[$dir]:-}" ]]; then pkill -f "tail -n 0 -F ${tails[$dir]}" || true; fi
            tail -n 0 -F "$newest" 2>/dev/null | sed -u "s/^/[$label] /" &
            tails[$dir]=$newest
        done
        sleep 60
    done
}
follow_logs &
logs_pid=$!

shutdown() {
    echo "Stopping nodes"
    manager stop >/dev/null 2>&1 || true
    kill -INT "$manager_pid" 2>/dev/null || true
    wait "$manager_pid" 2>/dev/null || true
    kill "$logs_pid" 2>/dev/null || true
    pkill -f "tail -n 0 -F" 2>/dev/null || true
    exit 0
}
trap shutdown TERM INT

manager start >/dev/null
if (( BROWSER_PORT_START == 0 )); then browser=off; else browser="UDP $BROWSER_PORT_START-$((BROWSER_PORT_START + NODE_COUNT - 1))"; fi
echo "Running $target node(s) on UDP ports $PORT_START-$((PORT_START + NODE_COUNT - 1)); browser access: $browser; storage fees go to $REWARDS_ADDRESS"
manager status || true

wait "$manager_pid" || true
echo "The node manager stopped unexpectedly; stopping the nodes" >&2
manager stop >/dev/null 2>&1 || true
exit 1
