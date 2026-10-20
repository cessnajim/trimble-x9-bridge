# Recipes and runs

A **recipe** describes a scanning job as a list of steps: scans, aiming,
exports, log notes, things a person must do, and checks. A **run** executes a
recipe against one project. The same recipe can be run by an AI agent or by a
person in a web UI; the bridge does the work and enforces the rules either
way.

This page is the reference for recipe format **v0** as implemented in
`crates/x9-bridge/src/recipe.rs` and `runs.rs`. The worked example is
[`crates/x9-bridge/tests/recipes/window-measurement.toml`](../crates/x9-bridge/tests/recipes/window-measurement.toml).
See [guide.md](guide.md) for the API in general.

## Contents

1. [Where recipes live](#where-recipes-live)
2. [File format](#file-format)
3. [Step kinds](#step-kinds)
4. [Actions](#actions)
5. [Placeholders](#placeholders)
6. [Checks](#checks)
7. [Validation](#validation)
8. [Runs](#runs)
9. [Worked example: measure a window](#worked-example-measure-a-window)

## Where recipes live

Recipes are TOML files with the extension `.toml` in `BRIDGE_RECIPES`
(default `$BRIDGE_DATA/recipes`). The bridge reads the directory again on
every recipe request (`GET /recipes`, `GET /recipes/{id}`, `POST /runs`), so a
new or changed file needs no restart. A run keeps the copy of the recipe it
started with.

- Files are read in file-name order. Subdirectories are not read.
- A file that does not parse or does not [validate](#validation) is skipped.
- If two files have the same recipe id, the first in file-name order is used
  and the other is skipped.
- Skipped files are written to the field log (kind `rule`, "Recipe … skipped:
  …") and counted on stderr **at startup only**. Later requests skip them
  silently. Restart the bridge to see why a new file is missing.

```bash
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8795/v1/recipes
# [{"recipe_id":"window-measurement","title":"Measure a window","version":"0.1.0","summary":"…"}]
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8795/v1/recipes/window-measurement
```

`GET /recipes/{id}` returns the whole recipe as JSON. It uses the canonical
field names `recipe_id`, `step_id` and `operation_id`, even when the file used
the short forms `id` and `operation`. Empty lists and unset optional fields
are left out, except `summary` and step `title`, which are always present
(possibly `""`).

## File format

### Top level

| Field | Type | Required | What |
| --- | --- | --- | --- |
| `id` (or `recipe_id`) | string | yes | 1–64 characters: ASCII letters, digits, `-`, `_`. Used in `POST /runs`. |
| `title` | string | yes | Shown to people |
| `version` | string | yes | Free text, for example `"0.1.0"`. Copied to the run as `recipe_version`. |
| `summary` | string | no | One-line description, shown in `GET /recipes` |
| `inputs` | array of tables (`[[inputs]]`) | no | Values the starter must supply |
| `steps` | array of tables (`[[steps]]`) | yes, at least one | Executed in order |

Unknown fields are ignored everywhere.

### Inputs

```toml
[[inputs]]
name = "purpose"
type = "choice"
choices = ["replacement window", "blinds", "trim", "other"]
prompt = "What is the measurement for?"
```

| Field | Type | Required | What |
| --- | --- | --- | --- |
| `name` | string | yes | Referenced as `{{input.<name>}}` |
| `type` | string | yes | A hint for clients (`string`, `number`, `boolean`, `choice`). The bridge does not check it. |
| `choices` | array of strings | no | If set, the value must be a string equal to one of them |
| `prompt` | string | no | The question to ask the starter |

Every declared input is required when a run starts. The bridge checks only
presence and, when `choices` is set, membership. Other values of any JSON type
are accepted as given. Extra inputs that are not declared are accepted but
cannot be referenced (placeholders must name declared inputs).

### Steps

```toml
[[steps]]
id = "overview"
title = "Overview scan from this position"
kind = "action"
operation = "startScan"
arguments = { kind = "full_dome", mode = "STD_L0", color = false, note = "window overview ({{input.purpose}})" }
```

| Field | Type | Used by | What |
| --- | --- | --- | --- |
| `id` (or `step_id`) | string | all | 1–64 characters: ASCII letters, digits, `-`, `_`. Unique in the recipe. |
| `title` | string | all | Optional; shown to people |
| `kind` | string | all | `action`, `person` or `check` |
| `operation` (or `operation_id`) | string | `action` | One of the [actions](#actions) |
| `arguments` | table | `action` | The action's parameters. Values may hold [placeholders](#placeholders). |
| `instructions` | string | `person` | Required for `person` steps: what to tell the person |
| `needs` | array of strings | `person` | Fields `advance` must supply |
| `preview_of` | string | `person` | An earlier step whose scan preview a client should show |
| `of` | string | `check` | The earlier step whose checks are inspected |
| `expect` | array of strings | `check` | Names of checks that must be ok |

Fields that do not apply to a step's kind are accepted and ignored, except
that `preview_of` is validated on every step.

## Step kinds

| Kind | What it does | Its output |
| --- | --- | --- |
| `action` | Calls one bridge operation with `arguments`, under the starter's role. Waits for the job to finish when the operation makes one. | Depends on the action, see below |
| `person` | Waits for someone to call `POST /runs/{id}/advance` with the fields in `needs` | The `input` object the person sent |
| `check` | Requires named checks of an earlier step to be ok; otherwise the run fails | `{"ok": true, "checked": [<names>]}` |

Every step's output is stored in the run context as `steps.<id>`, for
placeholders in later steps, and shown in the run as `steps[i].output`.

`preview_of` is not acted on by the bridge. It tells a client (the web UI, the
MCP server) which earlier step's scan to show while the person works: read the
recipe, take that step's `output.scan_id` from the run, and fetch
`GET /scans/{scan_id}/preview`.

## Actions

Four operations are allowed in `action` steps. Anything else fails
validation. An action runs exactly like the matching API call by the person
who started the run: the same rules, roles, approvals and grants apply.

### `startScan`

Queues a scan and waits until its job ends.

| Argument | What |
| --- | --- |
| `kind` | `full_dome` or `area` (required) |
| `mode` | A scan mode name (required) |
| `color` | `true` or `false` (default `false`) |
| `window` | `{start_ha, delta_ha, start_va, delta_va}`, for `area` only. Usually `"{{steps.<aim>.window}}"`. |
| `note` | Text for the "scan requested" log entry |

- `project_id` is always the run's project. A `project_id` argument is
  replaced.
- The job carries the run's id (`run_id`) and the starter's name
  (`requested_by`).
- Output: `{"job_id": "job_…", "scan_id": "<uuid>"}`.
- While the job is `awaiting_approval`, the run reads
  `state: waiting_for_person`, `waiting_for: approval`.
- If the scan is refused at admission (for example `battery_low`), or the job
  fails, the run fails. If the job is cancelled by anything other than
  cancelling the run, the run fails with "job … was cancelled".

### `aimFromPreview`

Turns a box on a scan's preview into an area-scan window, like `POST /aim`.
Runs at once; no job.

| Argument | What |
| --- | --- |
| `reference_scan_id` | The scan whose preview the box was drawn on (required) |
| `box` | `{x0, y0, x1, y1}` in preview pixels (required). Usually `"{{steps.<person>.box}}"`. |
| `margin_rad` | Padding on every side, radians (default `0.009`) |

Output: `{"window": {…}, "preview_size": [w, h], "cells_per_pixel": [x, y]}`.
See [guide.md, Previews and aiming](guide.md#previews-and-aiming) for the
box rules and the `instrument_moved` refusal.

### `exportPoints`

Starts an export and waits until its job ends.

| Argument | What |
| --- | --- |
| `scan_id` | The scan to export (required) |
| `format` | `ply` or `las` (required) |
| `intensity` | `auto` (default), `scan`, `preview` or `none` |

Output: the whole `Export` object, as from `GET /exports/{id}`:
`export_id`, `scan_id`, `format`, `point_count`, `bytes`, `bounds_m` and
`checks`. A `check` step can then inspect `checks`.

### `addLogNote`

Writes a `note` to the field log. Runs at once.

| Argument | What |
| --- | --- |
| `text` | The note: 1–4000 characters, not only spaces, as for `POST /log`. A missing, empty or too long `text` fails the run with `invalid_request` (`step "<id>": text must be 1–4000 characters`). |

The entry's author is the starter's token name, its project is the run's
project, and `refs` holds the `run_id`. Output: `{"entry_id": "log_…"}`.

## Placeholders

Strings inside `arguments` may contain `{{…}}` placeholders. They are resolved
when the step starts, from the run context:

| Placeholder | Value |
| --- | --- |
| `{{project_id}}` | The run's project id |
| `{{input.<name>}}` | A declared input's value |
| `{{steps.<id>}}` | The whole output of an earlier step |
| `{{steps.<id>.<field>}}` | A field of an earlier step's output. Deeper paths work: `{{steps.aim.window.start_ha}}`. |

Rules:

- **Whole-value placeholders keep their type.** A string that is exactly one
  placeholder (spaces around it allowed) becomes the referenced value itself:
  an object, a number, `null`. `window = "{{steps.aim.window}}"` passes the
  window object.
- **Placeholders inside text are interpolated.** Strings are inserted as
  they are; other values are inserted as JSON. `"for {{input.purpose}} in
  {{project_id}}"` becomes `"for blinds in <uuid>"`.
- Spaces inside the braces are trimmed: `{{ input.purpose }}` works.
- Paths are dot-separated keys. There is no array indexing.
- A path with no value when the step starts fails the run with
  `invalid_request` ("{{…}} has no value yet"). A value that exists but is
  `null` (for example a scan job that returned no `scan_id`) is passed as
  `null`.
- Only `arguments` are resolved. Placeholders in `instructions`, `title` or
  other fields are left as written.
- A `{{` with no closing `}}` is left as text.

Validation checks every placeholder in `arguments` before a recipe is
accepted (see [Validation](#validation)).

## Checks

A `check` step names an earlier step (`of`) and a list of check names
(`expect`):

```toml
[[steps]]
id = "checks"
kind = "check"
of = "export"
expect = ["point_count", "levelled", "coordinates"]
```

The bridge looks for a `checks` array in the `of` step's output. Each name in
`expect` must be there with `"ok": true`. There is no other condition syntax:
no thresholds, comparisons or expressions. Thresholds live in the checks
themselves.

- Only `exportPoints` produces checks today: `point_count`, `levelled`,
  `coordinates` and `intensity` (see
  [guide.md, Export points](guide.md#export-points)). `intensity` is always
  ok.
- A check that is not ok, or missing, fails the run with `409 check_failed`.
  The problem's `detail` lists each failure, for example
  `levelled (tilt correction of 6.2° applied …); flatness (missing)`.
- A `check` step whose `of` step has no `checks` fails on every name.
- Output on success: `{"ok": true, "checked": ["point_count", …]}`.

## Validation

A recipe is accepted only when all of these hold. Otherwise the file is
skipped (see [Where recipes live](#where-recipes-live)).

- It parses as TOML and has `id`, `title`, `version` and `steps`.
- The recipe id and every step id are 1–64 ASCII letters, digits, `-` or
  `_`. Step ids are unique.
- There is at least one step.
- Each step's `kind` is `action`, `person` or `check`.
- `action`: `operation` is one of `startScan`, `aimFromPreview`,
  `exportPoints`, `addLogNote`.
- `person`: `instructions` is present and not blank.
- `check`: `of` names an earlier step (not itself) and `expect` is not empty.
- `preview_of`, when present, names an earlier step.
- Every placeholder in `arguments` is `project_id`, `input.<declared name>`,
  or `steps.<earlier step id>…`. References to the step itself or to later
  steps are refused.

Validation does not check argument values (mode names, formats, required
arguments). Those are checked when the step runs, and a bad value fails the
run with `invalid_request`.

## Runs

### Starting a run

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  -d "{\"recipe_id\":\"window-measurement\",\"project_id\":\"$PROJECT\",\"inputs\":{\"purpose\":\"blinds\"}}" \
  http://127.0.0.1:8795/v1/runs
```

- Any role except `viewer` can start a run.
- `recipe_id` must name a loaded recipe (`404 not_found`), and `project_id` a
  project on the scanner (`404 not_found`; `503 scanner_disconnected` when the
  scanner does not answer).
- Inputs are checked as described in [Inputs](#inputs) (`400 invalid_request`).
- The answer is `201` with the `Run`: `state: running`, every step `pending`.
  The bridge then drives the run in the background.
- The field log gets a `run` entry: `Recipe "<title>" v<version> started`.

### The run object

```json
{
  "run_id": "run_5d6e7f8a9b0c",
  "recipe_id": "window-measurement",
  "recipe_version": "0.1.0",
  "project_id": "<uuid>",
  "state": "waiting_for_person",
  "waiting_for": "input",
  "current_step_id": "box",
  "started_by": "agent-1",
  "created_at": "2026-10-20T12:10:00Z",
  "finished_at": null,
  "steps": [
    {"step_id": "overview", "state": "done", "job_id": "job_…", "output": {"job_id": "job_…", "scan_id": "<uuid>"}},
    {"step_id": "box", "state": "waiting", "job_id": null},
    {"step_id": "aim", "state": "pending", "job_id": null}
  ]
}
```

| Run `state` | Meaning |
| --- | --- |
| `running` | A step is executing |
| `waiting_for_person` | Waiting for someone. `waiting_for` says who: `approval` (an operator must approve the step's scan) or `input` (a `person` step) |
| `succeeded` | Every step is done. `current_step_id` is `null`. |
| `failed` | A step failed. `problem` holds the error; `current_step_id` stays at the failed step. |
| `cancelled` | Cancelled. `current_step_id` stays where it was. |

| Step `state` | Meaning |
| --- | --- |
| `pending` | Not reached yet |
| `running` | Executing |
| `waiting` | A `person` step waiting for `advance` |
| `done` | Finished; `output` is set |
| `failed` | The step the run failed on |
| `skipped` | The step that was in progress when the run was cancelled |

Steps after a failed or cancelled step stay `pending`. `job_id` is set on
`startScan` and `exportPoints` steps.

`GET /runs` lists every run since the bridge started, newest first.
`GET /runs/{id}` returns one. Each change is also sent as a `run` event on
`GET /events`. Runs are kept in memory only.

### Whose run it is

A run executes **under the role and name of whoever started it**:

- Its scans are requested as the starter: an `agent`'s scans wait for approval
  unless a grant covers them; an `operator`'s are queued at once.
- Jobs carry the starter's name as `requested_by` and the run's id as
  `run_id`. An agent can cancel those jobs directly.
- `addLogNote` entries are written in the starter's name.
- A grant with `run_id` set covers only scans from that run. A grant without
  `run_id` covers matching scans from any run and from direct requests.

Only the starter (the same token name), an `operator` or an `admin` may
advance or cancel a run. Another `agent` gets `403 forbidden` ("Your role
can't advance other people's runs" / "… cancel other people's runs"), and a
`viewer` cannot do either. The ownership check comes before the request body
is read, so another agent's malformed `advance` is also `forbidden`.
Advancing or cancelling a run does not change whose run it is: an operator
who hands in a person step's input leaves the run, and its later scans, under
the starter's name and role.

### Waiting for approval

When a `startScan` step's job is `awaiting_approval`, the run reads
`waiting_for_person` / `approval`. An operator approves the job as usual:

```bash
JOB=$(curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8795/v1/runs/$RUN | jq -r '.steps[] | select(.state=="running") | .job_id')
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8795/v1/jobs/$JOB/approve
```

The run goes back to `running` once the job is queued.

### Advancing a person step

When a `person` step is reached, the step reads `waiting`, the run reads
`waiting_for_person` / `input`, and the field log gets "Waiting for a person:
<instructions>" with the `run_id` and `step_id`.

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  -d '{"step_id":"box","input":{"box":{"x0":523,"y0":418,"x1":674,"y1":564}}}' \
  http://127.0.0.1:8795/v1/runs/$RUN/advance
```

- Only the starter, an operator or an admin may advance (see
  [Whose run it is](#whose-run-it-is)).
- `step_id` must be the step that is waiting. An unknown step is
  `404 not_found` ("No such step"); a step that is not waiting for input is
  `409 conflict`.
- `input` must be an object with every field in `needs`
  (`400 invalid_request`, "missing: box"). Extra fields are kept. A field set
  to `null` counts as present.
- The answer is the run as it was just before the step completes. The step
  becomes `done` a moment later, with the `input` object as its output, and
  the next step starts.

### Proposals: the agent answers, a person confirms

AI-first, the agent does a person step's work (finds the window on the
preview, measures the opening) and a person only validates it. While a
`person` step waits, anyone who may advance the run (its starter, an operator
or an admin) can **propose** its input:

```bash
curl -s -H "Authorization: Bearer $TOKEN" \
  -d '{"step_id":"box","input":{"box":{"x0":1666,"y0":250,"x1":40,"y1":687}},
       "note":"Window on the right wall, framed with wall on every side"}' \
  http://127.0.0.1:8795/v1/runs/$RUN/propose
```

- `input` must hold every field in `needs`, as for `advance`
  (`400 invalid_request`); `note` is optional, at most 2000 characters.
- The step keeps waiting. It gains `proposal`: `{input, by, at, note}`. A
  newer proposal replaces an older one. The field log gets "<by> proposed an
  answer for step …".
- A person then advances the step as usual, usually with the proposal as is
  (the web UI shows it with **Looks right**). The step's output then has
  `proposal_accepted: true`, or `false` when the person changed any field in
  `needs`, and the field log says which.
- Another agent gets `403 forbidden`; a step that isn't waiting is
  `409 conflict`.

### Cancelling a run

```bash
curl -s -X POST -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8795/v1/runs/$RUN/cancel
```

- Only the starter, an operator or an admin may cancel (see
  [Whose run it is](#whose-run-it-is)).
- If the current step has a job: a `queued` or `awaiting_approval` job is
  cancelled at once (a granted scan gets its grant use back); a running scan
  is stopped exactly as with `POST /jobs/{id}/cancel` (the scanner is told to
  stop, and a partial scan may be kept as `partial_scan_id`). A running
  export stops at its next check, as with `POST /jobs/{id}/cancel`, leaving
  no export file; the run is cancelled as soon as it has.
- A waiting `person` step is released at once.
- An `aimFromPreview` or `addLogNote` step finishes first; the run is
  cancelled before the next step.
- The answer is the run as it is now; it settles to `cancelled` shortly
  (follow `GET /runs/{id}` or `run` events). The step in progress becomes
  `skipped`, and the field log gets "Run … cancelled".
- Cancelling a finished run changes nothing and returns it with `200`.

### When a run fails

A run fails on the first step that returns an error: a refused scan, a failed
job, a placeholder with no value, a bad argument, a failed check. The run's
`problem` is the error as a problem document (see [errors.md](errors.md)), and
the field log gets an `error` entry, "Run … failed: <detail>". There is no
retry; start a new run.

## Worked example: measure a window

The test recipe
[`window-measurement.toml`](../crates/x9-bridge/tests/recipes/window-measurement.toml):

```toml
id = "window-measurement"
title = "Measure a window"
version = "0.1.0"
summary = "Overview scan, an aimed close-up of the window, a checked point export, then measuring."

[[inputs]]
name = "purpose"
type = "choice"
choices = ["replacement window", "blinds", "trim", "other"]
prompt = "What is the measurement for?"

[[steps]]
id = "overview"
title = "Overview scan from this position"
kind = "action"
operation = "startScan"
arguments = { kind = "full_dome", mode = "STD_L0", color = false, note = "window overview ({{input.purpose}})" }

[[steps]]
id = "box"
title = "Mark the window"
kind = "person"
instructions = "Draw a box around the window on the overview preview: ..."
needs = ["box"]
preview_of = "overview"

[[steps]]
id = "aim"
title = "Turn the box into a scan window"
kind = "action"
operation = "aimFromPreview"
arguments = { reference_scan_id = "{{steps.overview.scan_id}}", box = "{{steps.box.box}}", margin_rad = 0.017 }

[[steps]]
id = "closeup"
title = "Close-up scan of the window"
kind = "action"
operation = "startScan"
arguments = { kind = "area", mode = "STD_L2", color = false, window = "{{steps.aim.window}}", note = "window close-up" }

[[steps]]
id = "export"
title = "Decode the close-up to points"
kind = "action"
operation = "exportPoints"
arguments = { scan_id = "{{steps.closeup.scan_id}}", format = "ply", intensity = "auto" }

[[steps]]
id = "checks"
title = "Check the export"
kind = "check"
of = "export"
expect = ["point_count", "levelled", "coordinates"]

[[steps]]
id = "measure"
title = "Measure and report"
kind = "person"
instructions = "Measure the opening from the export and attach the check image ..."
needs = ["notes"]
```

What each step does and what it leaves in the context:

| Step | Kind | Does | Output used later |
| --- | --- | --- | --- |
| `overview` | `startScan` | Full dome, STD_L0, no color; the note includes the purpose | `steps.overview.scan_id` |
| `box` | `person` | Waits for a box drawn on the overview's preview (`preview_of`) | `steps.box.box` |
| `aim` | `aimFromPreview` | Box → window, with 0.017 rad (about 1°) of margin | `steps.aim.window` |
| `closeup` | `startScan` | Area scan of that window, STD_L2 | `steps.closeup.scan_id` |
| `export` | `exportPoints` | PLY of the close-up, preview intensity | `steps.export.checks` |
| `checks` | `check` | Point count, levelled and coordinates must be ok | |
| `measure` | `person` | A person measures and hands in `notes` (the measurement worker is not built yet) | |

Running it as an agent, with an operator at hand:

```bash
B=http://127.0.0.1:8795/v1
A="Authorization: Bearer $AGENT"; O="Authorization: Bearer $TOKEN"

# 1. The agent starts the run.
RUN=$(curl -s -H "$A" -d "{\"recipe_id\":\"window-measurement\",\"project_id\":\"$PROJECT\",\"inputs\":{\"purpose\":\"blinds\"}}" $B/runs | jq -r .run_id)

# 2. The overview scan waits for approval; the operator approves it,
#    and grants the run its close-up scan.
JOB=$(curl -s -H "$O" $B/runs/$RUN | jq -r '.steps[0].job_id')
curl -s -X POST -H "$O" $B/jobs/$JOB/approve
curl -s -H "$O" -d "{\"project_id\":\"$PROJECT\",\"expires_at\":\"2026-10-20T16:00:00Z\",\"max_scans\":1,\"run_id\":\"$RUN\"}" $B/grants

# 3. When the run waits for input on "box", look at the overview's preview.
SCAN=$(curl -s -H "$A" $B/runs/$RUN | jq -r '.steps[0].output.scan_id')
curl -s -H "$A" -o overview.jpg $B/scans/$SCAN/preview

# 4. Hand in the box. The run aims, scans the close-up under the grant,
#    exports it and checks the export, then waits on "measure".
curl -s -H "$A" -d '{"step_id":"box","input":{"box":{"x0":523,"y0":418,"x1":674,"y1":564}}}' $B/runs/$RUN/advance

# 5. Read the export, measure, and finish the run.
curl -s -H "$A" $B/runs/$RUN | jq '.steps[] | select(.step_id=="export") | .output'
curl -s -H "$A" -d '{"step_id":"measure","input":{"notes":"opening 1196 x 1168 mm"}}' $B/runs/$RUN/advance
```

The run then reads `succeeded`, and the field log holds the whole session:
the run start, each scan request and its progress, the approval and grant,
the waits for a person, and the finish.
