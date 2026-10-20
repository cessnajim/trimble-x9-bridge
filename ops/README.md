# Bridge

The bridge is the computer next to the scanner (the Pi on the tripod, or the
Fedora box at the desk). It holds the USB link to the X9 and runs the MCP server.

## `x9-netctl`

A small privileged helper so the agent can bring the scanner's USB link up and
down without full root. Review it, then install:

```bash
sudo install -m 0755 ops/x9-netctl /usr/local/sbin/x9-netctl
```

Allow it without a password (and nothing else):

```bash
echo "$USER ALL=(root) NOPASSWD: /usr/local/sbin/x9-netctl" | sudo tee /etc/sudoers.d/x9-netctl
```

Changes from the version installed on 2026-10-18:
- `usb-up` / `usb-info` now find the scanner when Linux binds it as `cdc_eem`
  (the X9's usual mode), not only `rndis_host` / `cdc_ether`.
