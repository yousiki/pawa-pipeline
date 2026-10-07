# viewer: dataset viewer

A zero-dependency, zero-build static page (`index.html`) that browses a PAWA dataset directly
from a Hugging Face dataset repo in the browser. Nothing is downloaded to disk: videos are
streamed and `frames.jsonl` is parsed on the fly.

## Serving

Any static file server works:

```sh
cd viewer
python3 -m http.server 8000
# open http://localhost:8000/
```

It can equally be hosted on GitHub Pages or any other static host; it is a single file.

## Usage

On the home page, enter the HF dataset repo (`owner/name`) and, for private repos, an HF token.
The token is kept only in the tab's sessionStorage (it is gone when the tab closes) and is only
ever sent to `huggingface.co`. Pick a branch (each branch/revision is one dataset version), then an
episode.

You can also link straight to a repo and branch:

```
http://localhost:8000/?repo=YOUSIKI/pawa-dataset&revision=main
```

- **Episode index**: filter by scenario, player count N, QC verdict and your own marks; sort by
  time, scenario or N. QC badges load lazily as rows scroll into view.
- **Synchronized playback** of every POV: play/pause/replay, playback rate, global seek and
  single-frame stepping. If any stream is buffering, all of them pause together, so you never
  see misaligned frames.
- **Live action HUD** (a canvas port of `crates/hud/src/overlay.rs`): WASD / SPACE / SPRINT /
  SNEAK, ATTACK / USE / MINE / PLACE, selected hotbar slot, held item, a camera dial (full scale
  = the POV's largest measured per-frame turn) and a frame / world tick / position status line.
  Data is streamed from `frames.jsonl` (line i = video frame i; frame 0 is the same world tick in
  every POV).
- One column per POV: the default view (with HUD) on top and the equirectangular panorama below,
  all in sync. Prev/next episode follows the current index filter and sort order.
- **POV paging** to bound bandwidth and decoding: 1/2/4/8/all POVs per page (remembered,
  default 4). Paging keeps the playback position; off-page videos are fully unloaded, HUD data
  is cached.
- keep/drop/seen marks are stored in localStorage; "Export marks" on the index page copies them
  as JSON to the clipboard.

### Keyboard shortcuts (episode page)

| Key | Action |
| --- | --- |
| `Space` | play / pause all |
| `←` / `→` | seek -5 s / +5 s |
| `,` / `.` | step one frame back / forward |
| `p` / `n` | previous / next episode |
| `[` / `]` | previous / next POV page |
| `k` / `d` | toggle keep / drop mark |

## Implementation notes

- `<video>` cannot send an Authorization header, so the page issues one authenticated `HEAD` to
  the `resolve/` URL and uses the signed CDN URL behind the 302 redirect (no auth needed,
  supports Range requests) as the video `src`.
- The episode index comes from the HF tree API (paginated via the Link header); episode metadata
  in the index is parsed from the directory name `<ts>-<scenario>-<N>p-<hash8>`, so no
  per-episode `episode.json` fetch is needed until an episode is opened.
- Episodes with `qc.verdict=fail` are shown too: the verdict is a label for filtering, not a
  shipping gate.
- Known quirk: video lags the recorded actions by 0-4 frames (render phase residual; see
  `docs/dataset-spec.md`).
