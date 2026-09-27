# Engine LOD architecture

Native LOD for the Bevy runtime. LOD geometry joins `WORLD_LAYER`, so it
appears in the water reflection pass by design (`render.rs`,
`REFLECTION_VIEW_LAYERS`); reflection cost is budgeted, not ignored.

## Tier model

Fixed spatial tiers past the full-detail grid: 4-, 8-, and 16-cell blocks
(`side_cells` 4/8/16, DynDOLOD GEOM-01). Block size, mesh source, distance
policy, and variant stay separate dimensions (ARCH-03). Per-worldspace LOD
origins anchor chunks; floor division rounds toward negative infinity so a
position just west of zero lands in cell -1 (GEOM-02). `worldspaces` needs
new origin columns; it holds only id/editor/parent/flags today.

Selection is tier-based first. Projected-size selection is a later option,
not the initial contract.

## Streaming integration

`plan_cells`/`collect_cells` key on single cells with generation-guarded
stale discard and a default of one commit per frame
(`max_cell_commits_per_frame`, tunable in `EngineConfig`). Chunks get a parallel key space
`(world, tier, anchor)` sharing the same commit budget and epoch handling,
so fast travel cannot strand half-loaded chunks. Unload uses hysteresis:
drop a tier only when the camera leaves tier range plus a margin ring, so
boundary oscillation does not thrash loads.

Handoff: a chunk tier hides exactly where full cells (or a nearer tier)
attach. No indivisible chunk may overlap live full geometry (GEOM-05), and
no lasting duplicate or hole is allowed during handoff (RUN-07). Bounds
are conservative and include every emitted component, tested across chunk
and world boundaries (GEOM-06).

## Camera, shadows, fog

Camera far is currently `CELL_SIZE * (stream_radius + 2) * 2` (`app.rs`),
32,768 units at the default radius 2. Sun shadow cascades derive from the
same radius. LOD range must be fitted with far plane, cascades, and fog
together: `SkyrimClear` fog reaches full strength at 53,289 units
(`sky.rs`), so far geometry past the fog wall is wasted work.

## Render path

- LOD meshes render with the same GPU preprocessing, indirect draw, and
  HZB path as full geometry; the existing proof bridge (`render.rs`)
  covers them without special cases.
- Billboards get a dedicated material (see `billboard-generation.md`).
- Dynamic proxies are ordinary entities with distance/state systems (see
  `lod-runtime-proxies.md`), spatially bucketed, never full-scene scanned
  per frame (RUN-05).

## Phases and gates

- Phase 0: payload-format ADR, far/cascade/fog fit, enable-state
  extraction design, LOD-origin columns. Gate: decisions recorded, no
  behavior change.
- Phase 1: coarse terrain ring from the cell cache past the unload ring,
  seam/skirt policy, clean attach handoff. Gate: tier captures, no seam
  or overlap failures.
- Phase 2: static object chunks, provenance, missing-reference inspector.
  Gate: per-tier budgets met, rebuild equivalence.
- Phase 3: object-route tree billboards, atlas budgets, mip coverage.
  Gate: foliage at distance matches near-field palette (see ADR-0009).
- Phase 4: dynamic proxies, glow, approved animation. Gate: state tests,
  travel/load epoch tests.
- Phase 5: proof: tier captures, handoff sequences, budgets, signed
  visual review, fresh vista capture before any readiness claim.
