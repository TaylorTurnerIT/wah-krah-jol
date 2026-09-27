# LOD chunk compiler

Offline compiler turning `references` + `statics` + cell cache into
spatial LOD chunks. Follows converter norms: canonical paths, staged
outputs, atomic publish, content-hash invalidation, schema-versioned
manifests, fail-closed validation with file/block/shape in every error.

## Inputs

- `references` (placement, rotation, scale, `radius_override`), `statics`
  (model path, bounds), exterior R-tree, cell cache terrain.
- Per-worldspace LOD origins (new `worldspaces` columns; required by
  GEOM-02, missing today).
- Enable-state columns (new; `XESP` parent + inversion flags), extracted
  at convert time so the runtime never parses blobs per query.
- Rule set: explicit record rule, then component/type default, then
  omit-with-reason. Every decision records winner, rule, model, and
  fallback reason (ARCH-02, RULE-03).

## Selection

Per-tier source choice: a landmark may reuse its full GLB at tier 4 while
clutter drops out before tier 16 (GEOM-03). Authored `_lod` NIF variants
match by convention where present; `BSLODTriShape`/`BSSubIndexTriShape`
blocks parse today but flatten on export, so per-level preservation must
be verified in the vendor crate before they can feed tiers. Missing source
means omit-with-reason, never silent near-match substitution (RULE-05).

## Chunk build

- Transform in double precision, then emit chunk-local coordinates;
  transform normals and winding with the shape transform (GEOM-04).
- Partition batches by rendering compatibility (material family,
  alpha mode, overlay identity), not by texture filename (MAT-02).
- UVs outside 0-1 fall back to direct textures; never clamp repeating
  UVs without a preserving conversion (MAT-04).
- Strip collision and scene-graph extras from static output; animation
  and effect assets take the proxy path instead (GEOM-07).
- Destructive optimizations (dedup, hidden-face removal) are individually
  disablable; hidden-face removal uses a tolerance and conservative test
  (GEOM-08/09). Terrain may hide triangles only with the same
  conservatism; bridges and caves are never deleted on terrain say-so.
- Chunks whose references all sit in unreachable space may be pruned only
  against a reachable-viewpoint set, with a no-pruning reference mode
  for comparison (GEOM-10).

## Outputs

Chunk payload format is decided by ADR (GLB files vs `lod`-table blobs;
the `lod` table exists but has no writers or readers). Either way, each
chunk ships conservative bounds, a material batch list, subcell visibility
data for handoff (GEOM-05), and a manifest entry: content hashes of
inputs, rule set, settings, and compiler version (BUILD-01/02).

## Incremental builds

Reference moves propagate to every affected tier, world copy, and variant
(BUILD-03). Atlas pixel changes avoid remeshing only when UV layout and
material contracts are unchanged (BUILD-02). Partial builds must equal
clean builds semantically or be marked unsafe (BUILD-04); a dry-run impact
report precedes expensive regeneration (BUILD-05).
