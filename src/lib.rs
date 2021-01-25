mod bounding;
mod debug;
mod primitives;
mod raycast;

pub use crate::bounding::{update_bound_sphere, BoundVol, BoundingSphere};
pub use crate::debug::*;
pub use crate::primitives::*;

use crate::raycast::*;
use bevy::{
    prelude::*,
    render::{
        camera::Camera,
        mesh::{Indices, Mesh, VertexAttributeValues},
        pipeline::PrimitiveTopology,
    },
    tasks::{ComputeTaskPool, ParallelIterator},
    window::CursorMoved,
};
use std::marker::PhantomData;

/// Marks a Mesh entity as pickable
#[derive(Debug)]
pub struct RayCastMesh<T> {
    intersection: Option<Intersection>,
    _marker: PhantomData<T>,
}
impl<T> Default for RayCastMesh<T> {
    fn default() -> Self {
        RayCastMesh {
            intersection: None,
            _marker: PhantomData::default(),
        }
    }
}
impl<T> RayCastMesh<T> {
    pub fn intersection(&self) -> Option<Intersection> {
        self.intersection
    }
}

/// Specifies the method used to generate rays
pub enum RayCastMethod {
    /// Use cursor events to get coordinates  relative to a camera
    CameraCursor(UpdateOn, EventReader<CursorMoved>),
    /// Manually specify screen coordinates relative to a camera
    CameraScreenSpace(Vec2),
    /// Use a tranform in world space to define pick ray
    Transform,
}

// TODO
// instead of making user specify when to update the picks, have it be event driven in the bevy ecs system
// basically, the user is responsible for triggering events. Need a way of having a default every frame method

#[derive(Debug, Clone, Copy)]
pub enum UpdateOn {
    EveryFrame(Vec2),
    OnMouseEvent,
}

pub struct RayCastSource<T> {
    pub cast_method: RayCastMethod,
    ray: Option<Ray3d>,
    intersections: Vec<(Entity, Intersection)>,
    _marker: PhantomData<T>,
}

impl<T> RayCastSource<T> {
    pub fn new(pick_method: RayCastMethod) -> Self {
        RayCastSource {
            cast_method: pick_method,
            ray: None,
            intersections: Vec::new(),
            _marker: PhantomData::default(),
        }
    }
    pub fn intersect_list(&self) -> Option<&Vec<(Entity, Intersection)>> {
        if self.intersections.is_empty() {
            None
        } else {
            Some(&self.intersections)
        }
    }
    pub fn intersect_top(&self) -> Option<(Entity, Intersection)> {
        if self.intersections.is_empty() {
            None
        } else {
            self.intersections.first().copied()
        }
    }
}

impl<T> Default for RayCastSource<T> {
    fn default() -> Self {
        RayCastSource {
            cast_method: RayCastMethod::CameraCursor(
                UpdateOn::EveryFrame(Vec2::zero()),
                EventReader::default(),
            ),
            ..Default::default()
        }
    }
}

pub fn update_raycast<T: 'static + Send + Sync>(
    // Resources
    pool: Res<ComputeTaskPool>,
    meshes: ResMut<Assets<Mesh>>,
    cursor: Res<Events<CursorMoved>>,
    windows: Res<Windows>,
    // Queries
    mut pick_source_query: Query<(
        &mut RayCastSource<T>,
        Option<&GlobalTransform>,
        Option<&Camera>,
    )>,
    culling_query: Query<
        (&Visible, Option<&BoundVol>, &GlobalTransform, Entity),
        With<RayCastMesh<T>>,
    >,
    mut mesh_query: Query<(&mut RayCastMesh<T>, &Handle<Mesh>, &GlobalTransform, Entity)>,
) {
    // Generate a ray for the picking source based on the pick method
    for (mut pick_source, transform, camera) in &mut pick_source_query.iter_mut() {
        pick_source.ray = match &mut pick_source.cast_method {
            // Use cursor events and specified window/camera to generate a ray
            RayCastMethod::CameraCursor(update_picks, event_reader) => {
                let camera = match camera {
                    Some(camera) => camera,
                    None => panic!(
                        "The PickingSource is a CameraCursor but has no associated Camera component"
                    ),
                };
                let cursor_latest = match (*event_reader).latest(&cursor) {
                    Some(cursor_moved) => {
                        if cursor_moved.id == camera.window {
                            Some(cursor_moved)
                        } else {
                            None
                        }
                    }
                    None => None,
                };
                let cursor_pos_screen: Vec2 = match update_picks {
                    UpdateOn::EveryFrame(cached_pos) => match cursor_latest {
                        Some(cursor_moved) => {
                            //Updated the cached cursor position
                            pick_source.cast_method = RayCastMethod::CameraCursor(
                                UpdateOn::EveryFrame(cursor_moved.position),
                                EventReader::default(),
                            );
                            cursor_moved.position
                        }
                        None => *cached_pos,
                    },
                    UpdateOn::OnMouseEvent => match cursor_latest {
                        Some(cursor_moved) => cursor_moved.position,
                        None => continue,
                    },
                };

                // Get current screen size
                let window = windows
                    .get(camera.window)
                    .unwrap_or_else(|| panic!("WindowId {} does not exist", camera.window));
                let screen_size = Vec2::from([window.width() as f32, window.height() as f32]);

                // Normalized device coordinates (NDC) describes cursor position from (-1, -1, -1) to (1, 1, 1)
                let cursor_ndc = (cursor_pos_screen / screen_size) * 2.0 - Vec2::from([1.0, 1.0]);
                let cursor_pos_ndc_near: Vec3 = cursor_ndc.extend(-1.0);
                let cursor_pos_ndc_far: Vec3 = cursor_ndc.extend(1.0);

                let camera_matrix = match transform {
                    Some(matrix) => matrix,
                    None => panic!(
                        "The PickingSource is a CameraCursor but has no associated GlobalTransform component"
                    ),
                }
                .compute_matrix();

                let ndc_to_world: Mat4 = camera_matrix * camera.projection_matrix.inverse();
                let cursor_pos_near: Vec3 = ndc_to_world.transform_point3(cursor_pos_ndc_near);
                let cursor_pos_far: Vec3 = ndc_to_world.transform_point3(cursor_pos_ndc_far);

                let ray_direction = cursor_pos_far - cursor_pos_near;

                Some(Ray3d::new(cursor_pos_near, ray_direction))
            }
            // Use the camera and specified screen coordinates to generate a ray
            RayCastMethod::CameraScreenSpace(coordinates_ndc) => {
                let projection_matrix = match camera {
                    Some(camera) => camera.projection_matrix,
                    None => panic!(
                        "The PickingSource is a CameraScreenSpace but has no associated Camera component"
                    ),
                };
                let cursor_pos_ndc_near: Vec3 = coordinates_ndc.extend(-1.0);
                let cursor_pos_ndc_far: Vec3 = coordinates_ndc.extend(1.0);
                let camera_matrix = match transform {
                    Some(matrix) => matrix,
                    None => panic!(
                        "The PickingSource is a CameraScreenSpace but has no associated GlobalTransform component"
                    ),
                }
                .compute_matrix();

                let ndc_to_world: Mat4 = camera_matrix * projection_matrix.inverse();
                let cursor_pos_near: Vec3 = ndc_to_world.transform_point3(cursor_pos_ndc_near);
                let cursor_pos_far: Vec3 = ndc_to_world.transform_point3(cursor_pos_ndc_far);

                let ray_direction = cursor_pos_far - cursor_pos_near;

                Some(Ray3d::new(cursor_pos_near, ray_direction))
            }
            // Use the specified transform as the origin and direction of the ray
            RayCastMethod::Transform => {
                let pick_position_ndc = Vec3::from([0.0, 0.0, 1.0]);
                let source_transform = match transform {
                    Some(matrix) => matrix,
                    None => panic!(
                        "The PickingSource is a Transform but has no associated GlobalTransform component"
                    ),
                }
                .compute_matrix();
                let pick_position = source_transform.transform_point3(pick_position_ndc);

                let (_, _, source_origin) = source_transform.to_scale_rotation_translation();
                let ray_direction = pick_position - source_origin;

                Some(Ray3d::new(source_origin, ray_direction))
            }
        };

        if let Some(ray) = pick_source.ray {
            pick_source.intersections.clear();
            // Create spans for tracing
            let ray_cull = info_span!("ray culling");
            let raycast = info_span!("raycast");

            // Check all entities to see if the source ray intersects the bounding sphere, use this
            // to build a short list of entities that are in the path of the ray.
            let culled_list: Vec<Entity> = {
                let _ray_cull_guard = ray_cull.enter();
                culling_query
                    .par_iter(32)
                    .map(|(visibility, bound_vol, transform, entity)| {
                        let visible = visibility.is_visible;
                        let bound_hit = if let Some(bound_vol) = bound_vol {
                            if let Some(sphere) = &bound_vol.sphere {
                                let scaled_radius: f32 =
                                    1.01 * sphere.radius() * transform.scale.max_element();
                                let translated_origin =
                                    sphere.origin() * transform.scale + transform.translation;
                                let det = (ray.direction().dot(ray.origin() - translated_origin))
                                    .powi(2)
                                    - (Vec3::length_squared(ray.origin() - translated_origin)
                                        - scaled_radius.powi(2));
                                if det < 0.0 {
                                    false // Ray does not intersect the bounding sphere - skip entity
                                } else {
                                    true // Ray intersects the bounding sphere!
                                }
                            } else {
                                true // This bounding volume's sphere is not yet defined
                            }
                        } else {
                            true // This entity has no bounding volume
                        };
                        if visible && bound_hit {
                            Some(entity)
                        } else {
                            None
                        }
                    })
                    .filter_map(|value| value)
                    .collect(&pool)
            };

            for (mut pickable, mesh_handle, transform, entity) in mesh_query.iter_mut() {
                if !culled_list.contains(&entity) {
                    continue;
                }
                let _raycast_guard = raycast.enter();
                // Use the mesh handle to get a reference to a mesh asset
                if let Some(mesh) = meshes.get(mesh_handle) {
                    if mesh.primitive_topology() != PrimitiveTopology::TriangleList {
                        panic!("bevy_mod_picking only supports TriangleList topology");
                    }
                    // Get the vertex positions from the mesh reference resolved from the mesh handle
                    let vertex_positions: Vec<[f32; 3]> =
                        match mesh.attribute(Mesh::ATTRIBUTE_POSITION) {
                            None => panic!("Mesh does not contain vertex positions"),
                            Some(vertex_values) => match &vertex_values {
                                VertexAttributeValues::Float3(positions) => positions.clone(),
                                _ => panic!("Unexpected vertex types in ATTRIBUTE_POSITION"),
                            },
                        };
                    if let Some(indices) = &mesh.indices() {
                        // Iterate over the list of pick rays that belong to the same group as this mesh
                        let mesh_to_world = transform.compute_matrix();
                        let new_intersection = match indices {
                            Indices::U16(vector) => ray_mesh_intersection(
                                &mesh_to_world,
                                &vertex_positions,
                                &ray,
                                &vector.iter().map(|x| *x as u32).collect(),
                            ),
                            Indices::U32(vector) => ray_mesh_intersection(
                                &mesh_to_world,
                                &vertex_positions,
                                &ray,
                                vector,
                            ),
                        };
                        pickable.intersection = new_intersection;
                        if let Some(new_intersection) = new_intersection {
                            pick_source.intersections.push((entity, new_intersection));
                        }
                    } else {
                        // If we get here the mesh doesn't have an index list!
                        panic!(
                            "No index matrix found in mesh {:?}\n{:?}",
                            mesh_handle, mesh
                        );
                    }
                }
            }

            // Sort the pick list
            pick_source.intersections.sort_by(|a, b| {
                a.1.distance()
                    .partial_cmp(&b.1.distance())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
}

fn ray_mesh_intersection(
    mesh_to_world: &Mat4,
    vertex_positions: &[[f32; 3]],
    pick_ray: &Ray3d,
    indices: &Vec<u32>,
) -> Option<Intersection> {
    // The ray cast can hit the same mesh many times, so we need to track which hit is
    // closest to the camera, and record that.
    let mut min_pick_distance = f32::MAX;
    let mut pick_intersection: Option<Intersection> = None;

    // Make sure this chunk has 3 vertices to avoid a panic.
    if indices.len() % 3 == 0 {
        // Now that we're in the vector of vertex indices, we want to look at the vertex
        // positions for each triangle, so we'll take indices in chunks of three, where each
        // chunk of three indices are references to the three vertices of a triangle.
        for index in indices.chunks(3) {
            // Construct a triangle in world space using the mesh data
            let mut world_vertices: [Vec3; 3] = [Vec3::zero(), Vec3::zero(), Vec3::zero()];
            for i in 0..3 {
                let vertex_index = index[i] as usize;
                world_vertices[i] =
                    mesh_to_world.transform_point3(Vec3::from(vertex_positions[vertex_index]));
            }
            let world_triangle = Triangle::from(world_vertices);
            if world_vertices
                .iter()
                .map(|vert| (*vert - pick_ray.origin()).length().abs())
                .fold(f32::INFINITY, |a, b| a.min(b))
                > min_pick_distance
            {
                continue;
            }
            // Run the raycast on the ray and triangle
            if let Some(intersection) =
                ray_triangle_intersection(pick_ray, &world_triangle, RaycastAlgorithm::default())
            {
                let distance: f32 = (intersection.origin() - pick_ray.origin()).length().abs();
                if distance < min_pick_distance {
                    min_pick_distance = distance;
                    pick_intersection =
                        Some(Intersection::new(intersection, distance, world_triangle));
                }
            }
        }
    }
    pick_intersection
}

/*
fn par_ray_mesh_intersection(
    mesh_to_world: &Mat4,
    vertex_positions: &[[f32; 3]],
    pick_ray: &Ray3d,
    indices: &Vec<u32>,
) -> Option<Intersection> {
    // Make sure this chunk has 3 vertices to avoid a panic.
    let indices: &Vec<Vec<u32>> = &indices.chunks(3).map(|x| x.to_vec()).collect();

    // Now that we're in the vector of vertex indices, we want to look at the vertex
    // positions for each triangle, so we'll take indices in chunks of three, where each
    // chunk of three indices are references to the three vertices of a triangle.
    let pick_intersection = indices.par_iter().map( |index| {
        // Construct a triangle in world space using the mesh data
        let mut world_vertices: [Vec3; 3] = [Vec3::zero(), Vec3::zero(), Vec3::zero()];
        for i in 0..3 {
            let vertex_index: usize = index[i] as usize;
            world_vertices[i] =
                    mesh_to_world.transform_point3(Vec3::from(vertex_positions[vertex_index]));
        }
        let world_triangle = Triangle::from(world_vertices);
        // Run the raycast on the ray and triangle
        let intersection = ray_triangle_intersection(pick_ray, &world_triangle, RaycastAlgorithm::default());
        match intersection {
            Some(intersection) => {
                let distance = (intersection.origin() - pick_ray.origin()).length().abs();
                Some(Intersection::new(intersection, distance, world_triangle))
            }
            None => None,
        }
    })
    .filter_map(Option::Some)
    .reduce(|| None, |a,b| {
        if let Some(a) = a {
            match b {
                None => Some(a),
                Some(b) => if a.distance()<b.distance() {
                    Some(a)
                } else {
                    Some(b)
                }
            }
        } else {
            b
        }
    });

    pick_intersection
}
*/
