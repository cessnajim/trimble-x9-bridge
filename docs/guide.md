# x9-bridge guide

This guide is for people who install, run and use the bridge. It covers
installation, configuration, tokens and roles, and the everyday workflows,
with `curl` examples. Every statement here describes what the code in
`crates/x9-bridge` does today.

Related documents:

- [recipes.md](recipes.md): the recipe format and how runs execute.
- [errors.md](errors.md): every error `code` the bridge returns.
- [tools.md](tools.md): the `x9` CLI, the `x9-mock` simulator and the tests.
- [../api/bridge-v1.openapi.yaml](../api/bridge-v1.openapi.yaml): the API spec.
- [bridge-api-v1.md](bridge-api-v1.md): design notes and test history.

## Contents

1. [What the bridge is](#what-the-bridge-is)
2. [Install](#install)
3. [Configuration](#configuration)
4. [Tokens](#tokens)
5. [Roles](#roles)
6. [Conventions](#conventions)
7. [Workflows](#workflows)
8. [Events](#events)
9. [Data directory and state](#data-directory-and-state)

## What the bridge is

The bridge is the one program that talks to the scanner. It runs on the
computer next to the scanner (a Raspberry Pi on the tripod, or a desktop at
the bench). It reaches the scanner API over USB or over the scanner's own
Wi-Fi, and it serves an HTTP API at `/v1` to every client: AI agents through
an MCP server, a phone web UI, scripts, `curl`.

The safety rules live in the bridge, so no client can skip them: battery,
storage, a busy scanner, a single scan queue, known scan modes, approvals for
agents, confirmed deletes, and an echo check that stops a scan the scanner did
not accept as sent.

## Install

### Release archives

Tagged releases have archives for Linux `x86_64` and `aarch64` (Raspberry Pi
4/5) on the
[releases page](https://github.com/cessnajim/trimble-x9-bridge/releases).
Each archive is named `trimble-x9-bridge-<tag>-<target>.tar.gz` and comes with
a `.sha256` file. It contains:

| File | What |
| --- | --- |
| `x9-bridge` | The bridge service |
| `x9-mock` | The scanner simulator ([tools.md](tools.md#x9-mock)) |
| `x9` | A small CLI over the scanner API ([tools.md](tools.md#the-x9-cli)) |
| `ops/` | `x9-bridge.service`, `x9-bridge-run`, `90-x9-bridge`, `x9-netctl`, `README.md` |
| `README.md`, `PROVENANCE.md`, `LICENSE` | |

```bash
tag=v0.1.2; target=aarch64-unknown-linux-gnu
curl -LO https://github.com/cessnajim/trimble-x9-bridge/releases/download/$tag/trimble-x9-bridge-$tag-$target.tar.gz
curl -LO https://github.com/cessnajim/trimble-x9-bridge/releases/download/$tag/trimble-x9-bridge-$tag-$target.tar.gz.sha256
sha256sum -c trimble-x9-bridge-$tag-$target.tar.gz.sha256
tar xzf trimble-x9-bridge-$tag-$target.tar.gz
```

### Build from source

You need a stable Rust toolchain. The build fetches the public
[trimble-tzf](https://github.com/cessnajim/trimble-tzf) decoder from GitHub.

```bash
git clone https://github.com/cessnajim/trimble-x9-bridge
cd trimble-x9-bridge
cargo build --release          # target/release/{x9-bridge,x9-mock,x9}
```

### First run

The bridge refuses to start without a token file (see [Tokens](#tokens)).

```bash
printf 'me operator %s\n' "$(openssl rand -hex 16)" > tokens
chmod 600 tokens
X9_BASE=http://192.168.42.1:34567/api/v1 BRIDGE_TOKENS=tokens ./x9-bridge
# x9-bridge 0.1.2 listening on http://127.0.0.1:8795/v1 (scanner http://192.168.42.1:34567/api/v1)
```

To try it without a scanner, start `x9-mock` first and point `X9_BASE` at it
(`http://127.0.0.1:34567/api/v1`). See [tools.md](tools.md#x9-mock).

On startup the bridge creates `BRIDGE_DATA` and `BRIDGE_RECIPES` if they are
missing, loads the recipes, writes a `rule` entry to the field log
("Bridge … started; scanner at …"), and prints how many recipes it loaded and
skipped. It stops cleanly on Ctrl-C (SIGINT).

The bridge starts even when the scanner is unreachable. `GET /status` then
shows `"connected": false`, and anything that needs the scanner fails with
`scanner_disconnected`.

### Running as a systemd service

`ops/x9-bridge.service` runs the bridge with a throwaway user
(`DynamicUser=yes`), a read-only system (`ProtectSystem=strict`,
`ProtectHome=yes`) and `/var/lib/x9-bridge` as its only writable place
(`StateDirectory=x9-bridge`, `BRIDGE_DATA=/var/lib/x9-bridge`). The token file
is passed as a systemd credential (`LoadCredential=tokens:/etc/x9-bridge/tokens`),
so the bridge never reads `/etc/x9-bridge` itself. It restarts on failure
after 5 s.

```bash
sudo install -D -m 0755 x9-bridge ops/x9-bridge-run -t /opt/x9-bridge/
sudo install -D -m 0600 tokens /etc/x9-bridge/tokens
sudo install -m 0644 ops/x9-bridge.service /etc/systemd/system/
sudo systemctl enable --now x9-bridge
journalctl -u x9-bridge -f
```

The unit starts `x9-bridge-run`, a small wrapper that:

- picks the scanner link when `X9_BASE` is not set: USB
  (`http://192.168.42.1:34567/api/v1`) if `GET /tasks` answers within 2 s,
  else the scanner's Wi-Fi (`http://192.168.43.1:34567/api/v1`), else USB;
- sets `BRIDGE_TOKENS` to `$CREDENTIALS_DIRECTORY/tokens` when it is not set;
- runs `x9-bridge` from its own directory.

Put overrides (`X9_BASE`, `BRIDGE_ADDR`, `BRIDGE_POLL_MS`, …) in
`/etc/x9-bridge/env`, one `NAME=value` per line. The file is optional.

The link is chosen only at start. With NetworkManager, the dispatcher hook
`ops/90-x9-bridge` runs `systemctl try-restart x9-bridge.service` whenever a
connection named in `X9_NM_CONNECTIONS` comes up (default `x9-usb x9-wifi`;
edit the script to match your profile names):

```bash
sudo install -m 0755 ops/90-x9-bridge /etc/NetworkManager/dispatcher.d/
```

### The USB link

Linux binds the scanner's USB port as a network device (usually `cdc_eem`) and
gets an address on 192.168.42.0/24 by DHCP. `ops/x9-netctl` is a small
privileged helper that brings this link up isolated: never a default route,
never the scanner's DNS, a high route metric. Its verbs `usb-up`, `usb-info`
and `usb-down` manage that link; the others manage the uplink of the machine
it was written on, so review the defaults at the top before use. See
[../ops/README.md](../ops/README.md) for installing it with a narrow sudo rule.

## Configuration

All configuration is by environment variable. Nothing else is read at startup
except the token file and the recipe directory.

| Variable | Default | What |
| --- | --- | --- |
| `X9_BASE` | `http://192.168.42.1:34567/api/v1` | The scanner API base URL. USB is `192.168.42.1`, the scanner's Wi-Fi is `192.168.43.1`, the simulator is `127.0.0.1:34567` by default. |
| `BRIDGE_ADDR` | `127.0.0.1:8795` | Listen address. The API is served under `/v1`. Use `0.0.0.0:8795` (or a specific interface address) to reach it from other machines. |
| `BRIDGE_TOKENS` | none; **required** | Path to the token file. The bridge exits if it is unset, unreadable, malformed or empty. |
| `BRIDGE_DATA` | `$HOME/.local/share/x9-bridge` (`./.local/share/x9-bridge` if `HOME` is unset) | Cache for scan files, previews and exports. Created if missing. |
| `BRIDGE_RECIPES` | `$BRIDGE_DATA/recipes` | Directory of recipe files (`*.toml`). Created if missing. Read again on every recipe request, so new files need no restart. |
| `BRIDGE_POLL_MS` | `1000` | How often, in milliseconds, the worker polls the scanner while a scan runs, while it waits for the scanner to go idle, and after a cancel. Must be a whole number. |

`BRIDGE_POLL_MS` also scales two timeouts: a scan counts as done once the
scanner has been idle for 3 polls with the scan file in hand, and fails if the
scanner goes idle for 10 polls without filing a scan.

Fixed limits (not configurable):

| Limit | Value |
| --- | --- |
| Longest scan before the job fails | 45 minutes |
| Consecutive failed scanner polls before a running scan job fails | 10 |
| Wait for a task the bridge cancelled to clear | 60 s |
| Event buffer per client | 256 events |

### Rules

The rule settings are fixed at their defaults in this version. Anyone with a
token can read them:

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8795/v1/rules
```

| Rule | Default | Effect |
| --- | --- | --- |
| `min_battery_pct_to_scan` | `15` | Scans are refused (`battery_low`) below this, unless the scanner is on AC power. |
| `min_storage_free_bytes` | `2000000000` | Scans are refused (`storage_low`) when the scanner's card has less free space. |
| `require_approval_for_scans` | `true` | Scans from `agent` tokens wait for approval unless a grant covers them. |
| `refuse_aim_after_move` | `true` | `POST /aim` is refused (`instrument_moved`) when the scanner reports it moved since its last scan. |
| `max_queue_length` | `20` | Scan requests are refused (`conflict`) when this many scan jobs are already queued or awaiting approval. |

Battery and storage are checked only when the scanner reports them. They are
checked at admission and again right before the scanner starts.

## Tokens

The token file has one token per line:

```text
# name    role      token
phone     operator  4f1c0d8e9b2a7f3c5e6d1a0b9c8d7e6f
agent-1   agent     9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d
wall      viewer    0123456789abcdef0123456789abcdef
```

Rules for the file:

- Blank lines and lines that start with `#` (after leading spaces) are skipped.
  A `#` later in a line is not a comment.
- Every other line must have exactly three fields separated by whitespace:
  `name role token`. Anything else stops the bridge with an error naming the
  file and line ("expected \`name role token\`").
- `role` is one of `viewer`, `agent`, `operator`, `admin`, lower case. An
  unknown role stops the bridge.
- `token` must be at least 16 characters. A shorter one stops the bridge.
  `openssl rand -hex 16` gives 32 characters.
- The file must hold at least one token.
- `name` is what the bridge records as the author of log entries, as
  `requested_by` on jobs, `granted_by` on grants and `started_by` on runs.
  Names need not be unique. If the same token appears twice, the later line
  wins.
- The file is read once at startup. Restart the bridge to change tokens.

Clients send the token as `Authorization: Bearer <token>` (the word `Bearer`
with a capital B and one space). Every route except `GET /health` needs a
valid token; a missing or unknown token gets `401 unauthorized`.

## Roles

| Action | `viewer` | `agent` | `operator` | `admin` |
| --- | --- | --- | --- | --- |
| Read: status, rules, scan modes, projects, scans, previews, scan files, jobs, grants, exports, recipes, runs, log, events | yes | yes | yes | yes |
| Turn a preview box into a window (`POST /aim`) | yes | yes | yes | yes |
| Create a project | no | yes | yes | yes |
| Queue a scan | no | yes; waits for approval unless a grant covers it | yes, queued at once | yes, queued at once |
| Cancel a job | no | only jobs it requested (same token name) | any job | any job |
| Approve a job | no | no | yes | yes |
| Create or revoke a grant | no | no | yes | yes |
| Start an export | no | yes | yes | yes |
| Add a log note | no | yes | yes | yes |
| Start a recipe run | no | yes | yes | yes |
| Advance or cancel a recipe run | no | only runs it started (same token name) | any run | any run |
| Delete a scan | no | no | yes, with `X-Confirm` | yes, with `X-Confirm` |

`admin` has exactly the rights of `operator` in this version. Changing rules
and tokens over the API is not implemented.

A typical setup: an AI agent gets an `agent` token, a person on the phone gets
an `operator` token. The person grants "up to 12 scans in this project for two
hours", and the agent works inside that.

## Conventions

- Base URL: `http://127.0.0.1:8795/v1` with the default `BRIDGE_ADDR`.
- JSON, `snake_case`. Request bodies are parsed as JSON whatever the
  `Content-Type`; unknown fields are ignored.
- Lengths in metres, angles in radians, times in RFC 3339 UTC
  (`2026-10-20T12:40:00Z`), durations in seconds.
- `ha` is the scanner's horizontal angle (clockwise seen from above,
  `[0, 2π)`). `va` is the vertical angle from the zenith (`0` is straight up,
  `π` straight down).
- Ids are made by the bridge with a readable prefix: `job_…`, `grant_…`,
  `exp_…`, `run_…`, `log_…`. Projects and scans use the scanner's own UUIDs
  (`project_id`, `scan_id`); the scanner's numeric scan ids shift when a scan
  is deleted, so the bridge never exposes them.
- Long work (scans, exports) answers `202 Accepted` with a `Job`. Follow it
  with `GET /jobs/{job_id}` or the [event stream](#events).
- Errors are `application/problem+json` (RFC 9457) with a `code`; see
  [errors.md](errors.md).

The examples below assume:

```bash
B=http://127.0.0.1:8795/v1
TOKEN=...      # an operator token unless stated
AGENT=...      # an agent token
H="Authorization: Bearer $TOKEN"
```

## Workflows

### Check the bridge and the scanner

```bash
curl -s $B/health
# {"ok":true,"version":"0.1.2"}

curl -s -H "$H" $B/status
```

```json
{
  "bridge": {"version": "0.1.2", "started_at": "2026-10-20T12:00:00Z",
             "queue_length": 0, "worker_ready": false},
  "scanner": {
    "connected": true,
    "busy": false,
    "current_job_id": null,
    "battery_pct": 87,
    "on_ac_power": false,
    "storage_free_bytes": 24356585472,
    "moved_since_last_scan": false,
    "leveling": {"state": "ok", "compensator_on": true, "tilt_rad": [0.0155, 0.023]}
  }
}
```

| Field | Meaning |
| --- | --- |
| `bridge.queue_length` | Scan jobs `queued` or `awaiting_approval` |
| `bridge.worker_ready` | Whether the measurement worker is available. Always `false`: the worker is not built yet. |
| `scanner.connected` | `false` when the scanner did not answer. The other scanner fields are then missing. |
| `scanner.busy` | The scanner reports a current task (from any client) |
| `scanner.current_job_id` | The bridge's scan job in state `running`, if any. Exports run beside the scanner and never show here. |
| `scanner.leveling.state` | `ok`, `none`, `error`, or `in_progress` for any other value the scanner reports |
| `scanner.leveling.tilt_rad` | The two compensator readings, radians (`0` when missing) |

Scan modes, with the scanner's own figures:

```bash
curl -s -H "$H" $B/scan-modes
# [{"mode":"STD_L0","range_m":30,"angular_step_rad":0.00150796,"points_full_dome":6799768,
#   "fixed_seconds":38,"full_dome_seconds":50}, ...]
```

`fixed_seconds` is the overhead every scan pays; `full_dome_seconds` is the
scanner's estimate for a full dome with color off.

### Create and list projects

```bash
curl -s -H "$H" -d '{"name":"kitchen"}' $B/projects
# 201 {"project_id":"<uuid>","name":"kitchen","scan_count":0}

curl -s -H "$H" $B/projects
PROJECT=<the project_id>
```

- `name` is trimmed, must be 1–64 bytes and must not contain `/`.
- A name that already exists on the scanner is `409 conflict`.
- The bridge generates the project UUID.
- `scan_count` is the number of stations (full-dome scans) the scanner has
  numbered in the project; area scans are not counted.
- Creating a project writes a `note` to the field log.

### Start a full-dome scan

```bash
curl -s -H "$H" -d "{\"project_id\":\"$PROJECT\",\"kind\":\"full_dome\",\"mode\":\"STD_L0\",\"note\":\"station 1\"}" $B/scans
```

```json
{"job_id": "job_3f9a1c2b7d4e", "kind": "scan", "state": "queued",
 "requested_by": "phone", "run_id": null, "created_at": "2026-10-20T12:01:00Z",
 "started_at": null, "finished_at": null}
```

Request fields:

| Field | Required | What |
| --- | --- | --- |
| `project_id` | yes | A project on the scanner (`404 not_found` otherwise) |
| `kind` | yes | `full_dome` or `area` |
| `mode` | yes | A mode name from `GET /scan-modes` (`400 invalid_request` lists the valid ones) |
| `color` | no, default `false` | Camera images after the sweep. Adds about 150 s per scan. |
| `window` | for `area` only | See below. A full dome with a window is refused. |
| `note` | no | Added to the "scan requested" log entry |

How a scan request is admitted, in order:

1. The request is validated (mode not empty; window rules).
2. The mode must be one the scanner lists. This asks the scanner, so an
   unreachable scanner gives `503 scanner_disconnected` here.
3. The project must exist on the scanner.
4. Viewers are refused (`403 forbidden`). The role is checked first, so a
   viewer gets `forbidden` even for a malformed request.
5. Connected, battery and storage rules (see [Rules](#rules)).
6. The queue must have room (`max_queue_length`).
7. An `agent` scan not covered by a grant is held as `awaiting_approval`.
   Everything else is `queued`.

Queued scans run one at a time, oldest first. Right before the scanner starts,
the worker checks connected, battery and storage again, and checks that the
scanner is idle:

- If the scanner lists a task the bridge did not start (another client is
  driving it), the job fails at once with `scanner_busy`.
- If it lists only a task the bridge itself cancelled, the job waits for it
  to clear, up to 60 s, and logs "Waiting for the scanner to clear the
  cancelled task". After 60 s the job fails with `scanner_busy`. While it
  waits, the job reads `running` with phase `waiting`, then `leveling` once
  the scanner is clear.

Then the bridge sends the scan and compares the scanner's echo of the request
with what it sent. The scanner answers `202` even when it cannot parse a
request and then runs a default full dome. On any difference the bridge
cancels the scanner's task at once and the job fails with `conflict`
("the scanner did not accept the request as sent (…); its task was cancelled").

Follow the job:

```bash
curl -s -H "$H" $B/jobs/job_3f9a1c2b7d4e
```

| `state` | Meaning |
| --- | --- |
| `awaiting_approval` | An agent's scan waiting for an operator |
| `queued` | Waiting its turn |
| `running` | The worker has it |
| `succeeded` | Done. `result` is `{"scan_id": "<uuid>"}` (exports: `{"export_id": "exp_…"}`) |
| `failed` | `problem` holds the error (a problem document, see [errors.md](errors.md)) |
| `cancelled` | Cancelled; see [Cancel a job](#cancel-a-job) |

| `phase` | Meaning |
| --- | --- |
| `waiting` | Claimed, but the scanner still lists a task the bridge cancelled; the job waits for it to clear |
| `leveling` | Claimed and about to start, or the scanner is levelling (its task is not yet `running`) |
| `sweeping` | The laser sweep; also the few seconds after the file appears with color off |
| `imaging` | Color images, after the scan file appeared (color on only) |
| `downloading`, `decoding` | Export jobs |

`progress` is the scanner's own task percentage (0–1): it climbs to 1 during
the sweep, then starts again in steps of 1/30 during color imaging. It is set
to `1` when the job succeeds. `phase` is removed when a job finishes.

`GET /jobs` lists every job since the bridge started, newest first.

When the scan is done, read the scan record:

```bash
curl -s -H "$H" $B/scans/$SCAN
```

```json
{"scan_id": "<uuid>", "project_id": "<uuid>", "parent_scan_id": null, "station": 1,
 "kind": "full_dome", "mode": "STD_L0", "color": false,
 "created_at": "2026-10-20T12:01:00Z", "point_count": 6492453, "tzf_bytes": 58432077,
 "timings": {"leveling_s": 62.5, "sweep_s": 30.9, "geometry_ready_s": 93.4, "done_s": 100.9}}
```

- `GET /scans` lists all scans on the scanner, newest first;
  `?project_id=<uuid>` filters to one project.
- `kind` and `station` come from the scanner's file name: `N.tzf` is the
  full-dome scan of station `N`, `N_k.tzf` is area scan `k` of station `N`,
  and its `parent_scan_id` is station `N`'s scan.
- `created_at`, `timings` and `window` are known only for scans this bridge
  ran since it started; the scanner does not record them. `created_at` is
  when the job started.
- `timings` are seconds from the moment the bridge sent the scan:
  `leveling_s` when the task first reads `running`, `geometry_ready_s` when the
  scan file appeared, `sweep_s` the difference, `done_s` when the job ended.

### Start an area scan

An area scan covers a window of the dome:

```bash
curl -s -H "$H" -d "{\"project_id\":\"$PROJECT\",\"kind\":\"area\",\"mode\":\"STD_L2\",
  \"window\":{\"start_ha\":5.157,\"delta_ha\":0.456,\"start_va\":1.262,\"delta_va\":0.441}}" $B/scans
```

Window rules (else `400 invalid_request`):

- `0 ≤ start_ha < 2π`, `0 < delta_ha ≤ 2π`. The window may cross the `2π`
  seam: `start_ha + delta_ha` can exceed `2π`.
- `0 ≤ start_va ≤ π`, `delta_va > 0`, `start_va + delta_va ≤ π`.

The scanner files an area scan under the latest station of the project, even
when that station's full dome was cancelled. Use [aiming](#previews-and-aiming)
to get a window from a box drawn on a preview.

### Approvals: an agent's scan

```bash
# The agent asks.
curl -s -H "Authorization: Bearer $AGENT" \
  -d "{\"project_id\":\"$PROJECT\",\"kind\":\"full_dome\",\"mode\":\"STD_L0\"}" $B/scans
# 202 {"job_id":"job_…","state":"awaiting_approval",...}

# An operator sees it (GET /jobs, the event stream, or the field log) and approves.
curl -s -X POST -H "$H" $B/jobs/$JOB/approve
# 200 {"job_id":"job_…","state":"queued",...}
```

- Only `operator` and `admin` can approve (`403 forbidden` otherwise).
- Only a job in `awaiting_approval` can be approved (`409 conflict`
  otherwise, for example "job is queued, not awaiting approval").
- Approval writes an `approval` entry to the field log. The rules are checked
  again when the job starts, not at approval.
- To refuse a job, cancel it.
- A job waits for approval indefinitely; there is no timeout.

### Grants: standing approval for an agent

A grant lets an agent's scans queue without asking, within limits.

```bash
curl -s -H "$H" -d "{\"project_id\":\"$PROJECT\",\"expires_at\":\"2026-10-20T16:00:00Z\",
  \"max_scans\":12,\"allowed_modes\":[\"STD_L0\",\"STD_L2\"],\"allow_color\":false}" $B/grants
```

```json
{"project_id": "<uuid>", "expires_at": "2026-10-20T16:00:00Z", "max_scans": 12,
 "allowed_modes": ["STD_L0", "STD_L2"], "allow_color": false, "run_id": null,
 "grant_id": "grant_8c1d2e3f4a5b", "granted_by": "phone", "scans_used": 0}
```

| Field | Required | What |
| --- | --- | --- |
| `project_id` | yes | Scans in this project only. Not checked against the scanner. |
| `expires_at` | yes | RFC 3339 (`Z` or `±HH:MM` offset, optional fraction). Must be in the future. |
| `max_scans` | no | At most this many scans. Omitted: no limit until it expires. |
| `allowed_modes` | no | Only these modes. Empty or omitted: any mode. |
| `allow_color` | no, default `false` | Whether color scans are covered |
| `run_id` | no | Only scans started by this recipe run. Not checked against existing runs. |

A grant covers a scan request when all of these hold: it has not expired, the
project matches, `scans_used < max_scans` (if set), the mode is allowed, the
request has no color or the grant allows color, and the grant has no `run_id`
or the request comes from that run. A grant does not limit the scan kind:
full-dome and area scans are both covered.

- The first covering grant, in creation order, is used. Its `scans_used`
  goes up by one when the agent's scan is queued. If the job is cancelled
  before the scanner starts it (while `queued`, or while it waits for a
  cancelled task to clear), the use is given back. It is not given back once
  the scanner has the scan, or if the job fails.
- Grants matter only for `agent` tokens. Operator scans never use one.
- Grants never cover deletes.
- `GET /grants` lists grants that have not expired, including used-up ones.
  Expired grants are hidden but stay in memory.
- Creating a grant writes an `approval` entry to the field log.

A grant limited to one run. Start the run first, then grant scans for its
id:

```bash
RUN=$(curl -s -H "Authorization: Bearer $AGENT" \
  -d "{\"recipe_id\":\"window-measurement\",\"project_id\":\"$PROJECT\",\"inputs\":{\"purpose\":\"blinds\"}}" \
  $B/runs | jq -r .run_id)
curl -s -H "$H" -d "{\"project_id\":\"$PROJECT\",\"expires_at\":\"2026-10-20T16:00:00Z\",
  \"max_scans\":2,\"run_id\":\"$RUN\"}" $B/grants
```

Grants are checked only when a scan is requested. A run's first scan is
usually requested before the grant exists, so it waits for approval: approve
it by hand. Later scans of the run use the grant.

Revoke a grant:

```bash
curl -s -X DELETE -H "$H" $B/grants/$GRANT     # 204; 404 not_found if unknown
```

### Cancel a job

```bash
curl -s -X POST -H "$H" $B/jobs/$JOB/cancel
```

- `viewer` cannot cancel. An `agent` can cancel only jobs whose
  `requested_by` is its own token name.
- `queued` or `awaiting_approval`: the job is `cancelled` at once.
- `running` scan: the bridge marks the job for cancelling. On its next poll
  (within `BRIDGE_POLL_MS`) the worker sends the scanner a cancel for its task,
  and the job reads `cancelled`. The answer to the cancel request itself may
  still show `running`; follow the job or the event stream.
- A job that has already finished is returned unchanged with `200`.
- Every cancel request writes a "Cancel requested" `scan` entry to the field
  log.
- `running` export: the export stops at its next check: during the download,
  between decoding and writing, or while writing (every 65,536 points). It
  then reads `cancelled`, with no result and no export file. Decoding the
  scan file itself cannot be interrupted, so on a big scan the stop can take
  that long. Cancelling a run cancels its export the same way.

What happens after the scanner is told to stop:

- The scanner keeps its task listed for a while (about 13 s on a real
  scanner). The bridge remembers that it cancelled that task. The next queued
  scan waits for it to clear, up to 60 s, instead of failing with
  `scanner_busy`.
- If the scanner was already sweeping, it files what it had swept as a scan.
  The bridge watches for that scan for up to 60 s. When it appears, the
  cancelled job gains `partial_scan_id` (announced by a `job` event), and the
  field log says: "The scanner kept a partial scan from cancelled job …: …,
  N points. Keep it or delete it." The scan record gets `created_at` and the
  requested `window`.
- A scan cancelled before its sweep started (still levelling) leaves no file
  and no `partial_scan_id`.

```json
{"job_id": "job_…", "kind": "scan", "state": "cancelled", "progress": 0.05,
 "finished_at": "2026-10-20T12:05:13Z", "partial_scan_id": "<uuid>", ...}
```

Keep the partial scan or [delete it](#delete-a-scan).

### Previews and aiming

Each scan has a preview: a grid-aligned intensity panorama as a JPEG.

```bash
curl -s -H "$H" -o station1.jpg $B/scans/$SCAN/preview
```

Draw a box on it (pixels, `x` to the right, `y` down) and turn it into an
area-scan window:

```bash
curl -s -H "$H" -d "{\"reference_scan_id\":\"$SCAN\",
  \"box\":{\"x0\":523,\"y0\":418,\"x1\":674,\"y1\":564},\"margin_rad\":0.009}" $B/aim
```

```json
{"window": {"start_ha": 5.148, "delta_ha": 0.474, "start_va": 1.253, "delta_va": 0.459},
 "preview_size": [2082, 815],
 "cells_per_pixel": [2.0005, 2.0012]}
```

How the box maps to angles:

- The bridge reads the reference scan's grid from its scan file: columns,
  rows, start angles and angular steps. Preview pixels are scaled to grid
  cells by `cells_per_pixel` (grid size ÷ preview size).
- `start_ha` = scan start + `x0` × cells per pixel × step − margin, wrapped
  into `[0, 2π)`. `delta_ha` = box width in cells × step + 2 × margin, at most
  `2π`.
- `start_va` = scan start + `y0` in cells × step − margin, at least `0`.
  The window ends at `y1` + margin, at most `π`.
- `margin_rad` pads every side. Default `0.009` rad (about 0.5°). Negative
  values count as `0`.
- **Seam wrap:** a box with `x1 < x0` crosses the panorama's left/right edge.
  Its width is `x1 + preview width − x0`.

Box rules (else `400 invalid_request`): `0 ≤ x0, x1 ≤ preview width`,
`x0 ≠ x1`, `0 ≤ y0 < y1 ≤ preview height`.

Other things to know:

- `POST /aim` only computes; it does not scan. Pass the `window` to
  `POST /scans` with `kind: "area"`.
- Any role may call it, including `viewer`.
- It is refused with `409 instrument_moved` when the scanner reports it moved
  since its last scan. The flag is the scanner's own: the bridge does not check
  that the reference scan is the latest one. If the scanner does not answer,
  this check is skipped.
- It downloads the reference scan's file and preview on first use (cached
  afterwards), so the first call on a big scan takes a while. Like every
  scan file request, it asks the scanner whether the scan still exists first.
- A scan file the decoder cannot read gives `422 decode_failed`.

### Export points

Decode a scan into a levelled point file:

```bash
curl -s -H "$H" -d '{"format":"ply","intensity":"auto"}' $B/scans/$SCAN/exports
# 202 {"job_id":"job_…","kind":"export","state":"running","phase":"downloading",...}
```

| Field | Values | What |
| --- | --- | --- |
| `format` | `ply`, `las` | Required |
| `intensity` | `auto` (default), `scan`, `preview`, `none` | Where the grey value of each point comes from |

- **Formats.** PLY: binary little-endian, `x y z` as 32-bit floats and
  `red green blue` as bytes (the same grey value three times). LAS: version
  1.2, point data format 0, 0.1 mm scale, offsets at the minimum corner,
  intensity as 16-bit grey.
- **Levelling.** Points are always levelled: the decoder applies the tilt
  correction recorded in the scan file. There is no option to turn it off.
  Coordinates are metres in the scanner's own frame.
- **Intensity.** The scan file's own intensity is not decoded yet. `auto` and
  `scan` use the scan's preview (grid-aligned) when it can be downloaded, and
  mid-grey (128) otherwise. `preview` uses the preview and fails the export if
  there is none. `none` writes mid-grey and skips the preview download.
- Exports run alongside scans, not in the scan queue; they never start the
  scanner. They need no approval, but `viewer` cannot start one.
- The job's phases are `downloading` (fetching the scan file, unless cached)
  and `decoding`. On success `result` is `{"export_id": "exp_…"}`.
- A running export can be cancelled; see [Cancel a job](#cancel-a-job).

Read the export and its checks, then download the file:

```bash
curl -s -H "$H" $B/exports/$EXPORT
curl -s -H "$H" -OJ $B/exports/$EXPORT/file     # saves exp_….ply
```

```json
{"export_id": "exp_…", "scan_id": "<uuid>", "format": "ply", "point_count": 777580,
 "bytes": 11663954, "bounds_m": {"min": [-1.2, 2.1, -0.4], "max": [0.9, 3.4, 1.6]},
 "checks": [
   {"name": "point_count", "ok": true, "value": 777580, "unit": "points", "detail": "777580 decoded, 777580 declared by the file"},
   {"name": "levelled", "ok": true, "value": 0.031, "unit": "deg", "detail": "tilt correction of 0.031° applied (the compensator's range is a few degrees)"},
   {"name": "coordinates", "ok": true, "value": 4.212, "unit": "m", "detail": "all coordinates finite and within the scanner's range"},
   {"name": "intensity", "ok": true, "detail": "from the scanner preview (grid-aligned)"}
 ]}
```

The automatic checks:

| Check | Passes when | What a failure means |
| --- | --- | --- |
| `point_count` | Decoded points = the count the file declares. `value`: points decoded. | The decode lost or invented points; do not trust the export. |
| `levelled` | The tilt correction is under 5°. `value`: the correction's angle in degrees. | The correction is outside what the compensator can produce; the levelling data is suspect. |
| `coordinates` | Every coordinate is finite and every point is within 400 m of the scanner. `value`: the largest distance, metres. | Garbage coordinates in the decode. |
| `intensity` | Always ok. `detail` says the source: the preview, or none (mid-grey). | Informational only. |

A failed check does not fail the export job: the job succeeds and the field log
gets a `check` entry ("Export …: checks failed: …"). A client should not
present an export whose checks failed without saying so. A file the decoder
cannot read fails the job with `decode_failed`.

Export metadata lives in memory. After a restart, `GET /exports/{id}` is
`404` even though the file is still in the data directory.

### Download the raw scan file

```bash
curl -s -H "$H" -OJ $B/scans/$SCAN/tzf          # saves <scan_id>.tzf
```

The file is the scanner's own `.tzf`, unchanged. The bridge downloads it once
and serves it from its cache afterwards, but only while the scanner still
lists the scan: each request asks the scanner first. Decode it yourself with
[trimble-tzf](https://github.com/cessnajim/trimble-tzf). A scan with no file
is `404 not_found` ("No such scan file").

### Delete a scan

Deleting needs an `operator` or `admin` token **and** an `X-Confirm` header
that repeats the scan id exactly:

```bash
curl -s -X DELETE -H "$H" -H "X-Confirm: $SCAN" $B/scans/$SCAN
# 204 No Content
```

- Without the header, or with a different value: `412 confirm_mismatch`
  ("Repeat the scan id in the X-Confirm header.").
- `viewer` or `agent`: `403 forbidden`. The role is checked before the header.
- The bridge looks up the scanner's numeric id for the scan right before it
  deletes, because those ids shift.
- An unknown scan is `404 not_found`.
- The delete writes a `note` entry ("Scan … deleted") to the field log.
- Deleting removes the scan from the scanner, and the bridge removes its
  cached file and preview and every export made from it since the bridge
  started. After that, the scan, its preview, its file and those exports are
  all `404 not_found` (see [Data directory](#data-directory-and-state)).
- Grants never cover deletes, and recipes cannot delete.

### Field log

The field log is the record of a session. The bridge writes most entries
itself; people and agents add notes.

```bash
curl -s -H "$H" "$B/log?project_id=$PROJECT&since=2026-10-20T12:00:00Z"
curl -s -H "$H" -d "{\"text\":\"Moved the plant out of the way\",\"project_id\":\"$PROJECT\"}" $B/log
# 201 {"entry_id":"log_…","at":"…","kind":"note","text":"…","author":"phone","project_id":"…","refs":{}}
```

- `GET /log` returns entries oldest first. `since` (RFC 3339) keeps entries
  at or after that time; `project_id` keeps entries for one project. Both are
  optional.
- `POST /log`: `text` is required, 1–4000 bytes and not only spaces;
  `project_id` is optional and not checked. `viewer` cannot write. The entry's
  `kind` is `note` and its `author` is the token name.
- The log is append-only and in memory: it starts empty on each bridge start.

Entry fields: `entry_id`, `at`, `kind`, `text`, `author` (token name or
`bridge`), `project_id` (or `null`), `refs` (related ids such as `job_id`,
`scan_id`, `run_id`, `step_id`, `grant_id`, `export_id`).

| `kind` | Written when |
| --- | --- |
| `note` | A person's or agent's note, a project created, a scan deleted, a recipe's `addLogNote` step |
| `scan` | A scan requested, the scanner started, geometry ready, waiting for a cancelled task, cancel requested, a scan job finished or cancelled, a partial scan kept |
| `export` | An export requested or finished |
| `approval` | A job approved, a grant created or revoked |
| `run` | A run started, waiting for a person, finished or cancelled |
| `check` | An export's checks failed |
| `rule` | The bridge started; a recipe file was skipped |
| `error` | A job or run failed |

### Recipes and runs

Recipes are TOML files that describe a scanning job as steps; a run executes
one. See [recipes.md](recipes.md) for the format and the run lifecycle. In
short:

```bash
curl -s -H "$H" $B/recipes
curl -s -H "$H" $B/recipes/window-measurement
curl -s -H "$H" -d "{\"recipe_id\":\"window-measurement\",\"project_id\":\"$PROJECT\",\"inputs\":{\"purpose\":\"blinds\"}}" $B/runs
curl -s -H "$H" $B/runs/$RUN
curl -s -H "$H" -d '{"step_id":"box","input":{"box":{"x0":523,"y0":418,"x1":674,"y1":564}}}' $B/runs/$RUN/advance
curl -s -X POST -H "$H" $B/runs/$RUN/cancel
```

Only the run's starter, an operator or an admin may advance or cancel it;
another agent gets `403 forbidden`.

## Events

`GET /events` is a server-sent event stream (`text/event-stream`). Any valid
token can open it.

```bash
curl -sN -H "$H" $B/events
```

```text
event: job
data: {"job_id":"job_…","kind":"scan","state":"running","phase":"sweeping","progress":0.4,...}

event: log
data: {"entry_id":"log_…","at":"…","kind":"scan","text":"Scanner started STD_L0 (color off)",...}
```

Each event's `data` is one whole JSON object on one line, so a client never
needs to re-fetch.

| Event | `data` | Sent when |
| --- | --- | --- |
| `job` | A `Job`, as from `GET /jobs/{id}` | A job is created, claimed by the worker, approved, cancelled, finished, or gains `partial_scan_id`; and on every scanner poll while a scan runs (about once per `BRIDGE_POLL_MS`, even when nothing changed); when an export moves to `decoding` |
| `log` | A `LogEntry`, as from `GET /log` | Every new field-log entry |
| `run` | A `Run`, as from `GET /runs/{id}` | Every change to a run: step started, waiting, done, advanced, cancelled, finished |

Things to know:

- There is no `status` event in this version. Poll `GET /status` for battery,
  levelling and connection.
- Events are not stored or replayed. Read the current state with the `GET`
  routes after you connect.
- A client that falls more than 256 events behind silently skips the ones it
  missed.
- The stream sends keep-alive comments when idle.
- Event `id` fields are not sent.

## Data directory and state

Files under `BRIDGE_DATA`:

```text
$BRIDGE_DATA/
  scans/
    <scan_id>.tzf            raw scan file, downloaded on first use
    <scan_id>.jpg            preview, downloaded on first use
    <scan_id>.<ext>.part-dl_…   a download in progress (renamed when complete)
  exports/
    exp_<id>.ply | .las      export files
  recipes/                   default BRIDGE_RECIPES
    *.toml
```

- Scan files and previews are fetched from the scanner once, then served from
  the cache. They are immutable on the scanner, so the cache does not go stale
  while the scan exists.
- Every request for a cached file first asks the scanner whether it still
  lists the scan. If it doesn't (deleted through the bridge, the vendor app or
  anything else), the cached file and preview are removed and the request is
  `404 not_found`. If the scanner can't be reached, the request fails with
  `503 scanner_disconnected` rather than serve a file that may be deleted.
- `DELETE /scans/{id}` removes the scan's cached files and the exports made
  from it since the bridge started. Exports from before a restart are not
  tracked (see below) and are never removed; nor are old exports of scans that
  still exist. Clean the directory by hand when needed.
- A download that fails or is cancelled removes its `.part-dl_…` file.
- When the bridge cannot read or write this directory (permissions, a full
  disk), requests fail with `500 cache_error`.
- Scan ids in paths must be hexadecimal with dashes (up to 64 characters);
  anything else is `404`, so a path cannot leave the cache directory.

What is kept only in memory and lost on restart:

- jobs, approvals and grants;
- runs;
- the field log;
- export metadata (the files stay on disk but cannot be fetched through the
  API);
- per-scan `created_at`, `timings` and `window`.

Projects and scans are always read live from the scanner, so they survive a
restart.
