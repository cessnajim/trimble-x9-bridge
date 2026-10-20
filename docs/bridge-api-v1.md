# Bridge API v1 — design notes

Status: **draft 2** (2026-10-20); skeleton implemented in `crates/x9-bridge`. Spec: [`api/bridge-v1.openapi.yaml`](../api/bridge-v1.openapi.yaml).

The bridge service is the only thing that talks to the scanner. Every client
(AI agents via the MCP server, the phone web UI, the `x9` CLI, scripts, agent
frameworks calling operations as functions) uses this API, and the rules live
here, not in any client.

Decided 2026-10-20: the service is **Rust**, in this workspace (a new
`x9-bridge` crate on top of `x9-client`), with a **Python worker** for
measurement until that code is ported.

## Resource model

| Resource | What it is | Notes |
| --- | --- | --- |
| `status` | Bridge + scanner state in one read | Also pushed on `/events` |
| `projects` | Scanner projects | The bridge hides the scanner's name+UUID pairing |
| `scans` | Full-dome and area scans | Keyed by the scanner's scan UUID; the scanner's numeric ids shift |
| `jobs` | Queued and running work: scans, exports, measurements | One scanner job at a time; processing can run alongside |
| `grants` | Standing approval for scans within limits | Never covers deletes |
| `exports` | Decoded, levelled point files (PLY, LAS) | Every export carries automatic checks |
| `measurements` | Values with uncertainty, checks, and an evidence image | Python worker in v1 |
| `recipes`, `runs` | Recipes as data, and their execution | Same recipe runs from an agent or by hand in the web UI |
| `log` | The field log | Append-only; the bridge writes most entries itself |

Long work returns `202` + a `Job`. Clients follow it with `GET /jobs/{id}` or
the `/events` stream (server-sent events, whole objects, no re-fetch needed).

## Rules (checked at admission, and again before the scanner starts)

| Rule | Blocks | Error `code` | How it clears |
| --- | --- | --- | --- |
| Scanner connected | everything that needs the scanner | `scanner_disconnected` | Plug in and power the X9; the bridge brings the USB link up |
| One scanner job at a time | a second scan | queued, not refused | Waits its turn (`max_queue_length`) |
| Battery ≥ `min_battery_pct_to_scan` | scans | `battery_low` | Swap the battery or plug in |
| Storage ≥ `min_storage_free_bytes` | scans | `storage_low` | Free space on the scanner's card |
| Approval | scans by `agent` tokens | queued as `awaiting_approval` | An operator approves the job, or an active grant covers it |
| Not moved since the reference scan | `aim` | `instrument_moved` | Take a new preview scan at this station |
| Confirmed delete | `DELETE` | `confirm_mismatch` / `forbidden` | Operator token + `X-Confirm: <id>` |
| Known scan mode | scans | `invalid_request` | Use a name from `GET /scan-modes` |
| Scanner idle (no other client's task) | a scan about to start | job fails `scanner_busy` | Wait for the other task, then queue again |
| Echo matches the request | a scan just started | job fails, scanner task cancelled | The scanner answers 202 even when it can't parse a request and runs a default full dome; any difference stops it at once |

Rule settings are readable at `GET /rules` (admin can change them later; not in v1).

## Roles

| Role | Read | Queue scans | Queue exports / measurements | Approve, grant | Delete |
| --- | --- | --- | --- | --- | --- |
| `viewer` | yes | no | no | no | no |
| `agent` | yes | yes, needs approval or a grant | yes | no | no |
| `operator` | yes | yes | yes | yes | yes, with `X-Confirm` |
| `admin` | yes | yes | yes | yes | yes, with `X-Confirm` |

An AI agent gets an `agent` token. A person on the phone gets `operator`. So
a typical session is: the person grants "up to 12 scans in this project for
two hours", and the agent works inside that.

## Conventions

- `ha` is the instrument's horizontal angle as the scanner uses it (clockwise
  seen from above, `[0, 2π)`); `va` is measured from the zenith. Radians and
  metres throughout.
- Scan color is **off by default**: it costs ~150 s per station and
  measurement doesn't need it.
- Checks are part of every product. A client should not present a product
  whose checks failed without saying so.
- Measurements return an evidence image, so a person or a vision model can
  verify the result instead of trusting the numbers.

## How clients map onto it

- **MCP server:** done in `TrimbleX9-mcp` 0.2.0 bridge mode
  (`SCANNER_BRIDGE_URL` + `SCANNER_BRIDGE_TOKEN`): 24 thin tools, the preview
  returned as an image so the agent can look and draw the box, and the
  `run_recipe` prompt. It holds an `agent` token and enforces nothing itself;
  its old launch checks (idle, mode, echo) now live here.
- **Phone web UI:** status, approvals and grants, recipe runs as checklists,
  evidence images. Holds an `operator` token.
- **`x9` CLI:** thin subcommands over the same operations.
- **Other agent frameworks:** generate function definitions from this spec.

## Mapping to the scanner

The bridge translates to the scanner's HTTP API (`/api/v1` on port 34567) via
`x9-client`: `POST /scans` → scanner `POST /scans` with `scanType`
`FullDome`/`AreaScan`, `colorMode`, window angles; scan phases come from
polling `/tasks`; files from `/tzfs/{id}/file` and `/preview`. Decoding uses
the public `trimble-tzf` crate.

## Open questions

1. **Events transport:** server-sent events (simple, one-way, works through
   proxies) or WebSocket. Draft uses SSE.
2. **Recipe file format:** YAML matching the `Recipe` schema, stored in the
   separate recipes repository and installed onto the bridge?
3. **Measurement worker protocol:** a local HTTP worker, or a subprocess per
   job reading the export file and writing JSON + PNG? Subprocess is simplest.
4. **Visibility:** the bridge and this spec could be public and clean-room like
   the decoder; still undecided.
5. **COPC and the live viewer** are Phase 3; `ExportRequest.format` will grow
   `copc` then.
6. **Several scanners per bridge:** out of scope for v1 (one scanner per bridge).

## Implementation status (2026-10-20)

`crates/x9-bridge` (Rust, axum) implements: `health`, `status`, `rules`,
`scan-modes`, `projects`, `scans` (list, get, start, confirmed delete,
`preview`, `tzf`), `aim`, `exports` (PLY / LAS via `trimble-tzf`, with
automatic checks: point count, levelled, coordinates, intensity source),
`jobs` (list, get, cancel, approve), `grants`, `recipes` and `runs`, `log`,
and the `events` stream. Scans run one at a time through a worker that re-checks the rules
right before the scanner starts; exports run alongside. Scan files and
previews are fetched once and cached under `BRIDGE_DATA`.

Not yet: `measurements` (Python worker) and the scan's own intensity image
(component 4); exports take intensity from the preview.
State is in memory and rebuilt from the scanner on restart.

Testing: `cargo test -p x9-bridge` covers the rules, grants, aiming, recipe
validation and placeholders, and time parsing. `crates/x9-bridge/tests/e2e.py
target/release` runs 40 end-to-end checks against `x9-mock` (54 with
`E2E_FIXTURES` pointing at real scans, for the file, aim, export and full
recipe-run checks), which now simulates scans (phases, area scans, color,
cancel, positional ids).

Run it against the mock:

```bash
cargo build --release -p x9-mock -p x9-bridge
X9_MOCK_ADDR=127.0.0.1:34599 target/release/x9-mock &
printf 'jim operator %s\n' "$(openssl rand -hex 16)" > tokens
X9_BASE=http://127.0.0.1:34599/api/v1 BRIDGE_TOKENS=tokens target/release/x9-bridge
```

## Tested on the real X9 (2026-10-20)

Bridge on the Fedora box, scanner over its Wi-Fi (the scanner API at
`192.168.43.1:34567`, reached through the Pi). Agent token for requests,
operator token for approvals.

| Step | Result |
| --- | --- |
| `status`, `scan-modes`, `projects` | Mapped correctly (battery, leveling, moved flag; 8 modes) |
| Full dome, STD_L0, agent → awaiting approval → operator approves | Succeeded: leveling 62.5 s from unlevelled, sweep 30.9 s, done 100.9 s, 6,492,453 points |
| Grant (2 × STD_L2), two area scans by the agent | Both queued without asking, ran back to back: 102.4 s and 76.5 s; filed as area scans under station 1 |
| Field log | Request, scanner start and geometry-ready entries recorded |

Found and fixed: approval / cancel / finish log entries lacked the project id;
default port moved from 8790 (taken on the Fedora box) to 8795.

## Recipes v0 (2026-10-20)

Recipes are **TOML** files in `BRIDGE_RECIPES` (default `$BRIDGE_DATA/recipes`),
read on each request and validated: unique step ids, known operations, and
placeholders that refer only to inputs, `project_id` or earlier steps. Invalid
files are skipped and noted in the field log at startup. TOML rather than YAML:
comments and `[[steps]]` tables suit hand-written recipes. The canonical
recipes live in a separate recipes repository; a test copy of the window recipe
is in `crates/x9-bridge/tests/recipes/`.

| Step kind | What it does |
| --- | --- |
| `action` | Calls `startScan`, `aimFromPreview`, `exportPoints` or `addLogNote`, with arguments resolved from `{{input.x}}`, `{{project_id}}`, `{{steps.<id>.<field>}}` |
| `person` | Waits for `POST /runs/{id}/advance` with the fields in `needs` (e.g. a box drawn on the `preview_of` step's preview) |
| `check` | Requires the named checks of step `of` to be ok, or the run fails with `check_failed` |

A run executes under the role of whoever started it, so approvals and grants
apply as for direct requests; a grant can be limited to one run (`run_id`).
While a scan waits for approval the run shows `waiting_for: approval`; on a
person step, `waiting_for: input`. Cancelling a run cancels its scan in flight.

## Next

1. A minimal phone web UI: status, approvals, runs as checklists.
2. The measurement worker, so the window recipe's last step is automatic.
3. Run the bridge on the Pi (aarch64 build).
