# via-stitcher

Places a field of stitching vias tying a copper plane (typically GND) together
on a KiCad 10 board, and removes them again on demand.

## Requirements

KiCad 10 must be **running with the target board open**, and the IPC API
enabled under *Preferences → Plugins → Enable IPC API* (then restart KiCad).
The API is a channel into the editor's in-memory board, not a file library —
KiCad 10 has no headless mode, so there is no way to run this against a
`.kicad_pcb` on disk.

## Build

```sh
cargo build --release          # ./target/release/via-stitcher
```

The debug build works but is several times slower; on a 6-layer board with
23k-point zone fills a run takes ~10s debug. Use release for real work.

## Quick start

```sh
# 1. Check the connection and see what the board looks like. Changes nothing.
via-stitcher probe

# 2. See what would be placed. Changes nothing.
via-stitcher stitch --config stitch.toml --dry-run

# 3. Place them.
via-stitcher stitch --config stitch.toml

# 4. Look at the board. If you don't like it:
via-stitcher clear --config stitch.toml
```

A real run makes one undoable commit for the vias and a second for the group,
refills zones so the planes re-flood around the new vias, and leaves the board
**unsaved** so you can inspect it and undo. Pass `--save` to write the file.

### Flags

| Flag | Effect |
|---|---|
| `--dry-run` | Report counts and skips; touch nothing. |
| `--replace` | Delete the previous run's vias first, instead of filling gaps around them. |
| `--refill-first` | Refill zones before reading, instead of refusing on stale fill. |
| `--save` | Save the board afterwards. |

Without `--replace`, re-running adds vias in the gaps and keeps the existing
ones, so runs accumulate rather than starting over.

## Running more than one pattern on a board

`group_name` is what separates one field of vias from another. Give each config
its own name and the runs coexist — separate groups, separate manifests,
cleared independently:

```sh
via-stitcher stitch --config stitch-edge.toml   # group via-stitch:GND-edge
via-stitcher stitch --config stitch-grid.toml   # group via-stitch:GND-grid
via-stitcher clear  --config stitch-edge.toml   # removes only the edge ring
```

## How placement works

A through via touches every copper layer but only needs to *connect* on some of
them, so layers have two roles:

- **Connect layers** — where the via must land inside the target net's zone
  fill. `connect.layers = "auto"` discovers these from where the net's zones
  actually are, which on a typical board is a subset of the copper stack.
- **Traversed layers** — everything else the via passes through, where it must
  *clear* foreign copper instead. Foreign tracks and pads always block
  placement. Foreign *zone fill* only blocks it on layers set to `avoid`; on
  `allow` layers the via goes through and KiCad's refill voids around it.

The admissible region is the intersection of the plane fill across the connect
layers, shrunk by the via's radius plus clearance, minus foreign copper and any
via keepout, grown by the same. Candidates come from the configured X/Y grid and
from a perimeter ring. A candidate that doesn't fit is searched outward in
concentric rings up to `search.radius_mm`; if nothing fits it is reported and
skipped.

### Two edges, and they are very different

`edge.reference` decides what the perimeter ring follows:

- `"board"` — the **board outline** from Edge.Cuts. This is a via fence round
  the physical perimeter. `edge.distance_mm` is measured in from the board edge,
  so it interacts with your fab's copper-to-edge rule.
- `"plane"` — the **zone boundary**. This follows every plane cutout and island
  through the board interior, so it produces far more vias and they are not
  confined to the perimeter.

On a test board the same settings gave 107 vias with `"board"` and 373 with
`"plane"`. Pick deliberately.

## Finding its own vias again

Vias carry no user-property field in KiCad's IPC schema, so placed vias are
tagged two ways: a named PCB group, and a
`<board>.<group>.stitch-manifest.toml` sidecar listing their KIIDs. `clear`
deletes the union of both — a via has to be missing from both to survive.

The group is created in a **second commit**, after the vias are committed:
KiCad resolves a group's member KIIDs against the committed board, and a group
created alongside its members comes out empty and is rejected with
"no overlapping layers with the board".

Keep the manifest alongside the board. If it is lost the group still works, but
the two together are what make `clear` reliable.

## Configuration

See `stitch.toml` for a documented example. Every dimensional field carries its
unit; distances are millimetres and are converted to KiCad's nanometres at the
IPC boundary.

Fields worth understanding:

| Field | Notes |
|---|---|
| `connect.layers` | `"auto"`, or an explicit list of at least two layers. |
| `via.unconnected_layer_removal` | `remove_except_ends` strips annular rings from inner planes the via only passes through, minimising the void punched in them. |
| `traversal.layers` | Per-layer `allow`/`avoid`. Setting a full-board power plane to `avoid` can reject nearly every candidate — check with `--dry-run`. |
| `search.radius_mm` | How far a blocked via may move. Much smaller than the grid pitch means blocked points are skipped rather than relocated. |
| `search.min_spacing_mm` | Minimum centre-to-centre between final vias. |

## Troubleshooting

**"KiCad is busy"** — KiCad refuses document commands while a modal dialog is
open in the PCB editor. Close it and re-run. It also happens mid-drag and during
a zone refill, which clear on their own.

**"some zones are not filled"** — placement is computed from KiCad's own zone
fill, so a stale or empty fill would place vias against geometry that is not
there. Fill zones in KiCad, or pass `--refill-first`.

**"no net named ... on this board"** — the net name is checked against the board
up front, because creating an item on an unknown net name silently registers
that net rather than failing.

**"connect.layers = auto found N layer(s)"** — the net has zones on fewer than
two copper layers, so there is nothing to stitch between.

**A warning about untracked vias** — the board gained more vias than KiCad
reported ids for. The extra ones are not in the group or manifest and `clear`
will not remove them; revert the board (it is unsaved unless you passed
`--save`) and re-run.
