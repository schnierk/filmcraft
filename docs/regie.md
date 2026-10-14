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
