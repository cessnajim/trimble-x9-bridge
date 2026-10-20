# trimble-x9-bridge

A bridge service for the **Trimble X9** laser scanner: the one program that
talks to the scanner, with one HTTP API every client uses — AI agents (through
an MCP server), a phone web UI, a CLI, scripts. The safety rules live in the
bridge, so no client can skip them.

Independent interoperability project, documented from the scanner's own
behavior ([PROVENANCE.md](PROVENANCE.md)). It contains no vendor code and is
**not affiliated with or endorsed by Trimble Inc.**; "Trimble" and "X9" are
trademarks of their respective owners.

## What it does

- **Scans:** full-dome and area scans, queued one at a time, with phases
  (leveling, sweep, color) and timings reported live.
- **Rules every client is subject to:** connection, battery, storage, a busy
  scanner, a single queue, known scan modes, and an echo check that cancels a
  scan the scanner didn't accept as sent.
- **Approvals:** tokens with roles `viewer`, `agent`, `operator`, `admin`. An
  agent's scans wait for a person's approval unless an operator granted a set
  of scans (optionally limited to one recipe run). Deletes need an operator and
  an explicit confirmation.
- **Aiming:** a box drawn on a scan's preview becomes an area-scan window.
- **Exports:** levelled PLY / LAS via [trimble-tzf](https://github.com/northidaholabs/trimble-tzf),
  with automatic checks (point count, levelled, coordinates).
- **Recipes and runs:** TOML recipes of `action`, `person` and `check` steps;
  a run executes them under the starter's role.
- **Field log** and a live event stream.
- **Proposals:** the agent answers a person step (finds the window, measures it)
  and a person confirms it with one tap.
- **Phone web UI** at `/ui/`: approvals, live progress, recipe runs with box
  drawing on previews, aiming, exports and the log, from a phone in the field.

## Documentation

| Document | What |
| --- | --- |
| [docs/guide.md](docs/guide.md) | Install, configuration, tokens and roles, every workflow with `curl` examples (scans, approvals, grants, cancel, aiming, exports, deletes, field log, events), and the phone web UI |
| [docs/recipes.md](docs/recipes.md) | Recipe format v0 and how runs execute |
| [docs/errors.md](docs/errors.md) | Every error `code`, its HTTP status, when it happens and its hint |
| [docs/tools.md](docs/tools.md) | The `x9` CLI, the `x9-mock` simulator, and the tests |
| [api/bridge-v1.openapi.yaml](api/bridge-v1.openapi.yaml) | The API spec (OpenAPI 3.1; planned parts marked `x-status: planned`) |
| [docs/bridge-api-v1.md](docs/bridge-api-v1.md) | Design notes and what was tested on a real X9 |
| [ops/README.md](ops/README.md) | Running the bridge as a systemd service; the USB link helper |
| [PROVENANCE.md](PROVENANCE.md) | How the scanner API knowledge was obtained |

## Layout

| Path | What |
| --- | --- |
| `crates/x9-bridge` | The bridge service (Rust, axum) |
| `crates/x9-client` | Client for the scanner's HTTP API |
| `crates/x9-mock` | Scanner simulator: scans with realistic phases, area scans, cancel, failure modes |
| `crates/x9-cli` | Small CLI over the client |
| `ops/` | systemd unit for the bridge, and `x9-netctl` to bring the scanner's USB link up isolated (no default route, no DNS) |

## Install

Release builds for Linux x86_64 and aarch64 (Raspberry Pi 4/5) are on the
[releases page](https://github.com/northidaholabs/trimble-x9-bridge/releases): each
archive has `x9-bridge`, `x9-mock`, the `x9` CLI and `ops/` (the systemd unit and `x9-netctl`).

## Try it against the simulator

```bash
cargo build --release
target/release/x9-mock &                                   # simulator on 127.0.0.1:34567
printf 'me operator %s\n' "$(openssl rand -hex 16)" > tokens
X9_BASE=http://127.0.0.1:34567/api/v1 BRIDGE_TOKENS=tokens target/release/x9-bridge   # API on 127.0.0.1:8795/v1
```

Against a real scanner, point `X9_BASE` at it: `http://192.168.42.1:34567/api/v1`
over USB, or `http://192.168.43.1:34567/api/v1` on its Wi-Fi.

## Tests

```bash
cargo test                                   # rules, grants, aiming, recipes, echo check
python3 crates/x9-bridge/tests/e2e.py target/release
```

The end-to-end test starts the simulator and the bridge and checks the main
flows. With `E2E_FIXTURES` pointing at a directory of real scans
(`dome.tzf`/`.jpg`, `area.tzf`/`.jpg`) it also checks previews, aiming,
exports and a full recipe run.

## Agents

[TrimbleX9-mcp](https://github.com/northidaholabs/TrimbleX9-mcp) has a bridge mode
(`SCANNER_BRIDGE_URL`, `SCANNER_BRIDGE_TOKEN`) that exposes this API as MCP
tools, with previews returned as images and recipes as a prompt.

## Status

Version 0.1: the API is a draft. Tested against the simulator and on a real X9
(status, an approved full dome, two granted area scans).
