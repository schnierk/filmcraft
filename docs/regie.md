# KI-Regie (AI Director)

Say what you want and FilmCraft builds the film from your own media as a normal, fully
editable sequence. Nothing leaves the machine.

> "Erstelle ein neues Projekt in 16:9 und 4K und mach aus meinen Urlaubsvideos von diesem Jahr
> einen Rückblick, fröhlich, etwa drei Minuten."

## Steps and commands

Each step is a command of its own (`crates/engine/src/regie/`). `regie.run` chains them; the
panel (Window ▸ AI Director), the CLI, the control channel and MCP agents can also call them one
by one. The data types are in `crates/engine/src/regie/types.rs` (camelCase JSON, every field
optional).

| Command | Params | Result |
|---|---|---|
| `regie.plan` | `{"request": str, "language"?: "de"\|"en"}` | `Plan`: kind, aspect, size, rate, length, date range, places, mood, title, music, folders, what was understood, open questions |
| `media.captureInfo` | `{"path": str}` or `{"item": id}` | `{"created", "location", "width", "height", "seconds"}` from MP4/MOV metadata and JPEG EXIF |
| `regie.findMedia` | `{"folders": [str]?, "from"?, "to"?, "places"?: [str], "kinds"?: ["video","photo","audio"], "limit"?: n}` | `{"candidates": [Candidate], "scanned": n, "skipped": n}` |
| `media.highlights` | `{"item": id, "segmentSeconds"?: 3, "maxSegments"?: 4, "step"?: 0.5}` | `Highlights`: best segments with scores and reasons, score curve |
| `audio.beats` | `{"item": id}` | `Beats`: bpm, beats, bars, energy per beat, calm/lively sections |
| `regie.assemble` | `{"plan": Plan, "media": [Candidate + "highlights"?], "music"?: {"item": id, "beats"?: Beats}}` | `{"sequence": id, "clips": n, "seconds", "chapters": [...], "summary": [str]}` |
| `regie.run` | `{"request"?: str, "plan"?: Plan, "folders"?: [str], "wait"?: bool}` | starts a job (see `jobs.list`); `regie.status` reports progress and, when done, the `regie.assemble` result |
| `regie.status` | `{}` | `{"running", "job", "progress", "status", "result"}` |

## How the film is built (recipe "recap")

- Chapters: media grouped by capture date (a gap of more than two days starts a new chapter),
  named after the place or folder and the month ("Mykonos · September").
- Order: chapters by date, photos and clips by capture time inside a chapter.
- Cuts land on the music's beats: a bar per photo or shot in calm sections, half a bar in lively
  ones. Each video uses its best segment (`media.highlights`).
- Portrait media in a landscape film (and the other way round) sit on a blurred, darkened copy of
  themselves; photos move slowly (Ken Burns).
- Titles: the film title at the start, a chapter title on each chapter's first shot, a closing
  title; the music fades out at the end.
- The whole build is one undo step.

## The panel (Window ▸ AI Director; German "KI-Regie")

`crates/ui-egui/src/panels/regie.rs`. Top to bottom: a short intro, the wish (a text box and four
example chips that fill it), the folders to search (empty: the default Pictures, Videos and Music
folders; Add folder… uses the native folder dialog), Understand and Make film, then while a film
is being made its progress, status and Cancel, and when it is done the summary, its length and
Play, Undo and Explain how, and last "What I understood": the plan's `understood` lines, its key
fields (aspect, HD or 4K size for that aspect, frame rate, length in seconds with 0 = automatic,
dates `YYYY-MM-DD`, mood, title) and its `questions`.

- **Understand** runs `regie.plan {"request", "language"}` (`"de"` for a German interface, `"en"`
  for English, left out otherwise). Folders the plan names join the folder list.
- **Make film** runs `regie.run` with `"wait": false`: `{"plan", "folders"?}` when the plan was
  made from the wish as it reads now (with the changes made in the panel; `plan.folders` is the
  panel's list), otherwise `{"request", "folders"?}`. It expects `{"job": id}` (a result with
  `"sequence"` is taken as a film made at once).
- While the job runs the panel asks `regie.status` at most every 250 ms, also while it is hidden,
  and never waits on the UI thread. It reads `running`, `job`, `progress` (0..1) and `status`
  (text); once `running` is false, `result` (`{"sequence", "clips", "seconds", "chapters",
  "summary", "explain"}`, plus `"folders"` searched when it names them; read leniently) is the
  film when `job` is this job or null. Without a result it reports the job's error from the job
  list, or, two seconds after the job ended, that no film was made. **Cancel** is
  `jobs.cancel {"job": id}`.
- **Play** is `sequence.open {"item": sequence}`, `playhead.start` and `playback.toggle`.
  **Undo** is `edit.undo`, offered only while the film's build is still the last undo step (the
  history's depth and top step are remembered when the film arrives). **Explain how** shows the
  result's `explain` lines.
- Every command error becomes one friendly line in the panel (`ui.regie.error`), the status bar and
  the log; a missing command (an engine without that step) reads "The AI Director can't do this
  yet in this version of FilmCraft."
- State: `ui.inspect` → `ui.regie`; `ui.set {"regie": {…}}` deep-merges `request`, `folders`,
  `plan`, `job`, `progress`, `status`, `result`, `error`, `openSections` (`folders`, `plan`,
  `explain`).
- Automation ids: `regie.request`, `regie.example.<0-3>`, `regie.section.folders`,
  `regie.folders.default`, `regie.folder.<i>`, `regie.folder.remove.<i>`, `regie.folder.add`,
  `regie.understand`, `regie.make`, `regie.error`, `regie.progress`, `regie.cancel`,
  `regie.result`, `regie.play`, `regie.undo`, `regie.explain`, `regie.section.plan`,
  `regie.aspect` (`regie.aspect.<16x9|9x16|1x1|4x5>`), `regie.resolution`
  (`regie.resolution.<hd|uhd>`), `regie.fps` (`regie.fps.<24|25|30|50|60>`), `regie.seconds`,
  `regie.from`, `regie.to`, `regie.mood` (`regie.mood.<upbeat|calm|epic>`), `regie.title`.
