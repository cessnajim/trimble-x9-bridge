# Provenance

This project talks to a Trimble X9 laser scanner over the scanner's own local
HTTP API, for interoperability: running scans and getting data out on
platforms and in workflows the vendor does not support, on a scanner its author
owns. This file records how the knowledge of that API was obtained, so anyone
can check it.

## What this repository does not contain

- No vendor source code, binaries, SDKs, firmware or documentation.
- No names of vendor-internal programs, libraries, classes or functions.
- No captured data from the author's projects; tests use a simulator and the
  author's own scans of their home, which are not committed.

## How the API knowledge was obtained

**Every API fact the code relies on was confirmed against our own scanner**,
by sending requests and reading its responses, between 2026-10-19 and
2026-10-20. Facts that could not be confirmed are not used.

**How it was first discovered.** The port, base path and route names were
first identified by reading text strings in the vendor's Windows field
software, installed on our own tablet (nothing was executed, decompiled or
copied). One field value, `AreaScan`, was first seen the same way. These were
treated as hypotheses: each was kept only after the scanner confirmed it, and
routes seen only in that software (for example a leveling route) are not used.
This is interoperability reverse engineering of our own equipment, not a
two-team clean-room process.

## Evidence for each fact

| Fact | How our scanner confirmed it |
| --- | --- |
| Reachable over USB as a network device (Linux binds it as `cdc_eem`, DHCP on 192.168.42.0/24) and over its own Wi-Fi access point (192.168.43.1); HTTP on port 34567, base path `/api/v1` | Requests answered on both links |
| `GET /status`: battery, AC power, storage, leveling state and compensator readings, "moved since last scan", current task id | Responses read and mapped; values changed as expected (battery falling, leveling `none` → `ok`) |
| `GET /projects`, `POST /projects` with `projectName` + `projectUUID` | Projects created and listed |
| `GET /scans`: numeric ids are list positions, `scanUUID` is stable; `DELETE /scans/{id}` | Ids renumbered after a delete |
| `GET /scans/modes`: modes with range, spacing, point count, a fixed time and an acquisition time | Values matched measured scan durations |
| `POST /scans` returns 202 and a task whose `scanRequest` echoes the request, filled with the scanner's defaults (`scanType` `FullDome`, `colorMode`, `autoLeveling`, `startHA`/`deltaHA`/`startVA`/`deltaVA`) | Echo read on every scan; both targeting fields are needed to choose a project |
| `scanType` `AreaScan` with a window in radians, vertical angle from the zenith | Accepted, echoed, and the scans covered exactly the requested windows (a window and two speakers framed on the first try) |
| A request the scanner can't parse still gets 202, then a default full-dome scan | Observed; hence the bridge's echo check, which cancels on any difference |
| `GET /tasks`: `initializing` → `running`, `percentage` 0 → 1 during the sweep, then steps of 1/30 during color images | Polled at 1–2 Hz through full and area scans |
| Scan files `project/N.tzf`; area scans filed as `N_k.tzf` under station N | Read from `/scans` after each scan |
| `GET /tzfs/{id}/file` and `/tzfs/{id}/preview`; the preview is grid-aligned | Downloaded many times; previews matched the grid (template match 0.974; area previews equal the grid size) |
| `DELETE /tasks/{id}` stops a running task | **Not yet confirmed on our scanner** in these tests (exercised against the simulator only) |

The `.tzf` file format is handled by the separate
[trimble-tzf](https://github.com/cessnajim/trimble-tzf) project, which has its
own provenance record.

This describes the method; it is not legal advice.
