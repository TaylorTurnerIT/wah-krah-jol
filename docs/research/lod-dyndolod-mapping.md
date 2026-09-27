# LOD: DynDOLOD requirement mapping

Maps the DynDOLOD reimplementation spec (NATIVE target only) onto this repo.
Verdicts: **Reuse** (exists, extend it), **Build** (design below), **Defer**
(explicitly out of early phases). No SKYRIM byte-compat work is planned:
no `.BTO`, `.BTT`, `.LST`, `RNAM`, or `TVDT` output.

## Architecture (ARCH)

| Req | Subject | Verdict | Repo anchor |
|-----|---------|---------|-------------|
| ARCH-01 | Canonical IDs separate from runtime IDs | Reuse | `formid_map`, `ReferenceRow` in `crates/engine/src/world/database.rs` |
| ARCH-02 | Provenance per decision | Reuse | Pipeline manifests, `AssetFailure` chains in `streaming.rs` |
| ARCH-03 | Separate block size, mesh detail, distance, variant | Build | `docs/specs/engine/lod-architecture.md` |
| ARCH-04 | Immutable build snapshot | Reuse | Staged outputs + atomic publish in `crates/converter/src/cache.rs` |
| ARCH-05 | Generated content outside sources | Reuse | Staging dir, `invalidate_staged_mesh_outputs` in `pipeline.rs` |

## Input and classification (INPUT)

| Req | Subject | Verdict | Note |
|-----|---------|---------|------|
| INPUT-01/02 | Plugin loading, VFS precedence | Reuse | `asset_path`, archive overlay in load order |
| INPUT-04 | Scale, rotation, enable parents | Build | Transforms exist; `XESP`/inversion unextracted (`exporter.rs` has no `XESP`) |
| INPUT-05 | Season/swap resolution | Defer | No seasonal or swap consumer exists |
| INPUT-06 | Component-level classification | Build | Per-shape material contract is the pattern to follow |
| INPUT-07 | Gameplay refs stay authoritative | Reuse | Converter never mutates source records |

## Rules and content (RULE/CONTENT)

Ordered rule engine with `explain(reference)` is Build; start minimal
(explicit record rule, then type default, then omit-with-reason). A shipped
content library is Defer; early phases reuse converted full assets.

## Textures and billboards (TEX/MAT)

| Req | Subject | Verdict | Note |
|-----|---------|---------|------|
| TEX-01..05 | Recipe-driven render, framing, pivot | Build | `docs/specs/converters/billboard-generation.md` |
| TEX-06 | Mip coverage, gutters | Build | Same doc; MASK cutoff 128 already matches (`material.rs:451`) |
| TEX-08 | No recursion into generated assets | Reuse | Staging/source separation |
| MAT-01/02 | Material batching by compatibility | Reuse | `NifShapeMaterial` contract per shape |
| MAT-04 | UV repeat fallback | Build | Direct-texture escape hatch in chunk compiler |

## Static compilation (GEOM)

Fixed 4/8/16 tiers first; per-worldspace LOD origins are new DB columns
(`worldspaces` holds only id/editor/parent/flags today). Subcell handoff
must be designed against single-cell streaming keys. See
`docs/specs/converters/lod-compiler.md`.

## Trees and grass (TREE/GRASS)

Object-route billboards first (TREE-03/04); tilted and fallen transforms
preserved. Standard-tree path (TREE-01/02) is out of scope for NATIVE.
Grass LOD is Deferred: `GRAS` is not exported and no placement cache
reader exists.

## Runtime (RUN/GLOW)

Enable-state proxies, near/far/persistent classes, epochs on travel/load,
and the constant-vs-external emittance split are Build; see
`docs/specs/engine/lod-runtime-proxies.md`. Animation stays at the
approved-asset list (windmill/waterfall/flame pattern); no door pose,
destruction, or script-variable sync.

## World, map, occlusion (WORLD/MAP/OCC/UNDER)

Child/parent city copies, map-only level 32, precomputed occlusion, and
underside geometry are all Deferred. The engine already has runtime HZB
occlusion with a proof bridge (`render.rs`), which covers the near field;
revisit precomputed data only with measured far-field need.

## Workflow and builds (UX/BUILD/PERF)

Reuse: versioned presets, noninteractive CLI on the same settings model,
transactional publish with last-good preserved, content-hash invalidation,
dry-run impact. New: per-tier/per-category budgets, tier captures, handoff
sequences, rebuild-equivalence checks. See Phase 5 in `lod-architecture.md`.
