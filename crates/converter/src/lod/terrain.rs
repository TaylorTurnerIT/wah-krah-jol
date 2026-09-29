//! Phase 1 terrain chunk compiler: coarse terrain GLB per chunk.
//!
//! One file per chunk, one stable node per source cell, a `terrain` group
//! beneath each cell node holding that cell's coarse terrain batches
//! (GEOM-05). Phase 2 adds the sibling `objects` group; the runtime hides
//! each group independently when the matching full cell (or nearer tier) is
//! drawable.
//!
//! Coordinates: chunk-local Creation units. The runtime parents the spawned
//! scene under a chunk root positioned at the chunk's world origin, exactly
//! like `cell_translation` for full cells. Vertices never bake the chunk's
//! world offset, so a payload is correct wherever its chunk is placed and a
//! stale root transform cannot smear one chunk's terrain onto another's.

use color_eyre::{Result, eyre::WrapErr};
use rusqlite::{Connection, params};
use shared::lod::{ChunkAnchor, ChunkKey, LodOrigin, LodTier, chunk_payload_path, nodes};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

/// Exterior cells of 4096 Creation units.
const CELL_SIZE: f64 = 4096.0;

/// Coarse grid side length per cell: LAND's 33x33 field decimated by picking
/// every 8th sample (33 = 4 * 8 + 1), so the coarse grid shares the corner
/// and edge samples with the full field and adjacent chunks meet on the same
/// points where both cells kept their authored heights.
const COARSE_STRIDE: usize = 8;
const COARSE_SIDE: usize = 5;
const QUADRANTS: [(&str, &str, usize, usize); 4] = [
    ("sw", "southwest", 0, 0),
    ("se", "southeast", 2, 0),
    ("nw", "northwest", 0, 2),
    ("ne", "northeast", 2, 2),
];

/// One compiled chunk: its key, the cells it covers, its world bounds, and
/// the GLB bytes to publish.
pub struct TerrainChunk {
    pub key: ChunkKey,
    pub cells: Vec<TerrainCellInput>,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub glb: Vec<u8>,
}

/// One source cell's terrain as the compiler reads it: absolute cell grid
/// coordinates plus the 33x33 height field in Creation units.
pub struct TerrainCellInput {
    pub cell_id: u32,
    pub grid_x: i32,
    pub grid_y: i32,
    pub heights: Vec<f32>,
    /// Packed RGB terrain colors; empty means the source LAND omitted VCLR.
    pub vertex_colors: Vec<u8>,
}

/// Compiles every tier-4/8/16 chunk covering `cells` for one worldspace.
/// Cells outside every compiled chunk cannot happen: each cell anchors to
/// exactly one chunk per tier. Returns chunks sorted by `(tier, anchor)`.
pub fn compile_world_terrain(
    worldspace_id: u32,
    origin: LodOrigin,
    cells: &[TerrainCellInput],
) -> Result<Vec<TerrainChunk>> {
    let mut chunks = Vec::new();
    for tier in LodTier::ALL {
        let mut by_anchor: BTreeMap<(i32, i32), Vec<&TerrainCellInput>> = BTreeMap::new();
        for cell in cells {
            let anchor = origin.chunk_for_cell(tier, cell.grid_x, cell.grid_y);
            by_anchor
                .entry((anchor.x, anchor.y))
                .or_default()
                .push(cell);
        }
        for ((ax, ay), members) in by_anchor {
            let key = ChunkKey::new(worldspace_id, tier, ChunkAnchor::new(ax, ay));
            chunks.push(compile_chunk(key, origin, &members)?);
        }
    }
    Ok(chunks)
}

fn compile_chunk(
    key: ChunkKey,
    origin: LodOrigin,
    members: &[&TerrainCellInput],
) -> Result<TerrainChunk> {
    color_eyre::eyre::ensure!(!members.is_empty(), "chunk {key:?} covers no source cells");
    let mut seen = BTreeSet::new();
    for cell in members {
        color_eyre::eyre::ensure!(
            seen.insert((cell.grid_x, cell.grid_y)),
            "chunk {key:?} covers cell ({}, {}) twice",
            cell.grid_x,
            cell.grid_y
        );
        color_eyre::eyre::ensure!(
            cell.heights.len() == 33 * 33,
            "cell ({}, {}) has {} heights, expected 1089",
            cell.grid_x,
            cell.grid_y,
            cell.heights.len()
        );
        color_eyre::eyre::ensure!(
            cell.heights.iter().all(|height| height.is_finite()),
            "cell ({}, {}) has a non-finite height",
            cell.grid_x,
            cell.grid_y
        );
        color_eyre::eyre::ensure!(
            cell.vertex_colors.is_empty() || cell.vertex_colors.len() == 33 * 33 * 3,
            "cell ({}, {}) has {} vertex-color bytes, expected 0 or {}",
            cell.grid_x,
            cell.grid_y,
            cell.vertex_colors.len(),
            33 * 33 * 3
        );
    }
    // Chunk-local layout: each member's coarse grid sits at its offset from
    // the chunk's minimum cell, in Creation units with the Creation axis
    // convention (x east, y north, z up). The (min_x, min_y) cell's southwest
    // corner is the chunk local origin.
    let side = key.tier.side_cells();
    let (min_rel_x, min_rel_y) = (key.anchor.x * side, key.anchor.y * side);
    let mut positions: Vec<f32> = Vec::new();
    let mut normals: Vec<f32> = Vec::new();
    let mut uvs: Vec<f32> = Vec::new();
    let mut colors: Vec<f32> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut cell_ranges: Vec<(i32, i32, u32)> = Vec::new();
    let mut local_bounds_min = [f32::INFINITY; 3];
    let mut local_bounds_max = [f32::NEG_INFINITY; 3];
    let mut members_sorted: Vec<&&TerrainCellInput> = members.iter().collect();
    members_sorted.sort_by_key(|cell| (cell.grid_x, cell.grid_y));
    for cell in members_sorted {
        let rel_x = cell.grid_x - origin.grid_x - min_rel_x;
        let rel_y = cell.grid_y - origin.grid_y - min_rel_y;
        color_eyre::eyre::ensure!(
            (0..side).contains(&rel_x) && (0..side).contains(&rel_y),
            "cell ({}, {}) is outside chunk {key:?}",
            cell.grid_x,
            cell.grid_y
        );
        let base_x = rel_x as f64 * CELL_SIZE;
        let base_y = rel_y as f64 * CELL_SIZE;
        let coarse = decimate(&cell.heights);
        let coarse_colors = decimate_colors(&cell.vertex_colors);
        let step = CELL_SIZE / (COARSE_SIDE - 1) as f64;
        for (_, _, origin_x, origin_y) in QUADRANTS {
            for local_y in 0..3 {
                for local_x in 0..3 {
                    let x = origin_x + local_x;
                    let y = origin_y + local_y;
                    let sample = y * COARSE_SIDE + x;
                    let position = [
                        (base_x + x as f64 * step) as f32,
                        (base_y + y as f64 * step) as f32,
                        coarse[sample],
                    ];
                    for axis in 0..3 {
                        local_bounds_min[axis] = local_bounds_min[axis].min(position[axis]);
                        local_bounds_max[axis] = local_bounds_max[axis].max(position[axis]);
                    }
                    positions.extend_from_slice(&position);
                    normals.extend_from_slice(&coarse_normal(&coarse, x, y, step as f32));
                    uvs.extend_from_slice(&[x as f32 / 4.0, y as f32 / 4.0]);
                    let color = coarse_colors[sample];
                    colors.extend_from_slice(&[
                        f32::from(color[0]) / 255.0,
                        f32::from(color[1]) / 255.0,
                        f32::from(color[2]) / 255.0,
                        1.0,
                    ]);
                }
            }
            for y in 0..2 {
                for x in 0..2 {
                    // Each quadrant has its own POSITION accessor.
                    let a = (y * 3 + x) as u32;
                    let b = a + 1;
                    let c = a + 3;
                    let d = c + 1;
                    // Keep the winding used by full-detail terrain.
                    indices.extend_from_slice(&[a, b, c, b, d, c]);
                }
            }
        }
        cell_ranges.push((cell.grid_x, cell.grid_y, cell.cell_id));
    }
    let glb = emit_chunk_glb(
        key,
        &cell_ranges,
        &positions,
        &normals,
        &uvs,
        &colors,
        &indices,
    )?;
    // The GLB is placed under a chunk root, so its vertices stay local. The
    // database bounds instead describe that geometry in absolute cell-grid
    // Creation coordinates for world-space R-tree queries.
    let chunk_world_x = (origin.grid_x as f64 + key.anchor.x as f64 * side as f64) * CELL_SIZE;
    let chunk_world_y = (origin.grid_y as f64 + key.anchor.y as f64 * side as f64) * CELL_SIZE;
    let bounds_min = [
        (local_bounds_min[0] as f64 + chunk_world_x) as f32,
        (local_bounds_min[1] as f64 + chunk_world_y) as f32,
        local_bounds_min[2],
    ];
    let bounds_max = [
        (local_bounds_max[0] as f64 + chunk_world_x) as f32,
        (local_bounds_max[1] as f64 + chunk_world_y) as f32,
        local_bounds_max[2],
    ];
    Ok(TerrainChunk {
        key,
        cells: members
            .iter()
            .map(|cell| TerrainCellInput {
                cell_id: cell.cell_id,
                grid_x: cell.grid_x,
                grid_y: cell.grid_y,
                heights: cell.heights.clone(),
                vertex_colors: cell.vertex_colors.clone(),
            })
            .collect(),
        bounds_min,
        bounds_max,
        glb,
    })
}

/// Picks every 8th sample of the 33x33 field: a 5x5 grid sharing corners
/// and edges with the full field.
fn decimate(heights: &[f32]) -> Vec<f32> {
    let mut coarse = Vec::with_capacity(COARSE_SIDE * COARSE_SIDE);
    for y in 0..COARSE_SIDE {
        for x in 0..COARSE_SIDE {
            coarse.push(heights[y * COARSE_STRIDE * 33 + x * COARSE_STRIDE]);
        }
    }
    coarse
}

fn decimate_colors(colors: &[u8]) -> Vec<[u8; 3]> {
    let mut coarse = Vec::with_capacity(COARSE_SIDE * COARSE_SIDE);
    for y in 0..COARSE_SIDE {
        for x in 0..COARSE_SIDE {
            let index = (y * COARSE_STRIDE * 33 + x * COARSE_STRIDE) * 3;
            coarse.push(if colors.is_empty() {
                [255; 3]
            } else {
                [colors[index], colors[index + 1], colors[index + 2]]
            });
        }
    }
    coarse
}

/// A height-field normal from the coarse grid's own neighbours, in the
/// compiler's axis convention (x east, y north, z up), normalized.
fn coarse_normal(coarse: &[f32], x: usize, y: usize, step: f32) -> [f32; 3] {
    let at = |x: usize, y: usize| coarse[y * COARSE_SIDE + x];
    let left = at(x.saturating_sub(1), y);
    let right = at((x + 1).min(COARSE_SIDE - 1), y);
    let down = at(x, y.saturating_sub(1));
    let up = at(x, (y + 1).min(COARSE_SIDE - 1));
    let normal = [left - right, down - up, 2.0 * step];
    let length = (normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]).sqrt();
    [normal[0] / length, normal[1] / length, normal[2] / length]
}

/// Builds the chunk GLB with one stable source-cell node, a `terrain` group,
/// and four named quadrant nodes/meshes per cell. The scene root carries the
/// axis-convention rotation used by full-detail terrain.
///
/// Handoff identity lives in each source-cell node's `extras.cell_id`; child
/// lookup is by stable group/quadrant name, never by traversal order.
fn emit_chunk_glb(
    key: ChunkKey,
    cell_ranges: &[(i32, i32, u32)],
    positions: &[f32],
    normals: &[f32],
    uvs: &[f32],
    colors: &[f32],
    indices: &[u32],
) -> Result<Vec<u8>> {
    color_eyre::eyre::ensure!(
        positions.len().is_multiple_of(3)
            && normals.len() == positions.len()
            && uvs.len() / 2 == positions.len() / 3
            && colors.len() / 4 == positions.len() / 3,
        "chunk {key:?} has mismatched terrain attribute arrays"
    );
    let expected_vertices = cell_ranges.len() * 4 * 9;
    let expected_indices = cell_ranges.len() * 4 * 24;
    color_eyre::eyre::ensure!(
        positions.len() / 3 == expected_vertices && indices.len() == expected_indices,
        "chunk {key:?} does not contain four 3x3 terrain quadrants per cell"
    );
    // One binary buffer with tightly packed streams for each vertex attribute
    // followed by per-quadrant indices.
    let mut binary: Vec<u8> = Vec::new();
    binary.extend_from_slice(f32_slice_bytes(positions));
    let normal_offset = binary.len();
    binary.extend_from_slice(f32_slice_bytes(normals));
    let uv_offset = binary.len();
    binary.extend_from_slice(f32_slice_bytes(uvs));
    let color_offset = binary.len();
    binary.extend_from_slice(f32_slice_bytes(colors));
    pad_to_4(&mut binary);
    let index_offset = binary.len();
    for index in indices {
        binary.extend_from_slice(&index.to_le_bytes());
    }
    pad_to_4(&mut binary);

    let chunk_name = format!(
        "chunk_{}_{}_{}",
        key.tier.side_cells(),
        key.anchor.x,
        key.anchor.y
    );
    let mut nodes_json = Vec::new();
    let mut meshes_json = Vec::new();
    let mut accessors_json = Vec::new();
    // Node 0 is the scene root, carrying the Creation-to-glTF axis rotation:
    // Creation x-east/y-north/z-up becomes glTF x-east/y-up/z-south, the
    // same rotation `shared::coordinates` applies to full-detail points,
    // expressed as a column-major node matrix.
    nodes_json.push(serde_json::json!({
        "name": chunk_name,
        "matrix": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, -1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
        "children": (0..cell_ranges.len()).map(|i| 1 + i * 6).collect::<Vec<_>>(),
    }));
    for (cell_index, (grid_x, grid_y, cell_id)) in cell_ranges.iter().enumerate() {
        let cell_node = 1 + cell_index * 6;
        let group_node = cell_node + 1;
        nodes_json.push(serde_json::json!({
            "name": nodes::source_cell(*grid_x, *grid_y),
            "extras": { "cell_id": cell_id },
            "children": [group_node],
        }));
        nodes_json.push(serde_json::json!({
            "name": nodes::terrain_group(),
            "children": (0..4).map(|quadrant| group_node + 1 + quadrant).collect::<Vec<_>>(),
        }));
        for (quadrant, (short_name, full_name, _, _)) in QUADRANTS.iter().enumerate() {
            let quadrant_index = cell_index * 4 + quadrant;
            let start_vertex = quadrant_index * 9;
            let (quad_min, quad_max) = cell_bounds(positions, start_vertex);
            let accessor_base = quadrant_index * 5;
            let quad_node = group_node + 1 + quadrant;
            nodes_json.push(serde_json::json!({
                "name": format!("terrain_quadrant_{short_name}"),
                "extras": { "quadrant": full_name },
                "mesh": quadrant_index,
            }));
            accessors_json.push(serde_json::json!({
                "bufferView": 0,
                "byteOffset": start_vertex * 12,
                "componentType": 5126,
                "count": 9,
                "type": "VEC3",
                "min": quad_min,
                "max": quad_max,
            }));
            accessors_json.push(serde_json::json!({
                "bufferView": 1,
                "byteOffset": start_vertex * 12,
                "componentType": 5126,
                "count": 9,
                "type": "VEC3",
            }));
            accessors_json.push(serde_json::json!({
                "bufferView": 2,
                "byteOffset": start_vertex * 8,
                "componentType": 5126,
                "count": 9,
                "type": "VEC2",
            }));
            accessors_json.push(serde_json::json!({
                "bufferView": 3,
                "byteOffset": start_vertex * 16,
                "componentType": 5126,
                "count": 9,
                "type": "VEC4",
            }));
            accessors_json.push(serde_json::json!({
                "bufferView": 4,
                "byteOffset": quadrant_index * 24 * 4,
                "componentType": 5125,
                "count": 24,
                "type": "SCALAR",
            }));
            meshes_json.push(serde_json::json!({
                "name": format!("terrain_{grid_x}_{grid_y}_quadrant_{short_name}"),
                "primitives": [{
                    "attributes": {
                        "POSITION": accessor_base,
                        "NORMAL": accessor_base + 1,
                        "TEXCOORD_0": accessor_base + 2,
                        "COLOR_0": accessor_base + 3,
                    },
                    "indices": accessor_base + 4,
                    "material": 0,
                }],
            }));
            debug_assert_eq!(quad_node, nodes_json.len() - 1);
        }
    }
    let json = serde_json::json!({
        "asset": { "version": "2.0", "generator": "OpenSkyrim LOD terrain compiler" },
        "scene": 0,
        "scenes": [{ "name": chunk_name, "nodes": [0] }],
        "nodes": nodes_json,
        "meshes": meshes_json,
        "accessors": accessors_json,
        "bufferViews": [
            { "buffer": 0, "byteOffset": 0, "byteLength": positions.len() * 4, "target": 34962 },
            { "buffer": 0, "byteOffset": normal_offset, "byteLength": normals.len() * 4, "target": 34962 },
            { "buffer": 0, "byteOffset": uv_offset, "byteLength": uvs.len() * 4, "target": 34962 },
            { "buffer": 0, "byteOffset": color_offset, "byteLength": colors.len() * 4, "target": 34962 },
            { "buffer": 0, "byteOffset": index_offset, "byteLength": indices.len() * 4, "target": 34963 },
        ],
        "buffers": [{ "byteLength": binary.len() }],
        "materials": [{
            "name": "lod_terrain",
            "pbrMetallicRoughness": { "baseColorFactor": [1.0, 1.0, 1.0, 1.0], "roughnessFactor": 0.92 },
            "doubleSided": true,
        }],
    });
    let mut json_bytes = serde_json::to_vec(&json)?;
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total_length = 12 + 8 + json_bytes.len() + 8 + binary.len();
    let mut glb = Vec::with_capacity(total_length);
    glb.extend_from_slice(b"glTF");
    glb.extend_from_slice(&2u32.to_le_bytes());
    glb.extend_from_slice(&(total_length as u32).to_le_bytes());
    glb.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"JSON");
    glb.extend_from_slice(&json_bytes);
    glb.extend_from_slice(&(binary.len() as u32).to_le_bytes());
    glb.extend_from_slice(b"BIN\x00");
    glb.extend_from_slice(&binary);
    Ok(glb)
}

fn cell_bounds(positions: &[f32], start_vertex: usize) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for vertex in 0..9 {
        let base = (start_vertex + vertex) * 3;
        for axis in 0..3 {
            min[axis] = min[axis].min(positions[base + axis]);
            max[axis] = max[axis].max(positions[base + axis]);
        }
    }
    (min, max)
}

fn f32_slice_bytes(values: &[f32]) -> &[u8] {
    // SAFETY: `f32` is `repr(Rust)` but plain-old-data with no padding; a
    // shared slice reborrow as bytes reads exactly the values' storage.
    unsafe { std::slice::from_raw_parts(values.as_ptr() as *const u8, values.len() * 4) }
}

fn pad_to_4(bytes: &mut Vec<u8>) {
    while !bytes.len().is_multiple_of(4) {
        bytes.push(0);
    }
}

/// Validates the emitted GLB container, accessor ranges, scene references,
/// and every triangle index before the payload is indexed or published.
fn validate_terrain_glb(bytes: &[u8]) -> Result<()> {
    color_eyre::eyre::ensure!(bytes.len() >= 28, "GLB is truncated");
    color_eyre::eyre::ensure!(&bytes[..4] == b"glTF", "GLB magic is invalid");
    color_eyre::eyre::ensure!(read_u32(bytes, 4)? == 2, "GLB version is not 2");
    color_eyre::eyre::ensure!(
        read_u32(bytes, 8)? as usize == bytes.len(),
        "GLB header length does not match file length"
    );
    let json_length = read_u32(bytes, 12)? as usize;
    color_eyre::eyre::ensure!(
        json_length.is_multiple_of(4),
        "GLB JSON chunk is misaligned"
    );
    color_eyre::eyre::ensure!(&bytes[16..20] == b"JSON", "first GLB chunk is not JSON");
    let json_start = 20usize;
    let json_end = json_start
        .checked_add(json_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB JSON length overflows"))?;
    color_eyre::eyre::ensure!(json_end + 8 <= bytes.len(), "GLB JSON chunk is truncated");
    let json: serde_json::Value = serde_json::from_slice(&bytes[json_start..json_end])
        .wrap_err("GLB JSON chunk is invalid")?;
    let binary_length = read_u32(bytes, json_end)? as usize;
    color_eyre::eyre::ensure!(
        binary_length.is_multiple_of(4),
        "GLB BIN chunk is misaligned"
    );
    color_eyre::eyre::ensure!(
        &bytes[json_end + 4..json_end + 8] == b"BIN\0",
        "second GLB chunk is not BIN"
    );
    let binary_start = json_end + 8;
    let binary_end = binary_start
        .checked_add(binary_length)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB binary length overflows"))?;
    color_eyre::eyre::ensure!(
        binary_end == bytes.len(),
        "GLB binary chunk length is invalid"
    );

    let asset = json["asset"]
        .as_object()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB asset header is missing"))?;
    color_eyre::eyre::ensure!(
        asset.get("version").and_then(|v| v.as_str()) == Some("2.0"),
        "GLB asset version is not 2.0"
    );
    let buffers = json["buffers"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB buffers are missing"))?;
    color_eyre::eyre::ensure!(
        buffers.len() == 1,
        "terrain GLB must use one embedded buffer"
    );
    let declared_buffer_length = json_usize(&buffers[0]["byteLength"], "buffer byteLength")?;
    color_eyre::eyre::ensure!(
        declared_buffer_length <= binary_length,
        "GLB buffer exceeds the BIN chunk"
    );

    let views = json["bufferViews"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB bufferViews are missing"))?;
    let mut view_ranges = Vec::with_capacity(views.len());
    for (index, view) in views.iter().enumerate() {
        color_eyre::eyre::ensure!(
            json_usize(&view["buffer"], "bufferView buffer")? == 0,
            "bufferView {index} does not reference buffer 0"
        );
        let offset = view
            .get("byteOffset")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize;
        let length = json_usize(&view["byteLength"], "bufferView byteLength")?;
        let end = offset
            .checked_add(length)
            .ok_or_else(|| color_eyre::eyre::eyre!("bufferView {index} range overflows"))?;
        color_eyre::eyre::ensure!(
            end <= declared_buffer_length,
            "bufferView {index} exceeds the embedded buffer"
        );
        view_ranges.push((offset, length));
    }

    let accessors = json["accessors"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB accessors are missing"))?;
    for (index, accessor) in accessors.iter().enumerate() {
        let view_index = json_usize(&accessor["bufferView"], "accessor bufferView")?;
        let (view_offset, view_length) = *view_ranges.get(view_index).ok_or_else(|| {
            color_eyre::eyre::eyre!("accessor {index} references missing bufferView {view_index}")
        })?;
        let component_type = json_usize(&accessor["componentType"], "accessor componentType")?;
        let component_size = match component_type {
            5121 => 1,
            5123 => 2,
            5125 | 5126 => 4,
            _ => color_eyre::eyre::bail!(
                "accessor {index} has unsupported component type {component_type}"
            ),
        };
        let dimensions = match accessor["type"].as_str() {
            Some("SCALAR") => 1,
            Some("VEC2") => 2,
            Some("VEC3") => 3,
            Some("VEC4") => 4,
            _ => color_eyre::eyre::bail!("accessor {index} has an invalid type"),
        };
        let count = json_usize(&accessor["count"], "accessor count")?;
        let offset = accessor
            .get("byteOffset")
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as usize;
        color_eyre::eyre::ensure!(
            offset.is_multiple_of(component_size),
            "accessor {index} is misaligned"
        );
        let byte_length = count
            .checked_mul(component_size * dimensions)
            .ok_or_else(|| color_eyre::eyre::eyre!("accessor {index} byte length overflows"))?;
        let end = offset
            .checked_add(byte_length)
            .ok_or_else(|| color_eyre::eyre::eyre!("accessor {index} range overflows"))?;
        color_eyre::eyre::ensure!(
            end <= view_length,
            "accessor {index} exceeds its bufferView"
        );
        color_eyre::eyre::ensure!(
            view_offset + end <= binary_length,
            "accessor {index} exceeds the BIN chunk"
        );
    }

    let meshes = json["meshes"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB meshes are missing"))?;
    color_eyre::eyre::ensure!(!meshes.is_empty(), "terrain GLB has no meshes");
    let materials = json["materials"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("terrain GLB materials are missing"))?;
    color_eyre::eyre::ensure!(
        materials.len() == 1
            && materials[0]["name"].as_str() == Some("lod_terrain")
            && materials[0]["pbrMetallicRoughness"]["baseColorFactor"]
                == serde_json::json!([1.0, 1.0, 1.0, 1.0])
            && materials[0]["pbrMetallicRoughness"]["roughnessFactor"] == 0.92
            && materials[0]["doubleSided"] == true,
        "terrain GLB material contract is invalid"
    );
    for (mesh_index, mesh) in meshes.iter().enumerate() {
        let primitives = mesh["primitives"]
            .as_array()
            .ok_or_else(|| color_eyre::eyre::eyre!("mesh {mesh_index} has no primitives"))?;
        for (primitive_index, primitive) in primitives.iter().enumerate() {
            let context = format!("mesh {mesh_index} primitive {primitive_index}");
            color_eyre::eyre::ensure!(
                primitive
                    .get("mode")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(4)
                    == 4,
                "{context} is not a triangle list"
            );
            let position_index =
                json_usize(&primitive["attributes"]["POSITION"], "POSITION accessor")?;
            let position = accessors.get(position_index).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "{context} references missing POSITION accessor {position_index}"
                )
            })?;
            color_eyre::eyre::ensure!(
                position["type"].as_str() == Some("VEC3")
                    && json_usize(&position["componentType"], "POSITION componentType")? == 5126,
                "{context} POSITION accessor is not Float32 VEC3"
            );
            let attributes = primitive["attributes"]
                .as_object()
                .ok_or_else(|| color_eyre::eyre::eyre!("{context} has no attribute map"))?;
            for semantic in ["POSITION", "NORMAL", "TEXCOORD_0", "COLOR_0"] {
                color_eyre::eyre::ensure!(
                    attributes.contains_key(semantic),
                    "{context} is missing required {semantic} attribute"
                );
            }
            color_eyre::eyre::ensure!(
                attributes.len() == 4,
                "{context} has unexpected vertex attributes"
            );
            for (semantic, attribute) in attributes {
                let accessor_index = json_usize(attribute, "attribute accessor")?;
                let accessor = accessors.get(accessor_index).ok_or_else(|| {
                    color_eyre::eyre::eyre!(
                        "{context} {semantic} references missing accessor {accessor_index}"
                    )
                })?;
                color_eyre::eyre::ensure!(
                    json_usize(&accessor["count"], "attribute count")?
                        == json_usize(&position["count"], "POSITION count")?,
                    "{context} {semantic} count does not match POSITION"
                );
                let (expected_type, expected_component) = match semantic.as_str() {
                    "POSITION" | "NORMAL" => ("VEC3", 5126),
                    "TEXCOORD_0" => ("VEC2", 5126),
                    "COLOR_0" => ("VEC4", 5126),
                    _ => color_eyre::eyre::bail!("{context} has unsupported attribute {semantic}"),
                };
                color_eyre::eyre::ensure!(
                    accessor["type"].as_str() == Some(expected_type)
                        && json_usize(&accessor["componentType"], "attribute componentType")?
                            == expected_component,
                    "{context} {semantic} accessor has the wrong format"
                );
                if semantic == "TEXCOORD_0" || semantic == "COLOR_0" {
                    let dimensions = if semantic == "TEXCOORD_0" { 2 } else { 4 };
                    let (view_offset, _) =
                        view_ranges[json_usize(&accessor["bufferView"], "attribute bufferView")?];
                    let accessor_offset = accessor
                        .get("byteOffset")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0) as usize;
                    let values_start = binary_start + view_offset + accessor_offset;
                    let value_count =
                        json_usize(&accessor["count"], "attribute count")? * dimensions;
                    for value_index in 0..value_count {
                        let value = read_f32(bytes, values_start + value_index * 4)?;
                        color_eyre::eyre::ensure!(
                            value.is_finite() && (0.0..=1.0).contains(&value),
                            "{context} {semantic} value {value} is outside [0, 1]"
                        );
                    }
                }
            }
            let index_accessor_index = json_usize(&primitive["indices"], "index accessor")?;
            let index_accessor = accessors.get(index_accessor_index).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "{context} references missing index accessor {index_accessor_index}"
                )
            })?;
            color_eyre::eyre::ensure!(
                index_accessor["type"].as_str() == Some("SCALAR")
                    && json_usize(&index_accessor["componentType"], "index componentType")? == 5125,
                "{context} indices are not unsigned 32-bit scalars"
            );
            let index_count = json_usize(&index_accessor["count"], "index count")?;
            color_eyre::eyre::ensure!(
                index_count > 0 && index_count.is_multiple_of(3),
                "{context} has an incomplete triangle list"
            );
            let position_count = json_usize(&position["count"], "POSITION count")?;
            let (view_offset, _) =
                view_ranges[json_usize(&index_accessor["bufferView"], "index bufferView")?];
            let accessor_offset = index_accessor
                .get("byteOffset")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as usize;
            let indices_start = binary_start + view_offset + accessor_offset;
            for index in 0..index_count {
                let start = indices_start + index * 4;
                let value = read_u32(bytes, start)? as usize;
                color_eyre::eyre::ensure!(
                    value < position_count,
                    "{context} index {value} exceeds POSITION count {position_count}"
                );
            }
            let material_index = json_usize(&primitive["material"], "primitive material")?;
            color_eyre::eyre::ensure!(
                material_index == 0,
                "{context} does not use the terrain material"
            );
        }
    }

    let nodes = json["nodes"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB nodes are missing"))?;
    let mut source_cell_ids = BTreeSet::new();
    for (node_index, node) in nodes.iter().enumerate() {
        if let Some(mesh) = node.get("mesh") {
            color_eyre::eyre::ensure!(
                json_usize(mesh, "node mesh")? < meshes.len(),
                "node {node_index} references a missing mesh"
            );
        }
        if let Some(children) = node.get("children").and_then(|value| value.as_array()) {
            for child in children {
                color_eyre::eyre::ensure!(
                    json_usize(child, "node child")? < nodes.len(),
                    "node {node_index} references a missing child"
                );
            }
        }
        if let Some(name) = node["name"]
            .as_str()
            .filter(|name| name.starts_with("cell_"))
        {
            let cell_id = json_usize(&node["extras"]["cell_id"], "source cell extras.cell_id")?;
            color_eyre::eyre::ensure!(
                source_cell_ids.insert(cell_id),
                "GLB has duplicate source cell id {cell_id}"
            );
            let children = node["children"]
                .as_array()
                .ok_or_else(|| color_eyre::eyre::eyre!("source cell node has no terrain group"))?;
            color_eyre::eyre::ensure!(
                children.len() == 1,
                "source cell node must have exactly one terrain group"
            );
            let group_index = json_usize(&children[0], "terrain group node")?;
            let group = &nodes[group_index];
            color_eyre::eyre::ensure!(
                group["name"].as_str() == Some(nodes::terrain_group()),
                "source cell node does not contain a terrain group"
            );
            let (grid_x, grid_y) = nodes::parse_source_cell(name)
                .ok_or_else(|| color_eyre::eyre::eyre!("invalid source cell node name {name}"))?;
            let quadrants = group["children"]
                .as_array()
                .ok_or_else(|| color_eyre::eyre::eyre!("terrain group has no quadrant nodes"))?;
            color_eyre::eyre::ensure!(
                quadrants.len() == 4,
                "terrain group must contain four quadrant nodes"
            );
            for (quadrant, child) in quadrants.iter().enumerate() {
                let child_index = json_usize(child, "terrain quadrant node")?;
                let (short_name, full_name, _, _) = QUADRANTS[quadrant];
                color_eyre::eyre::ensure!(
                    nodes[child_index]["name"].as_str()
                        == Some(format!("terrain_quadrant_{short_name}").as_str())
                        && nodes[child_index]["extras"]["quadrant"].as_str() == Some(full_name),
                    "terrain quadrant {quadrant} has an invalid name"
                );
                color_eyre::eyre::ensure!(
                    nodes[child_index].get("mesh").is_some(),
                    "terrain quadrant {quadrant} has no mesh"
                );
                let mesh_index = json_usize(&nodes[child_index]["mesh"], "quadrant mesh")?;
                color_eyre::eyre::ensure!(
                    meshes[mesh_index]["name"].as_str()
                        == Some(
                            format!("terrain_{grid_x}_{grid_y}_quadrant_{short_name}").as_str()
                        ),
                    "source cell ({grid_x}, {grid_y}) quadrant {quadrant} maps to the wrong mesh"
                );
                let primitive = &meshes[mesh_index]["primitives"][0];
                let position_index = json_usize(
                    &primitive["attributes"]["POSITION"],
                    "quadrant POSITION accessor",
                )?;
                let index_index = json_usize(&primitive["indices"], "quadrant index accessor")?;
                color_eyre::eyre::ensure!(
                    json_usize(&accessors[position_index]["count"], "quadrant vertex count")? == 9
                        && json_usize(&accessors[index_index]["count"], "quadrant index count")?
                            == 24,
                    "terrain quadrant {quadrant} must contain a 3x3 grid"
                );
            }
        }
    }
    color_eyre::eyre::ensure!(!source_cell_ids.is_empty(), "GLB has no source cell nodes");
    let scenes = json["scenes"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB scenes are missing"))?;
    let scene_index = json_usize(&json["scene"], "default scene")?;
    let scene = scenes
        .get(scene_index)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB default scene is missing"))?;
    for node in scene["nodes"]
        .as_array()
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB default scene has no nodes"))?
    {
        color_eyre::eyre::ensure!(
            json_usize(node, "scene node")? < nodes.len(),
            "GLB scene references a missing node"
        );
    }
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB integer offset overflows"))?;
    let field = bytes
        .get(offset..end)
        .ok_or_else(|| color_eyre::eyre::eyre!("GLB integer is truncated"))?;
    Ok(u32::from_le_bytes(field.try_into()?))
}

fn read_f32(bytes: &[u8], offset: usize) -> Result<f32> {
    Ok(f32::from_bits(read_u32(bytes, offset)?))
}

fn json_usize(value: &serde_json::Value, description: &str) -> Result<usize> {
    let value = value
        .as_u64()
        .ok_or_else(|| color_eyre::eyre::eyre!("{description} must be a nonnegative integer"))?;
    usize::try_from(value)
        .map_err(|_| color_eyre::eyre::eyre!("{description} does not fit in memory address space"))
}

/// Writes compiled chunks into staging: GLB payloads at their canonical
/// paths plus the `lod_chunks` / `lod_chunks_spatial` / `lod_build` rows.
/// `build_identity` is the single identity shared by DB, manifest, and
/// chunks (BUILD-01/02); every payload hash is recorded before the rows
/// that reference it are written.
pub fn publish_chunks(
    connection: &Connection,
    staging_root: &Path,
    build_identity: Option<&str>,
    chunks: &[TerrainChunk],
) -> Result<()> {
    let tx = connection.unchecked_transaction()?;
    // The pipeline compiles every world before the final identity exists, so
    // it passes `None` per world and writes the single `lod_build` row once
    // at the end. Tests pass `Some` to pin their own identity.
    if let Some(build_identity) = build_identity {
        tx.execute(
            "INSERT OR REPLACE INTO lod_build(id, build_identity) VALUES (1, ?1)",
            params![build_identity],
        )?;
    }
    for chunk in chunks {
        let relative = chunk_payload_path(chunk.key);
        let path = staging_root.join(&relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &chunk.glb)?;
        // Validate what was written, not what was held: a short write or a
        // corrupted buffer must fail the build, not ship a bad payload with
        // a hash of the good bytes.
        let written = std::fs::read(&path)?;
        validate_terrain_glb(&written)
            .wrap_err_with(|| format!("invalid terrain chunk GLB {relative}"))?;
        let content_hash = crate::cache::hash_bytes(&written);
        let source_cells = chunk
            .cells
            .iter()
            .map(|cell| format!("{},{}", cell.grid_x, cell.grid_y))
            .collect::<Vec<_>>()
            .join(";");
        tx.execute(
            "INSERT OR REPLACE INTO lod_chunks(worldspace_id, tier, anchor_x, anchor_y, payload_path, content_hash, \
             bounds_min_x, bounds_min_y, bounds_min_z, bounds_max_x, bounds_max_y, bounds_max_z, source_cells) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                chunk.key.worldspace_id,
                chunk.key.tier.side_cells(),
                chunk.key.anchor.x,
                chunk.key.anchor.y,
                relative,
                content_hash,
                chunk.bounds_min[0],
                chunk.bounds_min[1],
                chunk.bounds_min[2],
                chunk.bounds_max[0],
                chunk.bounds_max[1],
                chunk.bounds_max[2],
                source_cells,
            ],
        )?;
        // Spatial index rows live in the rtree's own id space: the chunk key
        // columns ride along as payload so a range query returns keys, not
        // rowids the caller must join back. R-tree ids are not the chunk key,
        // so remove every prior row for this key before assigning a fresh id.
        tx.execute(
            "DELETE FROM lod_chunks_spatial WHERE worldspace_id = ?1 AND tier = ?2 AND anchor_x = ?3 AND anchor_y = ?4",
            params![
                chunk.key.worldspace_id,
                chunk.key.tier.side_cells(),
                chunk.key.anchor.x,
                chunk.key.anchor.y,
            ],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO lod_chunks_spatial(id, minX, maxX, minY, maxY, worldspace_id, tier, anchor_x, anchor_y) \
             VALUES ((SELECT COALESCE(MAX(id), 0) + 1 FROM lod_chunks_spatial), ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                chunk.bounds_min[0],
                chunk.bounds_max[0],
                chunk.bounds_min[1],
                chunk.bounds_max[1],
                chunk.key.worldspace_id,
                chunk.key.tier.side_cells(),
                chunk.key.anchor.x,
                chunk.key.anchor.y,
            ],
        ).wrap_err("failed to index chunk bounds")?;
    }
    tx.commit()?;
    Ok(())
}

/// Reads one worldspace's exterior cell grid from the converted database:
/// `(grid_x, grid_y, cell_id)` for every exterior cell with terrain.
pub fn exterior_terrain_cells(
    connection: &Connection,
    worldspace_id: u32,
) -> Result<Vec<(i32, i32, u32)>> {
    let mut statement = connection.prepare(
        "SELECT c.grid_x, c.grid_y, c.id FROM cells c JOIN land l ON l.cell_id = c.id \
         WHERE c.worldspace_id = ?1 AND c.grid_x IS NOT NULL AND c.grid_y IS NOT NULL \
         ORDER BY c.grid_y, c.grid_x",
    )?;
    let rows = statement.query_map([worldspace_id], |row| {
        Ok((
            row.get::<_, i32>(0)?,
            row.get::<_, i32>(1)?,
            row.get::<_, u32>(2)?,
        ))
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .wrap_err("failed to list exterior terrain cells")
}

/// Reads cached height fields for `cell_ids` from a cell-cache file, without
/// the engine's mmap reader: the compiler runs offline, before publication.
pub fn read_cached_heights(
    cache_path: &Path,
    cell_ids: &HashMap<u32, (i32, i32)>,
) -> Result<Vec<TerrainCellInput>> {
    let bytes = std::fs::read(cache_path)
        .wrap_err_with(|| format!("failed to read {}", cache_path.display()))?;
    let archived = rkyv::access::<shared::ArchivedCellCache, rkyv::rancor::Error>(&bytes)
        .wrap_err("invalid cell cache")?;
    color_eyre::eyre::ensure!(
        archived.version == shared::CELL_CACHE_VERSION,
        "cell cache version {} is unsupported; expected {}",
        archived.version,
        shared::CELL_CACHE_VERSION
    );
    let mut cells = Vec::new();
    for cell in archived.cells.iter() {
        let cell_id: u32 = cell.cell_id.into();
        let Some((grid_x, grid_y)) = cell_ids.get(&cell_id) else {
            continue;
        };
        cells.push(TerrainCellInput {
            cell_id,
            grid_x: *grid_x,
            grid_y: *grid_y,
            heights: cell.heights.iter().copied().map(Into::into).collect(),
            vertex_colors: cell.vertex_colors.iter().copied().collect(),
        });
    }
    color_eyre::eyre::ensure!(
        cells.len() == cell_ids.len(),
        "cell cache holds {} of {} requested cells",
        cells.len(),
        cell_ids.len()
    );
    Ok(cells)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_cell(grid_x: i32, grid_y: i32, height: f32) -> TerrainCellInput {
        TerrainCellInput {
            cell_id: ((grid_y as u32) << 16) | grid_x as u32,
            grid_x,
            grid_y,
            heights: vec![height; 33 * 33],
            vertex_colors: Vec::new(),
        }
    }

    #[test]
    fn compiles_one_chunk_per_tier_for_a_single_cell() {
        let chunks =
            compile_world_terrain(0x3c, LodOrigin::new(0, 0), &[flat_cell(1, 2, 10.0)]).unwrap();
        assert_eq!(chunks.len(), 3);
        let tiers: Vec<_> = chunks.iter().map(|chunk| chunk.key.tier).collect();
        assert_eq!(tiers, vec![LodTier::Tier4, LodTier::Tier8, LodTier::Tier16]);
        for chunk in &chunks {
            assert_eq!(chunk.cells.len(), 1);
            assert_eq!((chunk.bounds_min[2], chunk.bounds_max[2]), (10.0, 10.0));
            assert_eq!(&chunk.glb[..4], b"glTF");
        }
    }

    #[test]
    fn adjacent_cells_share_a_tier4_chunk() {
        let cells = vec![flat_cell(0, 0, 0.0), flat_cell(1, 0, 0.0)];
        let chunks = compile_world_terrain(0x3c, LodOrigin::new(0, 0), &cells).unwrap();
        let tier4: Vec<_> = chunks
            .iter()
            .filter(|chunk| chunk.key.tier == LodTier::Tier4)
            .collect();
        assert_eq!(tier4.len(), 1);
        assert_eq!(tier4[0].cells.len(), 2);
    }

    #[test]
    fn chunk_glb_has_named_source_cell_nodes_and_valid_structure() {
        let mut first = flat_cell(0, 0, 0.0);
        first.cell_id = 0x1234;
        first.vertex_colors = (0..33 * 33)
            .flat_map(|index| {
                if index == 0 {
                    [64, 128, 192]
                } else {
                    [
                        (index % 251) as u8,
                        ((index * 3) % 251) as u8,
                        ((index * 7) % 251) as u8,
                    ]
                }
            })
            .collect();
        let mut second = flat_cell(1, 0, 5.0);
        second.cell_id = 0x5678;
        let cells = vec![first, second];
        let chunks = compile_world_terrain(0x3c, LodOrigin::new(0, 0), &cells).unwrap();
        let chunk = chunks
            .iter()
            .find(|chunk| chunk.key.tier == LodTier::Tier4)
            .unwrap();
        assert_eq!(&chunk.glb[..4], b"glTF");
        let total = u32::from_le_bytes(chunk.glb[8..12].try_into().unwrap()) as usize;
        assert_eq!(total, chunk.glb.len());
        let json_len = u32::from_le_bytes(chunk.glb[12..16].try_into().unwrap()) as usize;
        assert_eq!(&chunk.glb[16..20], b"JSON");
        let json: serde_json::Value =
            serde_json::from_slice(&chunk.glb[20..20 + json_len]).unwrap();
        let nodes = json["nodes"].as_array().unwrap();
        // Root + source cell, terrain group, and four quadrant nodes per cell.
        assert_eq!(nodes.len(), 13);
        assert_eq!(nodes[1]["name"], "cell_0_0");
        assert_eq!(nodes[2]["name"], "terrain");
        assert_eq!(nodes[1]["extras"]["cell_id"], 0x1234);
        assert_eq!(nodes[3]["name"], "terrain_quadrant_sw");
        assert_eq!(nodes[3]["extras"]["quadrant"], "southwest");
        assert_eq!(nodes[4]["name"], "terrain_quadrant_se");
        assert_eq!(nodes[4]["extras"]["quadrant"], "southeast");
        assert_eq!(nodes[5]["name"], "terrain_quadrant_nw");
        assert_eq!(nodes[5]["extras"]["quadrant"], "northwest");
        assert_eq!(nodes[6]["name"], "terrain_quadrant_ne");
        assert_eq!(nodes[6]["extras"]["quadrant"], "northeast");
        assert_eq!(nodes[7]["name"], "cell_1_0");
        assert_eq!(nodes[7]["extras"]["cell_id"], 0x5678);
        assert_eq!(nodes[1]["children"][0], 2);
        assert_eq!(nodes[2]["children"], serde_json::json!([3, 4, 5, 6]));
        assert_eq!(nodes[7]["children"][0], 8);
        assert_eq!(nodes[8]["children"], serde_json::json!([9, 10, 11, 12]));
        assert_eq!(nodes[3]["mesh"], 0);
        assert_eq!(nodes[12]["mesh"], 7);
        // Five accessors per quadrant: position, normal, UV, color, indices.
        let accessors = json["accessors"].as_array().unwrap();
        assert_eq!(accessors.len(), 40);
        assert_eq!(accessors[0]["min"][2], 0.0);
        assert_eq!(accessors[0]["max"][2], 0.0);
        assert_eq!(accessors[20]["min"][2], 5.0);
        assert_eq!(accessors[20]["max"][2], 5.0);
        for mesh in json["meshes"].as_array().unwrap() {
            let attributes = &mesh["primitives"][0]["attributes"];
            for semantic in ["POSITION", "NORMAL", "TEXCOORD_0", "COLOR_0"] {
                assert!(attributes.get(semantic).is_some());
                assert_eq!(
                    accessors[attributes[semantic].as_u64().unwrap() as usize]["count"],
                    9
                );
            }
        }
        assert_eq!(
            json["materials"][0],
            serde_json::json!({
                "name": "lod_terrain",
                "pbrMetallicRoughness": {
                    "baseColorFactor": [1.0, 1.0, 1.0, 1.0],
                    "roughnessFactor": 0.92
                },
                "doubleSided": true
            })
        );
        validate_terrain_glb(&chunk.glb).unwrap();
        // Each quadrant's indices address only its nine-vertex accessor.
        let views = json["bufferViews"].as_array().unwrap();
        let index_view_offset = views[4]["byteOffset"].as_u64().unwrap() as usize;
        let bin_start = 20 + json_len + 8;
        for quadrant_index in 0..8 {
            let index_accessor = &accessors[quadrant_index * 5 + 4];
            assert_eq!(index_accessor["count"], 24);
            let start = index_view_offset + index_accessor["byteOffset"].as_u64().unwrap() as usize;
            let count = index_accessor["count"].as_u64().unwrap() as usize;
            for index in 0..count {
                let offset = bin_start + start + index * 4;
                let value = u32::from_le_bytes(chunk.glb[offset..offset + 4].try_into().unwrap());
                assert!(
                    value < 9,
                    "quadrant {quadrant_index} index {value} is out of range"
                );
            }
        }
        let first_indices = [0usize, 1, 2, 3, 4, 5].map(|index| {
            let offset = bin_start + index_view_offset + index * 4;
            u32::from_le_bytes(chunk.glb[offset..offset + 4].try_into().unwrap())
        });
        assert_eq!(first_indices, [0, 1, 3, 1, 4, 3]);
        let uv_view_offset = views[2]["byteOffset"].as_u64().unwrap() as usize;
        let uv = |vertex: usize, axis: usize| {
            let offset = bin_start + uv_view_offset + vertex * 8 + axis * 4;
            f32::from_le_bytes(chunk.glb[offset..offset + 4].try_into().unwrap())
        };
        assert_eq!([uv(0, 0), uv(0, 1)], [0.0, 0.0]);
        assert_eq!([uv(2, 0), uv(2, 1)], [0.5, 0.0]);
        assert_eq!([uv(6, 0), uv(6, 1)], [0.0, 0.5]);
        assert_eq!([uv(3 * 9 + 8, 0), uv(3 * 9 + 8, 1)], [1.0, 1.0]);
        let color_view_offset = views[3]["byteOffset"].as_u64().unwrap() as usize;
        let color = |quadrant: usize, vertex: usize, channel: usize| {
            let offset = bin_start + color_view_offset + (quadrant * 9 + vertex) * 16 + channel * 4;
            f32::from_le_bytes(chunk.glb[offset..offset + 4].try_into().unwrap())
        };
        assert!((color(0, 0, 0) - 64.0 / 255.0).abs() < f32::EPSILON);
        assert!((color(0, 0, 1) - 128.0 / 255.0).abs() < f32::EPSILON);
        assert!((color(0, 0, 2) - 192.0 / 255.0).abs() < f32::EPSILON);
        assert_eq!(color(0, 0, 3), 1.0);
        let sampled = 16 * 33 + 16;
        assert!((color(0, 8, 0) - (sampled % 251) as f32 / 255.0).abs() < f32::EPSILON);
        assert!((color(0, 8, 1) - ((sampled * 3) % 251) as f32 / 255.0).abs() < f32::EPSILON);
        assert!((color(0, 8, 2) - ((sampled * 7) % 251) as f32 / 255.0).abs() < f32::EPSILON);
        // The second source cell has no VCLR, so its color defaults to white.
        assert_eq!(
            [
                color(4, 0, 0),
                color(4, 0, 1),
                color(4, 0, 2),
                color(4, 0, 3)
            ],
            [1.0; 4]
        );
        // Buffer views cover exactly the binary chunk.
        let bin_len =
            u32::from_le_bytes(chunk.glb[20 + json_len..24 + json_len].try_into().unwrap())
                as usize;
        assert_eq!(&chunk.glb[24 + json_len..28 + json_len], b"BIN\x00");
        assert_eq!(20 + json_len + 8 + bin_len, chunk.glb.len());
        let buffers = json["buffers"].as_array().unwrap();
        assert_eq!(buffers[0]["byteLength"], bin_len);
    }

    #[test]
    fn rejects_missing_terrain_handoff_attributes() {
        let mut corrupted =
            compile_world_terrain(0x3c, LodOrigin::new(0, 0), &[flat_cell(0, 0, 0.0)])
                .unwrap()
                .remove(0)
                .glb;
        let key = b"COLOR_0";
        let start = corrupted
            .windows(key.len())
            .position(|window| window == key)
            .unwrap();
        corrupted[start + key.len() - 1] = b'X';
        assert!(validate_terrain_glb(&corrupted).is_err());
    }

    #[test]
    fn rejects_quadrant_mapping_and_material_changes() {
        let original = compile_world_terrain(0x3c, LodOrigin::new(0, 0), &[flat_cell(0, 0, 0.0)])
            .unwrap()
            .remove(0)
            .glb;

        let mut bad_quadrant = original.clone();
        let node_name = b"terrain_quadrant_sw";
        let node_start = bad_quadrant
            .windows(node_name.len())
            .position(|window| window == node_name)
            .unwrap();
        bad_quadrant[node_start + node_name.len() - 1] = b'X';
        assert!(validate_terrain_glb(&bad_quadrant).is_err());

        let mut bad_material = original;
        let material_value = b"roughnessFactor\":0.92";
        let material_start = bad_material
            .windows(material_value.len())
            .position(|window| window == material_value)
            .unwrap();
        bad_material[material_start + material_value.len() - 1] = b'3';
        assert!(validate_terrain_glb(&bad_material).is_err());
    }

    #[test]
    fn rejects_out_of_range_uv_and_vertex_color_values() {
        let original = compile_world_terrain(0x3c, LodOrigin::new(0, 0), &[flat_cell(0, 0, 0.0)])
            .unwrap()
            .remove(0)
            .glb;
        let json_len = read_u32(&original, 12).unwrap() as usize;
        let json: serde_json::Value = serde_json::from_slice(&original[20..20 + json_len]).unwrap();
        let bin_start = 20 + json_len + 8;
        let corrupt_attribute = |semantic: &str| {
            let accessor_index = json["meshes"][0]["primitives"][0]["attributes"][semantic]
                .as_u64()
                .unwrap() as usize;
            let accessor = &json["accessors"][accessor_index];
            let view_index = accessor["bufferView"].as_u64().unwrap() as usize;
            let view_offset = json["bufferViews"][view_index]["byteOffset"]
                .as_u64()
                .unwrap() as usize;
            let accessor_offset = accessor["byteOffset"].as_u64().unwrap_or(0) as usize;
            let mut glb = original.clone();
            let value_start = bin_start + view_offset + accessor_offset;
            glb[value_start..value_start + 4].copy_from_slice(&1.5f32.to_le_bytes());
            glb
        };
        assert!(validate_terrain_glb(&corrupt_attribute("TEXCOORD_0")).is_err());
        assert!(validate_terrain_glb(&corrupt_attribute("COLOR_0")).is_err());
    }

    #[test]
    fn rejects_a_triangle_index_outside_its_source_cell_accessor() {
        let chunks = compile_world_terrain(
            0x3c,
            LodOrigin::new(0, 0),
            &[flat_cell(0, 0, 0.0), flat_cell(1, 0, 0.0)],
        )
        .unwrap();
        let chunk = chunks
            .iter()
            .find(|chunk| chunk.key.tier == LodTier::Tier4)
            .unwrap();
        let mut corrupted = chunk.glb.clone();
        let json_len = read_u32(&corrupted, 12).unwrap() as usize;
        let json: serde_json::Value =
            serde_json::from_slice(&corrupted[20..20 + json_len]).unwrap();
        let view_offset = json["bufferViews"][4]["byteOffset"].as_u64().unwrap() as usize;
        let accessor_offset = json["accessors"][4]["byteOffset"].as_u64().unwrap() as usize;
        let bin_start = 20 + json_len + 8;
        corrupted[bin_start + view_offset + accessor_offset
            ..bin_start + view_offset + accessor_offset + 4]
            .copy_from_slice(&25u32.to_le_bytes());
        assert!(validate_terrain_glb(&corrupted).is_err());
    }

    #[test]
    fn rejects_nonfinite_heights() {
        let mut cell = flat_cell(0, 0, 0.0);
        cell.heights[100] = f32::NAN;
        assert!(compile_world_terrain(0x3c, LodOrigin::new(0, 0), &[cell]).is_err());
    }

    #[test]
    fn chunk_bounds_are_world_space_while_glb_positions_are_chunk_local() {
        let origin = LodOrigin::new(8, -8);
        let chunks = compile_world_terrain(0x3c, origin, &[flat_cell(11, -5, 7.0)]).unwrap();
        let chunk = chunks
            .iter()
            .find(|chunk| chunk.key.tier == LodTier::Tier4)
            .unwrap();

        assert_eq!(chunk.bounds_min, [11.0 * 4096.0, -5.0 * 4096.0, 7.0]);
        assert_eq!(chunk.bounds_max, [12.0 * 4096.0, -4.0 * 4096.0, 7.0]);

        let json_len = u32::from_le_bytes(chunk.glb[12..16].try_into().unwrap()) as usize;
        let json: serde_json::Value =
            serde_json::from_slice(&chunk.glb[20..20 + json_len]).unwrap();
        let accessor = &json["accessors"][0];
        assert_eq!(accessor["min"][0], 3.0 * 4096.0);
        assert_eq!(accessor["min"][1], 3.0 * 4096.0);
    }

    #[test]
    fn publishing_replaces_all_old_spatial_rows_for_a_chunk_key() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE lod_chunks (
                    worldspace_id INTEGER NOT NULL, tier INTEGER NOT NULL,
                    anchor_x INTEGER NOT NULL, anchor_y INTEGER NOT NULL,
                    payload_path TEXT NOT NULL, content_hash TEXT NOT NULL,
                    bounds_min_x REAL NOT NULL, bounds_min_y REAL NOT NULL, bounds_min_z REAL NOT NULL,
                    bounds_max_x REAL NOT NULL, bounds_max_y REAL NOT NULL, bounds_max_z REAL NOT NULL,
                    source_cells TEXT NOT NULL DEFAULT '',
                    PRIMARY KEY (worldspace_id, tier, anchor_x, anchor_y)
                );
                CREATE VIRTUAL TABLE lod_chunks_spatial USING rtree(
                    id, minX, maxX, minY, maxY, +worldspace_id, +tier, +anchor_x, +anchor_y
                );
                INSERT INTO lod_chunks_spatial VALUES (1, -1, 1, -1, 1, 60, 4, 0, 0);
                INSERT INTO lod_chunks_spatial VALUES (2, -2, 2, -2, 2, 60, 4, 0, 0);",
            )
            .unwrap();
        let chunks =
            compile_world_terrain(60, LodOrigin::new(8, -8), &[flat_cell(11, -5, 7.0)]).unwrap();
        let chunk = chunks
            .iter()
            .find(|chunk| chunk.key.tier == LodTier::Tier4)
            .unwrap();
        let directory = tempfile::tempdir().unwrap();

        publish_chunks(
            &connection,
            directory.path(),
            None,
            std::slice::from_ref(chunk),
        )
        .unwrap();
        publish_chunks(
            &connection,
            directory.path(),
            None,
            std::slice::from_ref(chunk),
        )
        .unwrap();

        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM lod_chunks_spatial WHERE worldspace_id = 60 AND tier = 4 AND anchor_x = 0 AND anchor_y = 0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let bounds: (f64, f64, f64, f64) = connection
            .query_row(
                "SELECT minX, maxX, minY, maxY FROM lod_chunks_spatial WHERE worldspace_id = 60 AND tier = 4 AND anchor_x = 0 AND anchor_y = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            bounds,
            (11.0 * 4096.0, 12.0 * 4096.0, -5.0 * 4096.0, -4.0 * 4096.0)
        );
    }
}
