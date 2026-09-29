use super::{
    RenderOrigin, StreamingCommitBudget, StreamingMetrics, TerrainCoverage, TerrainSurfaceReady,
};
use crate::{
    config::EngineConfig,
    profiling::ProfilingState,
    world::{
        components::{CELL_SIZE, StreamingCamera},
        database::{LodChunkMetadata, LodChunkQuery, WorldDatabase},
    },
};
use bevy::{
    asset::{LoadState, RecursiveDependencyLoadState},
    gltf::GltfAssetLabel,
    mesh::{Indices, PrimitiveTopology, VertexAttributeValues},
    prelude::*,
    tasks::{IoTaskPool, Task, block_on},
    world_serialization::{WorldAsset, WorldAssetRoot, WorldInstanceReady},
};
use sha2::{Digest, Sha256};
use shared::lod::{ChunkKey, LodOrigin, LodTier, nodes};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    time::Instant,
};

const LOD_MAX_DISTANCE_CELLS: i32 = 16;
const LOD_UNLOAD_MARGIN_CELLS: i32 = 2;
const LOD_RESPONSE_SCAN_LIMIT: usize = 32;

pub(crate) const LOD_CAMERA_FAR: f32 =
    CELL_SIZE * (LOD_MAX_DISTANCE_CELLS as f32 + 0.5) * std::f32::consts::SQRT_2;

#[derive(Resource, Default)]
pub(super) struct LodStreaming {
    generation: u64,
    center: Option<IVec2>,
    requested_queries: HashSet<LodTier>,
    pending_queries: HashSet<LodTier>,
    chunks: HashMap<ChunkKey, LodChunkStatus>,
    pending_chunks: VecDeque<(u64, LodChunkMetadata)>,
    build_identity: Option<String>,
}

#[derive(Clone, Copy)]
enum LodChunkStatus {
    Loading {
        root: Entity,
        generation: u64,
        origin: LodOrigin,
    },
    Ready {
        root: Entity,
        origin: LodOrigin,
    },
    Failed {
        origin: LodOrigin,
    },
}

impl LodChunkStatus {
    fn origin(self) -> LodOrigin {
        match self {
            Self::Loading { origin, .. } | Self::Ready { origin, .. } | Self::Failed { origin } => {
                origin
            }
        }
    }

    fn root(self) -> Option<Entity> {
        match self {
            Self::Loading { root, .. } | Self::Ready { root, .. } => Some(root),
            Self::Failed { .. } => None,
        }
    }
}

#[derive(Component)]
pub(super) struct LodChunkRoot {
    key: ChunkKey,
    generation: u64,
    origin: LodOrigin,
}

#[derive(Component)]
pub(super) struct LodChunkGridOrigin {
    pub(super) grid_x: i64,
    pub(super) grid_y: i64,
}

#[derive(Component)]
pub(super) struct PendingLodChunk {
    metadata: LodChunkMetadata,
    asset: Handle<WorldAsset>,
    scene_spawned: bool,
    hash_task: Option<Task<Result<(), String>>>,
    hash_verified: bool,
    started: Instant,
}

pub(super) fn mark_lod_world_instance_ready(
    ready: On<WorldInstanceReady>,
    mut pending: Query<&mut PendingLodChunk>,
) {
    if let Ok(mut pending) = pending.get_mut(ready.entity) {
        pending.scene_spawned = true;
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn plan_lod_chunks(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    origin: Res<RenderOrigin>,
    camera: Query<&Transform, With<StreamingCamera>>,
    mut streaming: ResMut<LodStreaming>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    if config.stream_radius < 0 {
        if streaming.center.take().is_some() {
            streaming.generation = streaming.generation.wrapping_add(1);
        }
        streaming.requested_queries.clear();
        streaming.pending_queries.clear();
        streaming.pending_chunks.clear();
        streaming.chunks.retain(|key, status| {
            if let Some(root) = status.root() {
                commands.entity(root).try_despawn();
            }
            profiler.event(format!("{key:?}"), "unloaded", None);
            false
        });
        update_counts(&streaming, &mut metrics, &mut profiler);
        profiler.record_elapsed("lod/plan", started);
        return;
    }

    let center = streaming_center(config.acceptance_screenshot.is_some(), origin.0, camera);
    if streaming.center != Some(center) {
        streaming.generation = streaming.generation.wrapping_add(1);
        streaming.requested_queries.clear();
        streaming.pending_queries.clear();
        streaming.pending_chunks.clear();
        streaming.center = Some(center);
    }
    let generation = streaming.generation;
    let query_radius = LOD_MAX_DISTANCE_CELLS + LOD_UNLOAD_MARGIN_CELLS;
    for tier in LodTier::ALL {
        if streaming.requested_queries.contains(&tier) {
            continue;
        }
        let query = query_for(config.worldspace_id, tier, center, query_radius);
        match database.request_lod_chunks(generation, query) {
            Ok(()) => {
                streaming.requested_queries.insert(tier);
                streaming.pending_queries.insert(tier);
                metrics.lod_queries_submitted = metrics.lod_queries_submitted.saturating_add(1);
                profiler.increment("lod/queries_submitted", 1);
            }
            Err(error) => {
                metrics.lod_query_submission_failures =
                    metrics.lod_query_submission_failures.saturating_add(1);
                debug!(?tier, %error, "terrain LOD query will be retried");
            }
        }
    }
    metrics.pending_lod_queries = streaming.pending_queries.len();

    let center64 = (i64::from(center.x), i64::from(center.y));
    streaming.chunks.retain(|key, status| {
        let keep = chunk_within_radius(*key, status.origin(), center64, query_unload_radius());
        if !keep {
            if let Some(root) = status.root() {
                commands.entity(root).try_despawn();
            }
            profiler.event(format!("{key:?}"), "unloaded", None);
        }
        keep
    });
    let generation = streaming.generation;
    streaming
        .pending_chunks
        .retain(|(queued_generation, _)| *queued_generation == generation);
    update_counts(&streaming, &mut metrics, &mut profiler);
    profiler.record_elapsed("lod/plan", started);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_lod_chunks(
    mut commands: Commands,
    config: Res<EngineConfig>,
    database: Res<WorldDatabase>,
    asset_server: Res<AssetServer>,
    origin: Res<RenderOrigin>,
    mut camera_projection: Query<&mut Projection, With<StreamingCamera>>,
    mut streaming: ResMut<LodStreaming>,
    mut budget: ResMut<StreamingCommitBudget>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    for _ in 0..LOD_RESPONSE_SCAN_LIMIT {
        let Some(response) = database.try_lod_response() else {
            break;
        };
        if response.generation == streaming.generation {
            streaming.pending_queries.remove(&response.query.tier);
        }
        metrics.lod_query_responses = metrics.lod_query_responses.saturating_add(1);
        metrics.total_lod_query_micros = metrics
            .total_lod_query_micros
            .saturating_add(response.query_micros);
        metrics.max_lod_query_micros = metrics.max_lod_query_micros.max(response.query_micros);
        profiler.record_micros("lod/db_queue_wait", response.queue_wait_micros);
        profiler.record_micros("lod/db_query", response.query_micros);
        profiler.record_micros("lod/db_request_total", response.total_request_micros);

        if !is_current_query(&streaming, response.generation) {
            metrics.stale_lod_query_responses = metrics.stale_lod_query_responses.saturating_add(1);
            profiler.increment("lod/stale_query_responses", 1);
            continue;
        }
        match response.result {
            Err(error) => {
                metrics.failed_lod_queries = metrics.failed_lod_queries.saturating_add(1);
                warn!(?response.query.tier, %error, "terrain LOD query failed");
            }
            Ok(chunks) => {
                if !chunks.is_empty()
                    && let Ok(mut projection) = camera_projection.single_mut()
                    && let Projection::Perspective(perspective) = &mut *projection
                {
                    perspective.far = perspective.far.max(LOD_CAMERA_FAR);
                }
                streaming.pending_chunks.extend(
                    chunks
                        .into_iter()
                        .map(|metadata| (response.generation, metadata)),
                );
            }
        }
    }

    while budget.remaining > 0 {
        let Some((generation, metadata)) = streaming.pending_chunks.pop_front() else {
            break;
        };
        if generation != streaming.generation || streaming.center.is_none() {
            metrics.stale_lod_query_responses = metrics.stale_lod_query_responses.saturating_add(1);
            continue;
        }
        let center = streaming.center.unwrap();
        if !chunk_within_radius(
            metadata.key,
            metadata.origin,
            (i64::from(center.x), i64::from(center.y)),
            query_unload_radius(),
        ) || streaming.chunks.contains_key(&metadata.key)
        {
            continue;
        }
        if let Some(identity) = &streaming.build_identity {
            if identity != &metadata.build_identity {
                metrics.failed_lod_chunks = metrics.failed_lod_chunks.saturating_add(1);
                error!(
                    ?metadata.key,
                    expected = identity,
                    actual = metadata.build_identity,
                    "terrain LOD chunk belongs to a different asset build"
                );
                continue;
            }
        } else {
            streaming.build_identity = Some(metadata.build_identity.clone());
        }

        let key = metadata.key;
        let chunk_origin = metadata.origin;
        let min_grid = chunk_min_grid(key, metadata.origin);
        let scene_path = GltfAssetLabel::Scene(0).from_asset(metadata.payload_path.clone());
        let asset = asset_server.load(scene_path);
        let expected_hash = metadata.content_hash.clone();
        let hash_path = config.assets_dir.join(&metadata.payload_path);
        let hash_task =
            IoTaskPool::get().spawn(async move { verify_payload_hash(hash_path, expected_hash) });
        let local_grid_x = min_grid.0 - i64::from(origin.0.x);
        let local_grid_y = min_grid.1 - i64::from(origin.0.y);
        let root = commands
            .spawn((
                Name::new(format!(
                    "LOD {:?} chunk {},{}",
                    key.tier, key.anchor.x, key.anchor.y
                )),
                LodChunkRoot {
                    key,
                    generation,
                    origin: metadata.origin,
                },
                LodChunkGridOrigin {
                    grid_x: min_grid.0,
                    grid_y: min_grid.1,
                },
                WorldAssetRoot(asset.clone()),
                Transform::from_xyz(
                    local_grid_x as f32 * CELL_SIZE,
                    0.0,
                    -(local_grid_y as f32) * CELL_SIZE,
                ),
                Visibility::Hidden,
                PendingLodChunk {
                    metadata,
                    asset,
                    scene_spawned: false,
                    hash_task: Some(hash_task),
                    hash_verified: false,
                    started: Instant::now(),
                },
            ))
            .id();
        streaming.chunks.insert(
            key,
            LodChunkStatus::Loading {
                root,
                generation,
                origin: chunk_origin,
            },
        );
        budget.remaining -= 1;
        budget.commits = budget.commits.saturating_add(1);
        metrics.lod_chunks_requested = metrics.lod_chunks_requested.saturating_add(1);
        profiler.increment("lod/chunks_requested", 1);
        profiler.event(format!("{key:?}"), "requested", None);
    }
    update_counts(&streaming, &mut metrics, &mut profiler);
    profiler.record_elapsed("lod/collect", started);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn track_lod_readiness(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    world_assets: Res<Assets<WorldAsset>>,
    meshes: Res<Assets<Mesh>>,
    children: Query<&Children>,
    names: Query<&Name>,
    mesh_handles: Query<&Mesh3d>,
    mut pending_chunks: Query<(Entity, &LodChunkRoot, &mut PendingLodChunk)>,
    mut streaming: ResMut<LodStreaming>,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    for (entity, root, mut pending) in &mut pending_chunks {
        let load_failure = asset_server.get_load_states(pending.asset.id()).and_then(
            |(load, _, recursive)| match (load, recursive) {
                (LoadState::Failed(error), _) => Some(error),
                (_, RecursiveDependencyLoadState::Failed(error)) => Some(error),
                _ => None,
            },
        );
        if let Some(error) = load_failure {
            fail_chunk(
                entity,
                root,
                error.to_string(),
                &mut commands,
                &mut streaming,
                &mut metrics,
                &mut profiler,
            );
            continue;
        }

        if !pending.hash_verified {
            let Some(task) = pending.hash_task.as_ref() else {
                continue;
            };
            if !task.is_finished() {
                continue;
            }
            let task = pending.hash_task.take().expect("finished hash task exists");
            match block_on(task) {
                Ok(()) => pending.hash_verified = true,
                Err(reason) => {
                    fail_chunk(
                        entity,
                        root,
                        reason,
                        &mut commands,
                        &mut streaming,
                        &mut metrics,
                        &mut profiler,
                    );
                    continue;
                }
            }
        }

        if !pending.scene_spawned
            || !asset_server.is_loaded_with_dependencies(pending.asset.id())
            || world_assets.get(&pending.asset).is_none()
        {
            continue;
        }

        let patches = match validate_lod_scene(
            entity,
            &pending.metadata,
            &children,
            &names,
            &mesh_handles,
            &meshes,
        ) {
            Ok(patches) => patches,
            Err(reason) => {
                fail_chunk(
                    entity,
                    root,
                    reason,
                    &mut commands,
                    &mut streaming,
                    &mut metrics,
                    &mut profiler,
                );
                continue;
            }
        };
        for (patch, coverage) in &patches {
            commands
                .entity(*patch)
                .insert((*coverage, TerrainSurfaceReady, Visibility::Hidden));
        }
        commands.entity(entity).insert(Visibility::Inherited);
        commands.entity(entity).remove::<PendingLodChunk>();
        if let Some(LodChunkStatus::Loading {
            root: expected_root,
            generation,
            origin,
        }) = streaming.chunks.get(&root.key).copied()
            && expected_root == entity
            && generation == root.generation
        {
            streaming.chunks.insert(
                root.key,
                LodChunkStatus::Ready {
                    root: entity,
                    origin,
                },
            );
        }
        metrics.lod_chunks_ready = metrics.lod_chunks_ready.saturating_add(1);
        metrics.ready_lod_terrain_patches = metrics
            .ready_lod_terrain_patches
            .saturating_add(patches.len() as u64);
        profiler.increment("lod/chunks_ready", 1);
        profiler.increment("lod/terrain_patches_ready", patches.len() as u64);
        profiler.record_elapsed("lod/chunk_ready", pending.started);
        profiler.event(format!("{:?}", root.key), "ready", None);
    }
    update_counts(&streaming, &mut metrics, &mut profiler);
    profiler.record_elapsed("lod/readiness", started);
}

pub(super) fn update_terrain_lod_visibility(
    config: Res<EngineConfig>,
    origin: Res<RenderOrigin>,
    camera: Query<&Transform, With<StreamingCamera>>,
    full_detail: Query<&TerrainCoverage, (With<super::TerrainPatch>, With<TerrainSurfaceReady>)>,
    mut lod_patches: Query<
        (
            &TerrainCoverage,
            Option<&TerrainSurfaceReady>,
            &mut Visibility,
        ),
        Without<super::TerrainPatch>,
    >,
    mut metrics: ResMut<StreamingMetrics>,
    mut profiler: ResMut<ProfilingState>,
) {
    let started = Instant::now();
    let Ok(camera) = camera.single() else {
        return;
    };
    let camera_grid = streaming_center(config.acceptance_screenshot.is_some(), origin.0, camera);
    let full_ready: HashSet<_> = full_detail
        .iter()
        .map(|coverage| (coverage.grid, coverage.quadrant))
        .collect();
    let mut available = HashMap::<(IVec2, u8), HashSet<LodTier>>::new();
    let mut ready_patches = 0usize;
    for (coverage, ready, _) in &mut lod_patches {
        if let (Some(tier), Some(_)) = (coverage.tier, ready) {
            available
                .entry((coverage.grid, coverage.quadrant))
                .or_default()
                .insert(tier);
            ready_patches += 1;
        }
    }
    let selected_tiers: HashMap<_, _> = available
        .iter()
        .map(|(&(grid, quadrant), candidates)| {
            let distance = chebyshev_grid_distance(grid, camera_grid);
            (
                (grid, quadrant),
                select_terrain_lod_tier(
                    distance,
                    full_ready.contains(&(grid, quadrant)),
                    candidates,
                ),
            )
        })
        .collect();
    let mut visible_patches = 0usize;
    for (coverage, ready, mut visibility) in &mut lod_patches {
        let selected = coverage.tier.is_some_and(|tier| {
            ready.is_some()
                && selected_tiers.get(&(coverage.grid, coverage.quadrant)) == Some(&Some(tier))
        });
        let next = if selected {
            Visibility::Inherited
        } else {
            Visibility::Hidden
        };
        if selected {
            visible_patches += 1;
        }
        if *visibility != next {
            *visibility = next;
        }
    }
    metrics.visible_lod_terrain_patches = visible_patches;
    profiler.set_gauge("lod/ready_terrain_patches", ready_patches as f64);
    profiler.set_gauge("lod/visible_terrain_patches", visible_patches as f64);
    profiler.record_elapsed("lod/visibility", started);
}

fn fail_chunk(
    entity: Entity,
    root: &LodChunkRoot,
    reason: String,
    commands: &mut Commands,
    streaming: &mut LodStreaming,
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
) {
    warn!(?root.key, %reason, "terrain LOD chunk failed validation or loading");
    metrics.failed_lod_chunks = metrics.failed_lod_chunks.saturating_add(1);
    profiler.increment("lod/chunks_failed", 1);
    profiler.event(format!("{:?}", root.key), "failed", None);
    if let Some(LodChunkStatus::Loading {
        root: expected_root,
        generation,
        ..
    }) = streaming.chunks.get(&root.key).copied()
        && expected_root == entity
        && generation == root.generation
    {
        streaming.chunks.insert(
            root.key,
            LodChunkStatus::Failed {
                origin: root.origin,
            },
        );
    }
    commands.entity(entity).try_despawn();
}

fn validate_lod_scene(
    root: Entity,
    metadata: &LodChunkMetadata,
    children: &Query<&Children>,
    names: &Query<&Name>,
    mesh_handles: &Query<&Mesh3d>,
    meshes: &Assets<Mesh>,
) -> Result<Vec<(Entity, TerrainCoverage)>, String> {
    let key = metadata.key;
    let source_cells: HashSet<_> = metadata
        .source_cells
        .iter()
        .map(|[x, y]| IVec2::new(*x, *y))
        .collect();
    if source_cells.is_empty() || source_cells.len() != metadata.source_cells.len() {
        return Err(format!(
            "{key:?} has empty or duplicate source-cell metadata"
        ));
    }
    if source_cells
        .iter()
        .any(|cell| !chunk_contains_cell(key, metadata.origin, *cell))
    {
        return Err(format!(
            "{key:?} source-cell metadata lies outside its chunk"
        ));
    }

    let mut stack = vec![(root, None)];
    let mut seen = HashSet::new();
    let mut patches = Vec::with_capacity(source_cells.len() * 4);
    while let Some((entity, inherited_grid)) = stack.pop() {
        let name = names.get(entity).ok().map(Name::as_str);
        let source_grid = match name.filter(|name| name.starts_with("cell_")) {
            Some(name) => {
                let Some((x, y)) = nodes::parse_source_cell(name) else {
                    return Err(format!("{key:?} has malformed source-cell node {name:?}"));
                };
                let grid = IVec2::new(x, y);
                if !source_cells.contains(&grid) {
                    return Err(format!("{key:?} has unexpected source cell {grid:?}"));
                }
                Some(grid)
            }
            None => inherited_grid,
        };
        if let Some(name) = name.filter(|name| name.starts_with("terrain_quadrant_")) {
            let quadrant = quadrant_from_node_name(name)
                .ok_or_else(|| format!("{key:?} has unknown terrain quadrant node {name:?}"))?;
            let grid = source_grid
                .ok_or_else(|| format!("{key:?} quadrant node has no source-cell parent"))?;
            // Bevy's glTF loader spawns each mesh primitive as a child of the
            // glTF node entity, not on the node itself: the quadrant node
            // carries the name, its single primitive child carries the Mesh3d.
            let primitive_children: Vec<Entity> = children
                .get(entity)
                .map(|direct| {
                    direct
                        .iter()
                        .filter(|child| mesh_handles.contains(*child))
                        .collect()
                })
                .unwrap_or_default();
            let [primitive] = primitive_children.as_slice() else {
                return Err(format!(
                    "{key:?} quadrant node {name:?} has {} mesh primitives, expected 1",
                    primitive_children.len()
                ));
            };
            let mesh_handle = mesh_handles
                .get(*primitive)
                .map_err(|_| format!("{key:?} quadrant node {name:?} has no mesh"))?;
            let mesh = meshes
                .get(&mesh_handle.0)
                .ok_or_else(|| format!("{key:?} quadrant node {name:?} mesh is missing"))?;
            validate_lod_quadrant_mesh(mesh)
                .map_err(|reason| format!("{key:?} {grid:?} quadrant {quadrant}: {reason}"))?;
            if !seen.insert((grid, quadrant)) {
                return Err(format!("{key:?} duplicates {grid:?} quadrant {quadrant}"));
            }
            patches.push((
                entity,
                TerrainCoverage {
                    grid,
                    quadrant,
                    tier: Some(key.tier),
                },
            ));
        }
        if let Ok(direct_children) = children.get(entity) {
            for child in direct_children.iter() {
                stack.push((child, source_grid));
            }
        }
    }

    if seen.len() != source_cells.len() * 4 {
        return Err(format!(
            "{key:?} contains {} terrain quadrants for {} source cells; expected {}",
            seen.len(),
            source_cells.len(),
            source_cells.len() * 4
        ));
    }
    Ok(patches)
}

fn validate_lod_quadrant_mesh(mesh: &Mesh) -> Result<(), String> {
    if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
        return Err("mesh is not a triangle list".to_owned());
    }
    let positions = match mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
        Some(VertexAttributeValues::Float32x3(values)) if values.len() == 9 => values,
        _ => return Err("mesh must contain nine Float32x3 positions".to_owned()),
    };
    if !positions.iter().flatten().all(|value| value.is_finite()) {
        return Err("mesh positions contain non-finite values".to_owned());
    }
    match mesh.attribute(Mesh::ATTRIBUTE_NORMAL) {
        Some(VertexAttributeValues::Float32x3(values))
            if values.len() == positions.len()
                && values.iter().flatten().all(|value| value.is_finite()) => {}
        _ => return Err("mesh normals do not match its positions".to_owned()),
    }
    match mesh.attribute(Mesh::ATTRIBUTE_UV_0) {
        Some(VertexAttributeValues::Float32x2(values))
            if values.len() == positions.len()
                && values
                    .iter()
                    .flatten()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value)) => {}
        _ => return Err("mesh UVs are missing, non-finite, or outside 0..1".to_owned()),
    }
    match mesh.attribute(Mesh::ATTRIBUTE_COLOR) {
        Some(VertexAttributeValues::Float32x4(values))
            if values.len() == positions.len()
                && values
                    .iter()
                    .flatten()
                    .all(|value| value.is_finite() && (0.0..=1.0).contains(value)) => {}
        _ => return Err("mesh colors are missing, non-finite, or outside 0..1".to_owned()),
    }
    let index_count = match mesh.indices() {
        Some(Indices::U16(indices)) if indices.len() == 24 => {
            if indices
                .iter()
                .any(|index| usize::from(*index) >= positions.len())
            {
                return Err("mesh has an out-of-range index".to_owned());
            }
            indices.len()
        }
        Some(Indices::U32(indices)) if indices.len() == 24 => {
            if indices
                .iter()
                .any(|index| *index as usize >= positions.len())
            {
                return Err("mesh has an out-of-range index".to_owned());
            }
            indices.len()
        }
        _ => return Err("mesh must contain 24 triangle indices".to_owned()),
    };
    if index_count % 3 != 0 {
        return Err("mesh indices do not form complete triangles".to_owned());
    }
    Ok(())
}

fn select_terrain_lod_tier(
    grid_distance: i32,
    full_detail_ready: bool,
    available: &HashSet<LodTier>,
) -> Option<LodTier> {
    if full_detail_ready {
        return None;
    }
    LodTier::ALL
        .into_iter()
        .find(|tier| grid_distance <= tier.side_cells() && available.contains(tier))
}

fn chebyshev_grid_distance(left: IVec2, right: IVec2) -> i32 {
    left.x
        .saturating_sub(right.x)
        .saturating_abs()
        .max(left.y.saturating_sub(right.y).saturating_abs())
}

fn quadrant_from_node_name(name: &str) -> Option<u8> {
    let direction = name.strip_prefix("terrain_quadrant_")?;
    ["sw", "se", "nw", "ne"]
        .iter()
        .position(|candidate| *candidate == direction)
        .map(|index| index as u8)
}

fn streaming_center(screenshot: bool, origin: IVec2, camera: &Transform) -> IVec2 {
    if screenshot {
        return origin;
    }
    let global_x = camera.translation.x + origin.x as f32 * CELL_SIZE;
    let global_y = -camera.translation.z + origin.y as f32 * CELL_SIZE;
    IVec2::new(
        (global_x / CELL_SIZE).floor() as i32,
        (global_y / CELL_SIZE).floor() as i32,
    )
}

fn query_for(worldspace_id: u32, tier: LodTier, center: IVec2, radius: i32) -> LodChunkQuery {
    let min_x = (i64::from(center.x) - i64::from(radius)) as f64 * f64::from(CELL_SIZE);
    let min_y = (i64::from(center.y) - i64::from(radius)) as f64 * f64::from(CELL_SIZE);
    let max_x = (i64::from(center.x) + i64::from(radius) + 1) as f64 * f64::from(CELL_SIZE);
    let max_y = (i64::from(center.y) + i64::from(radius) + 1) as f64 * f64::from(CELL_SIZE);
    LodChunkQuery {
        worldspace_id,
        tier,
        bounds_min: [min_x, min_y],
        bounds_max: [max_x, max_y],
    }
}

fn is_current_query(streaming: &LodStreaming, generation: u64) -> bool {
    streaming.center.is_some() && generation == streaming.generation
}

fn query_unload_radius() -> i32 {
    LOD_MAX_DISTANCE_CELLS + LOD_UNLOAD_MARGIN_CELLS
}

fn chunk_min_grid(key: ChunkKey, origin: LodOrigin) -> (i64, i64) {
    let side = i64::from(key.tier.side_cells());
    (
        i64::from(origin.grid_x) + i64::from(key.anchor.x) * side,
        i64::from(origin.grid_y) + i64::from(key.anchor.y) * side,
    )
}

fn chunk_contains_cell(key: ChunkKey, origin: LodOrigin, cell: IVec2) -> bool {
    let side = i64::from(key.tier.side_cells());
    let min = chunk_min_grid(key, origin);
    let x = i64::from(cell.x);
    let y = i64::from(cell.y);
    x >= min.0 && x < min.0 + side && y >= min.1 && y < min.1 + side
}

fn chunk_within_radius(key: ChunkKey, origin: LodOrigin, center: (i64, i64), radius: i32) -> bool {
    let side = i64::from(key.tier.side_cells());
    let min = chunk_min_grid(key, origin);
    let max = (min.0 + side - 1, min.1 + side - 1);
    let distance = |value: i64, low: i64, high: i64| {
        if value < low {
            low - value
        } else if value > high {
            value - high
        } else {
            0
        }
    };
    distance(center.0, min.0, max.0).max(distance(center.1, min.1, max.1)) <= i64::from(radius)
}

fn verify_payload_hash(path: PathBuf, expected: String) -> Result<(), String> {
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("failed to read LOD payload {}: {error}", path.display()))?;
    let digest = Sha256::digest(bytes);
    let actual: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "LOD payload {} has SHA-256 {actual}, expected {expected}",
            path.display()
        ))
    }
}

fn update_counts(
    streaming: &LodStreaming,
    metrics: &mut StreamingMetrics,
    profiler: &mut ProfilingState,
) {
    metrics.pending_lod_queries = streaming.pending_queries.len();
    metrics.resident_lod_chunks = streaming
        .chunks
        .values()
        .filter(|status| matches!(status, LodChunkStatus::Ready { .. }))
        .count();
    metrics.pending_lod_chunks = streaming
        .chunks
        .values()
        .filter(|status| matches!(status, LodChunkStatus::Loading { .. }))
        .count();
    profiler.set_gauge("lod/resident_chunks", metrics.resident_lod_chunks as f64);
    profiler.set_gauge("lod/pending_chunks", metrics.pending_lod_chunks as f64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn tier_handoff_is_per_quadrant_and_falls_back_to_ready_coarser_data() {
        let available = HashSet::from([LodTier::Tier4, LodTier::Tier8, LodTier::Tier16]);
        assert_eq!(
            select_terrain_lod_tier(2, false, &available),
            Some(LodTier::Tier4)
        );
        assert_eq!(
            select_terrain_lod_tier(6, false, &available),
            Some(LodTier::Tier8)
        );
        assert_eq!(
            select_terrain_lod_tier(12, false, &available),
            Some(LodTier::Tier16)
        );
        assert_eq!(select_terrain_lod_tier(2, true, &available), None);

        let coarse_only = HashSet::from([LodTier::Tier16]);
        assert_eq!(
            select_terrain_lod_tier(2, false, &coarse_only),
            Some(LodTier::Tier16)
        );
        assert_eq!(select_terrain_lod_tier(17, false, &coarse_only), None);
        assert_eq!(select_terrain_lod_tier(2, false, &HashSet::new()), None);
    }

    #[test]
    fn lod_grid_rebase_and_unload_bounds_handle_negative_anchors() {
        let origin = LodOrigin::new(8, -8);
        let key = ChunkKey::new(0x3c, LodTier::Tier4, shared::lod::ChunkAnchor::new(-1, 2));
        assert_eq!(chunk_min_grid(key, origin), (4, 0));
        assert!(chunk_contains_cell(key, origin, IVec2::new(7, 3)));
        assert!(!chunk_contains_cell(key, origin, IVec2::new(8, 3)));
        assert!(chunk_within_radius(key, origin, (3, 1), 1));
        assert!(!chunk_within_radius(key, origin, (10, 1), 1));
    }

    #[test]
    fn two_cell_handoff_is_independent_and_falls_back_after_a_tier_failure() {
        let mut world = World::new();
        world.insert_resource(EngineConfig::default());
        world.insert_resource(RenderOrigin(IVec2::ZERO));
        world.insert_resource(StreamingMetrics::default());
        world.insert_resource(ProfilingState::default());
        let camera = world
            .spawn((
                StreamingCamera,
                Transform::from_xyz(CELL_SIZE * 0.5, 0.0, -CELL_SIZE * 0.5),
            ))
            .id();

        let full_patches: Vec<_> = (0..4)
            .map(|quadrant| {
                world
                    .spawn((
                        super::super::TerrainPatch,
                        TerrainCoverage {
                            grid: IVec2::ZERO,
                            quadrant,
                            tier: None,
                        },
                        TerrainSurfaceReady,
                        Visibility::Inherited,
                    ))
                    .id()
            })
            .collect();
        let mut lod_patches = HashMap::new();
        for quadrant in 0..4 {
            for tier in LodTier::ALL {
                let zero = world
                    .spawn((
                        TerrainCoverage {
                            grid: IVec2::ZERO,
                            quadrant,
                            tier: Some(tier),
                        },
                        TerrainSurfaceReady,
                        Visibility::Hidden,
                    ))
                    .id();
                let three = world
                    .spawn((
                        TerrainCoverage {
                            grid: IVec2::new(3, 0),
                            quadrant,
                            tier: Some(tier),
                        },
                        TerrainSurfaceReady,
                        Visibility::Hidden,
                    ))
                    .id();
                lod_patches.insert((IVec2::ZERO, tier, quadrant), zero);
                lod_patches.insert((IVec2::new(3, 0), tier, quadrant), three);
            }
        }

        world
            .run_system_once(update_terrain_lod_visibility)
            .unwrap();
        assert!(
            full_patches.iter().all(|entity| {
                *world.get::<Visibility>(*entity).unwrap() == Visibility::Inherited
            })
        );
        for quadrant in 0..4 {
            assert_eq!(
                *world
                    .get::<Visibility>(lod_patches[&(IVec2::ZERO, LodTier::Tier4, quadrant)])
                    .unwrap(),
                Visibility::Hidden,
                "full-detail cell zero owns quadrant {quadrant}"
            );
            assert_eq!(
                *world
                    .get::<Visibility>(lod_patches[&(IVec2::new(3, 0), LodTier::Tier4, quadrant)])
                    .unwrap(),
                Visibility::Inherited,
                "cell three independently selects tier 4 for quadrant {quadrant}"
            );
        }

        world
            .entity_mut(full_patches[1])
            .remove::<TerrainSurfaceReady>();
        for quadrant in 0..4 {
            world
                .entity_mut(lod_patches[&(IVec2::new(3, 0), LodTier::Tier4, quadrant)])
                .remove::<TerrainSurfaceReady>();
        }
        world
            .run_system_once(update_terrain_lod_visibility)
            .unwrap();
        assert_eq!(
            *world
                .get::<Visibility>(lod_patches[&(IVec2::ZERO, LodTier::Tier4, 1)])
                .unwrap(),
            Visibility::Inherited,
            "a single unready full-detail quadrant transfers to LOD"
        );
        for quadrant in 0..4 {
            assert_eq!(
                *world
                    .get::<Visibility>(lod_patches[&(IVec2::new(3, 0), LodTier::Tier8, quadrant)])
                    .unwrap(),
                Visibility::Inherited,
                "ready tier 8 covers failed tier 4 in quadrant {quadrant}"
            );
        }

        world.get_mut::<Transform>(camera).unwrap().translation.x = CELL_SIZE * 10.5;
        world
            .run_system_once(update_terrain_lod_visibility)
            .unwrap();
        assert_eq!(
            *world
                .get::<Visibility>(lod_patches[&(IVec2::ZERO, LodTier::Tier16, 1)])
                .unwrap(),
            Visibility::Inherited,
            "teleporting outward selects tier 16 for cell zero"
        );
        assert_eq!(
            *world
                .get::<Visibility>(lod_patches[&(IVec2::new(3, 0), LodTier::Tier8, 0)])
                .unwrap(),
            Visibility::Inherited,
            "cell three retains its independent tier selection"
        );

        let current = LodStreaming {
            generation: 2,
            center: Some(IVec2::new(10, 0)),
            ..default()
        };
        assert!(
            !is_current_query(&current, 1),
            "pre-teleport results are stale"
        );
        assert!(is_current_query(&current, 2));

        let fine_chunk = ChunkKey::new(0x3c, LodTier::Tier4, shared::lod::ChunkAnchor::new(0, 0));
        assert!(!chunk_within_radius(
            fine_chunk,
            LodOrigin::new(0, 0),
            (22, 0),
            LOD_MAX_DISTANCE_CELLS + LOD_UNLOAD_MARGIN_CELLS,
        ));
    }
}
