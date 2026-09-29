# LOD chunk compiler

Offline compiler turning `references` + `statics` + cell cache into
spatial LOD chunks. Uses canonical paths, an immutable input boundary,
staged outputs, a validated build identity, content-hash invalidation, and
schema-versioned manifests. The runtime holds a shared sibling lock while it
reads an asset directory; publication takes a nonblocking exclusive lock for
the final swap and refuses to proceed while a reader is active. An interrupted
swap restores the last-good backup before the next build. Validation errors
identify file, block, and shape.

## Inputs

- `references` (placement, rotation, scale, `radius_override`), `statics`
  (model path, bounds), exterior R-tree, cell cache terrain.
- Per-worldspace LOD origins (new `worldspaces` columns; required by
  GEOM-02, missing today): use a valid `lodsettings/<worldspace>.lod`
  sidecar or an explicit origin for a custom world. Otherwise skip that
  world's LOD with an actionable error; never assume an origin of zero.
- Reference header flags and enable-state columns (new; `XESP` parent +
  inversion flags), extracted at convert time so eligibility and runtime
  queries do not depend on per-query blob parsing.
- Rule set: explicit record rule, then component/type default, then
  omit-with-reason. Every decision records winner, rule, model, and
  fallback reason (ARCH-02, RULE-03).

## Selection

Per-tier source choice: a landmark may reuse its full GLB at tier 4 while
clutter drops out before tier 16 (GEOM-03). Authored `_lod` NIF variants
match by convention where present. The vendor parser reads
`BSLODTriShape.lod_sizes` and `BSSubIndexTriShape` segment data, but the
current exporter passes only each block's base `BSTriShape` to the generic
mesh path. Those per-level counts and segment ranges are lost. Preserve and
validate them before using these blocks as tier-specific sources. Missing
source means omit-with-reason, never silent near-match substitution (RULE-05).
The first static pass admits only verified, unconditionally enabled fixed
`STAT` references. `statics` is a model catalog, not an eligibility flag;
unknown, initially disabled, XESP-controlled, movable, animated, and
unresolved large-reference cases are omitted with reasons until a validated
individual proxy or handoff route exists.

## Chunk build

- Transform in double precision, then emit chunk-local coordinates;
  transform normals and winding with the shape transform (GEOM-04).
- Emit a stable node per source cell in each GLB chunk, with independently
  hideable terrain and object groups. Partition batches beneath each group by
  rendering compatibility (material family, alpha mode, overlay identity),
  not by texture filename (MAT-02).
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

Chunk payloads are GLB files with the database holding a spatial index,
per ADR-0010: chunk key `(world, tier, anchor)`, bounds, batch list,
content hashes, and manifest references, plus an R-tree over world bounds
for range queries. Each chunk ships conservative bounds, a material batch
list, source-cell/group node identity for handoff (GEOM-05), and a manifest entry:
content hashes of inputs, rule set, settings, and compiler version
(BUILD-01/02). DB, manifest, and chunk records share one build identity;
publish only after every referenced payload validates. The `lod` reshape
ships with a world DB version bump.

GLB vertex and accessor bounds are chunk-local, relative to the chunk root.
Database bounds and R-tree XY bounds are world-space Creation units: translate
the chunk-local X/Y extent by `(origin + anchor * tier_side_cells) * 4096`; Z
is unchanged. Never index chunk-local coordinates as world-space bounds.

## Incremental builds

Reference moves propagate to every affected tier, world copy, and variant
(BUILD-03). Atlas pixel changes avoid remeshing only when UV layout and
material contracts are unchanged (BUILD-02). Partial builds must equal
clean builds semantically or be marked unsafe (BUILD-04); a dry-run impact
report precedes expensive regeneration (BUILD-05).
