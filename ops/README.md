# Ops

The bridge runs on the computer next to the scanner: a Raspberry Pi on the
tripod in the field, or a desktop at the bench. It holds the link to the X9
(USB or the scanner's Wi-Fi) and serves the bridge API.

## Running the bridge as a service

`x9-bridge.service` runs the bridge under systemd with a throwaway user and a
read-only system; its only writable place is `/var/lib/x9-bridge`. The token
file is handed over as a systemd credential, so the bridge never reads
`/etc/x9-bridge` itself.

```bash
sudo install -D -m 0755 x9-bridge ops/x9-bridge-run -t /opt/x9-bridge/
sudo install -D -m 0600 tokens /etc/x9-bridge/tokens   # "name role token" per line
sudo install -m 0644 ops/x9-bridge.service /etc/systemd/system/
sudo systemctl enable --now x9-bridge
```

`x9-bridge-run` picks the scanner link at start: USB (192.168.42.1) if it
answers, else the scanner's Wi-Fi (192.168.43.1). To pin one, or to change the
listen address (default `127.0.0.1:8795`), put `X9_BASE=...` or
`BRIDGE_ADDR=...` in `/etc/x9-bridge/env`.

The link is chosen only at start. With NetworkManager, `90-x9-bridge`
restarts the bridge whenever one of the scanner's connections comes up (set
`X9_NM_CONNECTIONS` in the script to your profile names):

```bash
sudo install -m 0755 ops/90-x9-bridge /etc/NetworkManager/dispatcher.d/
```

### Prefer the scanner's Wi-Fi

On a Pi that also knows a home network, NetworkManager joins whichever it sees
first and then stays there: it never leaves a working network for a better
one. So after the scanner's access point drops (a reboot, a flat battery), the
Pi stays on home Wi-Fi and the bridge loses the scanner. Two fixes, both
needed:

```bash
# Prefer the scanner's network when both are in range at start-up.
sudo nmcli connection modify x9-wifi connection.autoconnect-priority 10
# Every 30 s: if the scanner's network is in range and the Pi is on another, switch.
sudo install -m 0755 ops/x9-wifi-prefer -t /opt/x9-bridge/
sudo install -m 0644 ops/x9-wifi-prefer.service ops/x9-wifi-prefer.timer /etc/systemd/system/
sudo systemctl enable --now x9-wifi-prefer.timer
```

`x9-wifi-prefer` does nothing while the Pi is on the scanner's network or the
scanner's network isn't in range; when the scanner's network goes away,
NetworkManager falls back to home Wi-Fi by itself. After a failed switch it
waits 2 minutes before trying again. It logs to the journal
(`journalctl -t x9-wifi-prefer`). Set `X9_WIFI_CON` / `X9_WIFI_DEV` in the
service if your profile or interface has another name.

## `x9-netctl`

A small privileged helper to bring the scanner's USB link up isolated (no
default route, no DNS) and down again, without full root. It finds the scanner
whether Linux binds it as `cdc_eem` (the X9's usual mode), `rndis_host` or
`cdc_ether`. Review it, then install:

```bash
sudo install -m 0755 ops/x9-netctl /usr/local/sbin/x9-netctl
```

Allow it without a password (and nothing else):

```bash
echo "$USER ALL=(root) NOPASSWD: /usr/local/sbin/x9-netctl" | sudo tee /etc/sudoers.d/x9-netctl
```
