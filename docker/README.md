# Running storage nodes on Unraid

This sets up storage nodes for our network in a Docker container on your Unraid
server. It takes about 15 minutes. You'll need:

- A **GitHub account** (free), to download the private image.
- A **wallet address** for your storage fees: any Ethereum-style address
  starting with `0x`, for example from MetaMask.
- **Access to your router**, to forward some UDP ports.
- An **SSD cache pool** on Unraid for the node data (recommended).

We're in a test phase: fees are paid in a test token with no real value yet.

## 1. Check your internet connection can host nodes

Nodes must be reachable from the internet. Compare the WAN/Internet IP shown on
your router's status page with what [whatismyip.com](https://www.whatismyip.com)
shows. If they're **different**, your provider uses "carrier-grade NAT" and port
forwarding won't work; tell us before going further.

## 2. Get access to the image

1. Send your GitHub username to the person who gave you this guide. They'll
   give your account read access to the image.
2. On GitHub, go to **Settings → Developer settings → Personal access tokens →
   Tokens (classic) → Generate new token (classic)**. Tick only
   **`read:packages`**, set an expiry (for example 1 year), generate it and copy
   the token.

## 3. Log in to the image registry from Unraid

In the Unraid web interface, open the terminal (the `>_` icon, top right) and run,
with your GitHub username:

```
docker login ghcr.io -u YOUR_GITHUB_USERNAME
```

Paste the token when asked for a password. You should see `Login Succeeded`.

Unraid forgets this login when it reboots. To keep it, run once:

```
mkdir -p /boot/config/docker-auth && cp /root/.docker/config.json /boot/config/docker-auth/
echo 'mkdir -p /root/.docker && cp /boot/config/docker-auth/config.json /root/.docker/' >> /boot/config/go
```

(Running nodes keep working after a reboot either way; the login only matters
for downloading updates.)

## 4. Forward the ports on your router

Forward **UDP** ports **10000–10005** (one per node; 6 nodes use 10000–10005) to
your Unraid server's local IP address. The exact menu depends on your router; it
is usually called "Port forwarding" or "Virtual servers". Use the same port
numbers outside and inside.

## 5. Add the container

1. Copy `unraid-storage-node.xml` (sent with this guide) to your Unraid flash
   drive as `config/plugins/dockerMan/templates-user/my-storage-node.xml`. Over
   the network, the flash drive is the `flash` share.
2. In Unraid, go to **Docker → Add Container**, and choose **storage-node** from
   the Template list.
3. Fill in:
   - **Rewards wallet**: your `0x…` address.
   - **Number of nodes**: 6 (the maximum for now), or fewer on a slow connection.
   - **Storage limit (GB)**: the most disk space all the nodes together may use
     (1000 = 1 TB). They stop taking new data at the limit. `0` means no limit:
     they fill the disk up to its last 500 MB.
   - **Data folder**: leave `/mnt/cache/appdata/storage-node` if you have a cache
     pool. Otherwise pick a folder on one specific disk or pool, such as
     `/mnt/disk1/storage-node`, not `/mnt/user/...`: the nodes measure free space
     where the folder is, and `/mnt/user` can show the whole array's free space
     instead of the disk the data really lands on.
4. Click **Apply**. Unraid downloads the image and starts the nodes.

Without the template file, fill in Add Container by hand instead: Repository
`ghcr.io/project-dspace/storage-node:latest`, Network Type **Host**, a path
`/data` → `/mnt/cache/appdata/storage-node`, and variables `REWARDS_ADDRESS`
(your wallet), `NODE_COUNT` (6) and `STORAGE_LIMIT_GB` (the limit in GB). Under
**Advanced view → Extra Parameters**, add
`--ulimit nofile=65536:65536 --stop-timeout 60`.

## 6. Check it's working

On the Docker tab, click the container's icon and choose **Logs**. Within a
minute you should see:

```
Storage limit: 500 GB for all nodes together (0 GB used so far, … GB free on the disk)
Started 6 node(s) on UDP ports 10000-10005; storage fees go to 0x…
[node 10000] … Successfully connected to 2 bootstrap peers
```

On the Docker tab the container's network should show as **host**, with the IP
and port columns blank. If it says **bridge**, edit the container and set
Network Type to **Host**: in bridge mode nothing from the internet can reach
the nodes.

Then send your public IP address to the person who gave you this guide, so they
can confirm from the network side that your nodes are reachable.

## Day to day

- **Updates**: on the Docker tab, click **Check for Updates**, then **Apply
  Update** when one is available.
- **Keep it running**: nodes that are offline for long get dropped by the
  network and stop earning. Restarts and reboots are fine.
- **Stopping**: stop the container on the Docker tab. Your node data stays in the
  data folder.
- **Changing the storage limit**: edit the container, change **Storage limit
  (GB)** and click **Apply**. Lowering it below what the nodes already hold
  stops new data but deletes nothing. The limit counts only the nodes' folder:
  if other files on the same disk shrink or grow a lot, the container notices
  within 6 hours and briefly restarts the nodes one at a time to re-apply it,
  so expect the nodes to be within about 5% of the limit rather than exact.
- **No IPv6?** If the logs show connection problems and your connection has no
  working IPv6, set **IPv4 only** to `true` (under "Show more settings").
