# Storage node image: our build of ant-node plus our network's settings.
# Built by .github/workflows/build.yml from the same binary our servers run.
FROM ubuntu:24.04

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates tini util-linux \
    && rm -rf /var/lib/apt/lists/*

COPY dist/ant-node dist/bootstrap_peers.toml /opt/ant/
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod 755 /opt/ant/ant-node /entrypoint.sh

# Our network. Operators set REWARDS_ADDRESS and usually NODE_COUNT,
# STORAGE_LIMIT_GB and BROWSER_PORT_START; the rest change only with a new
# image (for example the switch to the main token). Browser access is off
# unless BROWSER_PORT_START is set (the Unraid template sets 11000), so
# installs from before it existed don't advertise ports they never forwarded.
ENV ANT_NETWORK_MODE=testnet \
    ANT_EVM_PAYMENT_TOKEN=0x96ca806d020bf2a2c6b7ea62e0a1b318112ae180 \
    ANT_EVM_PAYMENT_VAULT=0x69C5BB60dB609F0B9a5AE48F08d3dc98092b566C \
    BOOTSTRAP_PEERS="142.93.32.90:10000 165.22.193.90:10000" \
    RPC_URL=https://robinhood-rpc.publicnode.com \
    NODE_COUNT=1 \
    STORAGE_LIMIT_GB=500 \
    PORT_START=10000 \
    BROWSER_PORT_START=0 \
    IPV4_ONLY=false \
    PUID=99 \
    PGID=100 \
    NO_COLOR=1

VOLUME /data
ENTRYPOINT ["/usr/bin/tini", "--", "/entrypoint.sh"]
