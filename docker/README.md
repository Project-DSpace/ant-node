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
   - **Data folder**: leave `/mnt/cache/appdata/storage-node` if you have a cache
     pool.
4. Click **Apply**. Unraid downloads the image and starts the nodes.

Without the template file, fill in Add Container by hand instead: Repository
`ghcr.io/project-dspace/storage-node:latest`, Network Type **Host**, a path
`/data` → `/mnt/cache/appdata/storage-node`, and variables `REWARDS_ADDRESS`
(your wallet) and `NODE_COUNT` (6).

## 6. Check it's working

On the Docker tab, click the container's icon and choose **Logs**. Within a
minute you should see:

```
Started 6 node(s) on UDP ports 10000-10005; storage fees go to 0x…
[node 10000] … Successfully connected to 2 bootstrap peers
```

Then send your public IP address to the person who gave you this guide, so they
can confirm from the network side that your nodes are reachable.

## Day to day

- **Updates**: on the Docker tab, click **Check for Updates**, then **Apply
  Update** when one is available.
- **Keep it running**: nodes that are offline for long get dropped by the
  network and stop earning. Restarts and reboots are fine.
- **Stopping**: stop the container on the Docker tab. Your node data stays in the
  data folder.
- **No IPv6?** If the logs show connection problems and your connection has no
  working IPv6, set **IPv4 only** to `true` (under "Show more settings").
