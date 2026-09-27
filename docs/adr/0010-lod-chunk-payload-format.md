# ADR-0010: LOD chunk payloads are GLB files, indexed by manifest

- **Status:** Proposed
- **Date:** 2026-09-27

## Context

Phase 2 of the LOD plan needs a storage format for spatial chunks. Two
candidates exist. The `lod` table (`exporter.rs`) has a `(cell_id,
lod_level, mesh_data)` schema but no writers or readers, so it is a name,
not a working path. The GLB path is fully worked: atomic publish,
structural validation, bounds audit, `AssetServer` loading, readiness
tracking with dependency chains, and schema-versioned invalidation.

## Decision

Chunks ship as GLB files through the existing asset pipeline, with the
database holding an index, not blobs: chunk key `(world, tier, anchor)`,
bounds, batch list, content hashes, and manifest references. The `lod`
table is reshaped to that index or replaced; it never stores geometry.

## Consequences

- Chunk loading reuses strict readiness, fallback diagnostics, and cache
  invalidation instead of inventing a parallel binary path.
- Very large worlds pay file-per-chunk overhead; mitigate with tier
  packaging (one file per tier per region) if measured, not upfront.
- The `lod` table schema change rides the next world DB version bump with
  the LOD-origin columns.
