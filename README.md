# rustjay-clipper

Load a long video instantly, mark multiple in/out regions, export each as its
own clip. Sibling app to rustjay-mosh.

- **Instant load** — the file is probed and seeked, never transcoded or read
  into RAM.
- **Scrub & mark** — drag the timeline (keyframe-fast while dragging, exact on
  release), `I` sets an In point, `O` commits the region. Space plays, ←/→
  step one frame.
- **Export** — H.264 sources export with `-c copy` (no re-encode; the cut
  snaps back to the previous keyframe so nothing is lost). Other codecs — or
  the "Force re-encode" checkbox — re-encode to H.264 veryfast CRF 18.
  Clips land next to the source as `<name>_c01.mp4`, `_c02.mp4`, … unless an
  output folder is chosen.

## Build

```bash
cargo run --release
```

Needs Rust, FFmpeg 8.x libraries (for probe/preview) and the `ffmpeg` CLI on
PATH (for export).

Skipped: audio preview, waveform display, draggable region edges — add when
the I/O-key workflow isn't enough.
