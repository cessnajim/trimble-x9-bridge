# Errors

Every error the bridge returns is a problem document (RFC 9457) with a
machine-readable `code`. This page lists every code the code base can produce
(`crates/x9-bridge/src/problem.rs` and the places that call `ApiError::new`),
with its HTTP status, when it happens, and its hint. See [guide.md](guide.md)
for the workflows these errors belong to.

## Shape

```http
HTTP/1.1 409 Conflict
Content-Type: application/problem+json

{
  "type": "https://x9-bridge.local/problems/battery_low",
  "title": "Battery too low to scan",
  "status": 409,
  "code": "battery_low",
  "detail": "battery 12% is below 15%",
  "hint": "Swap the battery or plug the scanner in."
}
```

| Field | Always | What |
| --- | --- | --- |
| `type` | yes | `https://x9-bridge.local/problems/<code>`. An identifier, not a page you can fetch. |
| `title` | yes | Short, human-readable summary |
| `status` | yes | The HTTP status |
| `code` | yes | The value to branch on |
| `detail` | no | What exactly went wrong, often with values |
| `hint` | no | What a person or client can do to clear it |

Branch on `code`, not on `title` or `detail`: their wording may change.

### Where problems appear

1. **As the response** to a request that fails at once.
2. **As `problem` on a failed job** (`GET /jobs/{id}`, `job` events). Work
   that fails after `202 Accepted` (a scan the worker could not start, an
   export the decoder could not read) is reported here. `status` is then the
   status the error would have as a response; the job request itself
   succeeded.
3. **As `problem` on a failed run** (`GET /runs/{id}`, `run` events): the
   error of the step the run failed on.

Routes that do not exist answer `404` and wrong methods `405`, both with an
empty body, not a problem document.

## Codes

| Code | Status | Title |
| --- | --- | --- |
| [`unauthorized`](#unauthorized) | 401 | Missing or unknown bearer token |
| [`forbidden`](#forbidden) | 403 | Your role can't … |
| [`not_found`](#not_found) | 404 | No such … |
| [`invalid_request`](#invalid_request) | 400 | Invalid request |
| [`confirm_mismatch`](#confirm_mismatch) | 412 | Delete not confirmed |
| [`conflict`](#conflict) | 409 | Conflict |
| [`scanner_disconnected`](#scanner_disconnected) | 503 | The scanner is not reachable |
| [`scanner_busy`](#scanner_busy) | 409 | The scanner is busy |
| [`scanner_error`](#scanner_error) | 502 | The scanner refused the request (…) |
| [`cache_error`](#cache_error) | 500 | The bridge could not read or write its files |
| [`battery_low`](#battery_low) | 409 | Battery too low to scan |
| [`storage_low`](#storage_low) | 409 | Not enough space on the scanner |
| [`instrument_moved`](#instrument_moved) | 409 | The scanner moved since its last scan |
| [`decode_failed`](#decode_failed) | 422 | Could not read the reference scan / Could not decode the scan |
| [`check_failed`](#check_failed) | 409 | A recipe check failed |

The spec also lists `needs_approval` and `worker_unavailable`. The bridge
never returns them in this version: a scan that needs approval is accepted as
`awaiting_approval` instead, and the measurement worker does not exist yet.
A run that is cancelled ends with state `cancelled` and no problem.

### `unauthorized`

**401.** The `Authorization` header is missing, does not start with
`Bearer ` (capital B, one space), or holds a token that is not in the token
file. Every route except `GET /health` needs a token. No hint.

### `forbidden`

**403.** The token is valid but its role may not do this. The title names the
action:

| Title | Route | Allowed roles |
| --- | --- | --- |
| Your role can't create projects | `POST /projects` | agent, operator, admin |
| Your role can't start scans | `POST /scans` | agent, operator, admin |
| Your role can't delete scans | `DELETE /scans/{id}` | operator, admin |
| Your role can't cancel jobs | `POST /jobs/{id}/cancel` | agent, operator, admin |
| Your role can't cancel other people's jobs | `POST /jobs/{id}/cancel` by an agent on a job it did not request | operator, admin |
| Your role can't approve jobs | `POST /jobs/{id}/approve` | operator, admin |
| Your role can't create grants | `POST /grants` | operator, admin |
| Your role can't revoke grants | `DELETE /grants/{id}` | operator, admin |
| Your role can't export scans | `POST /scans/{id}/exports` | agent, operator, admin |
| Your role can't write to the log | `POST /log` | agent, operator, admin |
| Your role can't start runs | `POST /runs` | agent, operator, admin |
| Your role can't advance runs | `POST /runs/{id}/advance` | agent, operator, admin |
| Your role can't advance other people's runs | `POST /runs/{id}/advance` by an agent on a run it did not start | operator, admin |
| Your role can't cancel runs | `POST /runs/{id}/cancel` | agent, operator, admin |
| Your role can't cancel other people's runs | `POST /runs/{id}/cancel` by an agent on a run it did not start | operator, admin |

The role (and, for runs, the ownership) is checked before the request body
is read, so a viewer's malformed request gets `forbidden`, not
`invalid_request`. For runs, an unknown run id is `not_found` before the
ownership check. No hint.

### `not_found`

**404.** Title `No such <thing>`:

| Thing | When |
| --- | --- |
| scan | The scan id is not on the scanner, or is not a UUID-like id (hex digits and `-`, at most 64 characters). For previews, files, aim and exports the scanner is asked every time, cached or not; a scan it no longer lists has its cached copies removed. |
| scan file | The scan exists but the scanner lists no file for it (preview, `tzf`, aim, export) |
| project | `POST /scans` or `POST /runs` with a `project_id` the scanner does not have |
| job | Unknown job id (approve, cancel, get) |
| grant | `DELETE /grants/{id}` for an unknown grant |
| export | Unknown export id, one from before a restart, or one made from a scan that has since been deleted through the bridge |
| recipe | Unknown or skipped recipe id |
| run | Unknown run id |
| step | `POST /runs/{id}/advance` with a `step_id` the run's recipe does not have |

No hint.

### `invalid_request`

**400.** The request is malformed or out of range. `detail` says what:

| Where | `detail` (examples) |
| --- | --- |
| Any JSON body | The JSON parser's message: missing field, wrong type, unknown enum value, empty body |
| `POST /projects` | `name must be 1–64 characters with no '/'` |
| `POST /scans` | `mode is required`; `an area scan needs a window`; `a full-dome scan takes no window`; `window out of range: …`; `unknown mode "STD_L9"; valid: STD_L0, …` |
| `POST /grants` | `expires_at must be RFC 3339`; `expires_at is in the past` |
| `GET /log` | `since must be RFC 3339` |
| `POST /log` | `text must be 1–4000 characters` |
| `POST /aim` | `box must lie within the WxH preview with y0 < y1`; `box has zero width` |
| `POST /runs` | `input "purpose" is required`; `input "purpose" must be one of: …` |
| `POST /runs/{id}/advance` | `missing: box` |
| Run `problem` | A step's arguments do not fit the operation (`step "aim": …`), `exportPoints needs scan_id`, an `addLogNote` step's text is missing, blank or too long (`step "note": text must be 1–4000 characters`), or a placeholder had no value (`{{steps.aim.window}} has no value yet`) |

No hint. Fix the request and send it again.

### `confirm_mismatch`

**412.** `DELETE /scans/{id}` without an `X-Confirm` header, or with one that
is not exactly the scan id in the path. Only an operator or admin gets this
far; other roles get `forbidden` first.

Hint: "Repeat the scan id in the X-Confirm header."

### `conflict`

**409.** The request clashes with the current state. Title "Conflict";
`detail` says why.

As a response:

| When | `detail` |
| --- | --- |
| `POST /projects` with a name the scanner already has | `a project named "kitchen" already exists` |
| `POST /scans` when `max_queue_length` scan jobs are already queued or awaiting approval | `the queue is full (20 jobs)` |
| `POST /jobs/{id}/approve` on a job not awaiting approval | `job is queued, not awaiting approval` (the job's `state` as the API spells it) |
| `POST /runs/{id}/advance` on a step that is not waiting for input | `step "box" is not waiting for input` |

As a failed job's `problem`:

| When | `detail` |
| --- | --- |
| The scanner's echo of a scan request differed from what was sent; the bridge cancelled the scanner's task | `the scanner did not accept the request as sent (scanMode: sent "HS_L1", echoed "STD_L0", …); its task was cancelled` |
| A scan ran for more than 45 minutes | `the scan did not finish within 45 minutes` |
| The scanner went idle for 10 polls without filing a scan | `the scanner finished without producing a scan file` |

As a failed run's `problem`:

| When | `detail` |
| --- | --- |
| A step's job failed | `job job_… failed: <the job problem's detail or title>` |
| A step's job was cancelled other than by cancelling the run | `job job_… was cancelled` |

No hint.

### `scanner_disconnected`

**503.** The bridge could not talk to the scanner: no connection, a timeout
(5 s to connect), or a response that is not the expected JSON. (A scanner
that answers with an error status is [`scanner_error`](#scanner_error); a
disk error on the bridge's side is [`cache_error`](#cache_error).)

- At admission: any request that needs the scanner (`/scan-modes`,
  `/projects`, `/scans`, previews, files, aim, exports, `POST /runs`).
  `POST /scans` is refused at once. Cached previews and files are not served
  either: the bridge cannot tell whether the scan still exists.
- In a job: the worker checks the connection again before starting a scan.
  While a scan runs, the job fails after 10 failed polls in a row.

`GET /status` never fails for this reason: it reports
`"scanner": {"connected": false}`.

Hint: "Plug in and power the X9; the bridge brings the USB link up." When the
bridge runs under systemd with the NetworkManager hook, it restarts when a
scanner link comes up.

### `scanner_busy`

**409.** Something other than this bridge's queue is using the scanner.

| When | Where | Title | Hint |
| --- | --- | --- | --- |
| Right before a scan starts, the scanner lists a task the bridge did not start (another client is driving it) | job `problem` | The scanner is busy with another task (`detail`: `N task(s) running`) | Wait for the other task to finish, then queue the scan again. |
| A task the bridge cancelled is still listed after 60 s | job `problem` | The scanner is busy with another task (`detail`: `a task the bridge cancelled is still listed after 60 s`) | Same |
| The scanner itself answered 409 (busy) to a request | response, or job `problem` if it happened when the scan was sent | The scanner is busy (`detail`: the scanner's answer) | none |

A task the bridge cancelled itself does not cause `scanner_busy` for the next
60 s: the next scan waits for it to clear.

### `scanner_error`

**502.** The scanner answered a request with an error status other than 409
(which is [`scanner_busy`](#scanner_busy)), for example 404 for a file or
preview it does not have, or 500. Title `The scanner refused the request
(<status>)`; `detail` holds the scanner's status, path and body.

- As a response: any request the bridge passes on to the scanner (previews,
  files, aim, delete, project creation, …).
- As a job's `problem`: an export whose download the scanner refused, or a
  scan the scanner refused to start.

No hint.

### `cache_error`

**500.** The bridge could not read or write its own files under
`BRIDGE_DATA`: the scan file cache, a download in progress, or an export
file. Typical causes are a full disk or a directory the bridge may not write.

| When | Where | `detail` |
| --- | --- | --- |
| Creating the cache directory, or moving a finished download into place | response, or export job `problem` | `cache directory: …`, `cache: …` |
| Writing a scan file or preview as it downloads | response, or export job `problem` | `writing a download: …` |
| Reading a cached file or an export file | response | `cache: …` |
| Creating the exports directory or writing the export file | export job `problem` | `exports directory: …`, `writing the export: …` |

Hint: "Check that BRIDGE_DATA is writable and its disk has space."

### `battery_low`

**409.** The scanner's battery is below `min_battery_pct_to_scan` (15 %) and
it is not on AC power. Checked when a scan is requested (response) and again
right before it starts (job `problem`). Not checked if the scanner does not
report its battery.

`detail`: `battery 12% is below 15%`. Hint: "Swap the battery or plug the
scanner in."

### `storage_low`

**409.** The scanner's card has less than `min_storage_free_bytes`
(2,000,000,000 bytes) free. Checked like `battery_low`.

`detail`: `<free> bytes free, need 2000000000`. Hint: "Free space on the
scanner's card."

### `instrument_moved`

**409.** `POST /aim` (or an `aimFromPreview` recipe step) while the scanner
reports it has moved since its last scan, so angles from an older preview may
no longer match. Only checked when `refuse_aim_after_move` is on (the default)
and the scanner answers.

Hint: "Take a new overview scan at this station and aim from its preview."

### `decode_failed`

**422.** A scan file or preview could not be read.

| When | Where | Title |
| --- | --- | --- |
| `POST /aim`: the reference scan's file or preview does not decode | response | Could not read the reference scan |
| An export: the scan file or preview does not decode, or `intensity: "preview"` was asked for and there is no preview | export job `problem` | Could not decode the scan |

`detail` holds the decoder's message. No hint. The scan file itself is kept
in the cache; download it with `GET /scans/{id}/tzf` to inspect it.

### `check_failed`

**409.** A recipe's `check` step found a named check that was not ok, or
missing. Appears only as a failed run's `problem`.

`detail` lists each failure: `levelled (tilt correction of 6.2° applied …);
coordinates (missing)`. No hint. See [recipes.md](recipes.md#checks).
