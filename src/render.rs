use crate::types::{Camera, Model, Raster, Vertex};
use glam::Vec3;
use std::path::Path;

const MAX_SIDE: u32 = 4096;

/// Keep every output (and its depth buffer) within 16 megapixels, even for bad UI sizes.
pub(crate) fn bounded_dimensions(width: u32, height: u32) -> (u32, u32) {
    let width = width.max(1);
    let height = height.max(1);
    let scale = (MAX_SIDE as f64 / f64::from(width.max(height))).min(1.0);
    (
        (f64::from(width) * scale).floor().max(1.0) as u32,
        (f64::from(height) * scale).floor().max(1.0) as u32,
    )
}

/// Orthographic, Z-up orbit view. Depth increases toward the camera. Triangle soup is
/// processed directly: no projected-mesh copy, triangle sorting, or per-triangle heap work.
pub fn render_model(model: &Model, width: u32, height: u32, camera: Camera) -> Raster {
    if model.vertices.len() < 3 {
        return empty_raster(width, height);
    }
    let mut frame = RasterFrame::new(model.bounds_min, model.bounds_max, width, height, camera);
    for triangle in model.vertices.as_chunks::<3>().0 {
        frame.draw(*triangle);
    }
    frame.raster
}

/// Render every source facet without retaining an STL's triangle soup in memory.
pub fn render_file_thumbnail(path: &Path, width: u32, height: u32) -> anyhow::Result<Raster> {
    if path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("stl"))
    {
        let info = crate::model::visit_stl(path, |_| Ok(()))?;
        let mut frame = RasterFrame::new(
            info.bounds_min,
            info.bounds_max,
            width,
            height,
            Camera::default(),
        );
        crate::model::visit_stl(path, |triangle| {
            frame.draw(triangle);
            Ok(())
        })?;
        Ok(frame.raster)
    } else {
        let model = crate::model::load_model_full(path)?;
        Ok(render_model(&model, width, height, Camera::default()))
    }
}

fn empty_raster(width: u32, height: u32) -> Raster {
    let (width, height) = bounded_dimensions(width, height);
    Raster {
        width,
        height,
        rgba: vec![0; width as usize * height as usize * 4],
    }
}

struct Projection {
    center: [f64; 3],
    radius: f64,
    right: Vec3,
    up: Vec3,
    toward: Vec3,
    scale: f32,
    pan: [f32; 2],
    light: Vec3,
}

impl Projection {
    fn new(
        bounds_min: [f32; 3],
        bounds_max: [f32; 3],
        width: u32,
        height: u32,
        camera: Camera,
    ) -> Option<Self> {
        let mut center = [0.0_f64; 3];
        let mut radius_squared = 0.0;
        for (axis, c) in center.iter_mut().enumerate() {
            let lo = f64::from(bounds_min[axis]);
            let hi = f64::from(bounds_max[axis]);
            if !lo.is_finite() || !hi.is_finite() || hi < lo {
                return None;
            }
            *c = (lo + hi) * 0.5;
            radius_squared += ((hi - lo) * 0.5).powi(2);
        }
        let radius = radius_squared.sqrt().max(1.0e-15);
        let yaw = if camera.yaw.is_finite() {
            camera.yaw
        } else {
            -0.95
        };
        let pitch = if camera.pitch.is_finite() {
            camera.pitch
        } else {
            0.55
        };
        let zoom = if camera.zoom.is_finite() {
            camera.zoom.clamp(0.02, 1000.0)
        } else {
            1.0
        };
        let (sy, cy) = yaw.sin_cos();
        let (sp, cp) = pitch.sin_cos();
        let right = Vec3::new(-sy, cy, 0.0);
        let up = Vec3::new(-cy * sp, -sy * sp, cp);
        let toward = Vec3::new(cy * cp, sy * cp, sp);
        let scale = width.min(height) as f32 * 0.45 * zoom;
        let pan = camera.pan.map(|v| {
            if v.is_finite() {
                v * width.min(height) as f32
            } else {
                0.0
            }
        });
        let light = (toward * 0.7 - right * 0.3 + up * 0.6).normalize();
        Some(Self {
            center,
            radius,
            right,
            up,
            toward,
            scale,
            pan,
            light,
        })
    }
}

struct RasterFrame {
    raster: Raster,
    depth: Vec<f32>,
    projection: Option<Projection>,
}

impl RasterFrame {
    fn new(
        bounds_min: [f32; 3],
        bounds_max: [f32; 3],
        width: u32,
        height: u32,
        camera: Camera,
    ) -> Self {
        let raster = empty_raster(width, height);
        let projection =
            Projection::new(bounds_min, bounds_max, raster.width, raster.height, camera);
        let depth = if projection.is_some() {
            vec![f32::NEG_INFINITY; raster.width as usize * raster.height as usize]
        } else {
            Vec::new()
        };
        Self {
            raster,
            depth,
            projection,
        }
    }

    fn draw(&mut self, triangle: [Vertex; 3]) {
        let Some(projection) = &self.projection else {
            return;
        };
        let mut positions = [Vec3::ZERO; 3];
        let mut screen = [Vec3::ZERO; 3];
        let mut colors = [Vec3::ZERO; 3];
        let mut valid = true;
        for i in 0..3 {
            let p = triangle[i].position;
            positions[i] = Vec3::new(
                ((f64::from(p[0]) - projection.center[0]) / projection.radius) as f32,
                ((f64::from(p[1]) - projection.center[1]) / projection.radius) as f32,
                ((f64::from(p[2]) - projection.center[2]) / projection.radius) as f32,
            );
            valid &= positions[i].is_finite();
            screen[i] = Vec3::new(
                self.raster.width as f32 * 0.5
                    + projection.pan[0]
                    + positions[i].dot(projection.right) * projection.scale,
                self.raster.height as f32 * 0.5 + projection.pan[1]
                    - positions[i].dot(projection.up) * projection.scale,
                positions[i].dot(projection.toward),
            );
            colors[i] = Vec3::from_array(triangle[i].color).clamp(Vec3::ZERO, Vec3::ONE);
            valid &= colors[i].is_finite();
        }
        if !valid {
            return;
        }
        let normal = (positions[1] - positions[0]).cross(positions[2] - positions[0]);
        if !normal.is_finite() || normal.length_squared() == 0.0 {
            return;
        }
        // Two-sided shading intentionally keeps open STL surfaces and mirrored parts visible.
        let mut normal = normal.normalize();
        if normal.dot(projection.toward) < 0.0 {
            normal = -normal;
        }
        let shade = 0.38 + 0.62 * normal.dot(projection.light).max(0.0);
        draw_triangle(&mut self.raster, &mut self.depth, screen, colors, shade);
    }
}

fn edge(a: Vec3, b: Vec3, x: f32, y: f32) -> f32 {
    (b.x - a.x) * (y - a.y) - (b.y - a.y) * (x - a.x)
}

fn draw_triangle(
    raster: &mut Raster,
    depth: &mut [f32],
    p: [Vec3; 3],
    colors: [Vec3; 3],
    shade: f32,
) {
    let area = edge(p[0], p[1], p[2].x, p[2].y);
    if !area.is_finite() || area.abs() < 1.0e-8 {
        return;
    }
    let min_x = p
        .iter()
        .map(|v| v.x)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let max_x = p
        .iter()
        .map(|v| v.x)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(raster.width as f32 - 1.0);
    let min_y = p
        .iter()
        .map(|v| v.y)
        .fold(f32::INFINITY, f32::min)
        .floor()
        .max(0.0) as u32;
    let max_y = p
        .iter()
        .map(|v| v.y)
        .fold(f32::NEG_INFINITY, f32::max)
        .ceil()
        .min(raster.height as f32 - 1.0);
    if max_x < min_x as f32 || max_y < min_y as f32 {
        return;
    }
    let inv_area = area.recip();
    let dx = [
        (p[1].y - p[2].y) * inv_area,
        (p[2].y - p[0].y) * inv_area,
        (p[0].y - p[1].y) * inv_area,
    ];
    let dy = [
        (p[2].x - p[1].x) * inv_area,
        (p[0].x - p[2].x) * inv_area,
        (p[1].x - p[0].x) * inv_area,
    ];
    let x = min_x as f32 + 0.5;
    let y = min_y as f32 + 0.5;
    let mut row = [
        edge(p[1], p[2], x, y) * inv_area,
        edge(p[2], p[0], x, y) * inv_area,
        edge(p[0], p[1], x, y) * inv_area,
    ];
    for y in min_y..=max_y as u32 {
        let mut weights = row;
        let start = y as usize * raster.width as usize + min_x as usize;
        let end = y as usize * raster.width as usize + max_x as usize;
        let pixels = raster.rgba[start * 4..(end + 1) * 4].as_chunks_mut::<4>().0;
        for (stored_depth, pixel) in depth[start..=end].iter_mut().zip(pixels) {
            if weights[0] >= -1.0e-6 && weights[1] >= -1.0e-6 && weights[2] >= -1.0e-6 {
                let z = p[0].z * weights[0] + p[1].z * weights[1] + p[2].z * weights[2];
                if z > *stored_depth {
                    *stored_depth = z;
                    let color =
                        (colors[0] * weights[0] + colors[1] * weights[1] + colors[2] * weights[2])
                            * shade;
                    pixel[0] = (color.x.clamp(0.0, 1.0) * 255.0).round() as u8;
                    pixel[1] = (color.y.clamp(0.0, 1.0) * 255.0).round() as u8;
                    pixel[2] = (color.z.clamp(0.0, 1.0) * 255.0).round() as u8;
                    pixel[3] = 255;
                }
            }
            for i in 0..3 {
                weights[i] += dx[i];
            }
        }
        for i in 0..3 {
            row[i] += dy[i];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Vertex;

    fn model(vertices: Vec<Vertex>) -> Model {
        Model {
            triangle_count: (vertices.len() / 3) as u64,
            vertices,
            bounds_min: [-1.0; 3],
            bounds_max: [1.0; 3],
            palette: vec![],
            warnings: vec![],
        }
    }
    fn triangle(x: f32, color: [f32; 3]) -> Vec<Vertex> {
        [[x, -1.0, -1.0], [x, 1.0, -1.0], [x, 0.0, 1.0]]
            .into_iter()
            .map(|position| Vertex { position, color })
            .collect()
    }
    fn front() -> Camera {
        Camera {
            yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
            pan: [0.0; 2],
        }
    }
    fn center(raster: &Raster) -> &[u8] {
        let offset = ((raster.height / 2 * raster.width + raster.width / 2) * 4) as usize;
        &raster.rgba[offset..offset + 4]
    }

    #[test]
    fn occlusion_is_independent_of_triangle_order() {
        let mut vertices = triangle(-0.5, [0.0, 0.0, 1.0]);
        vertices.extend(triangle(0.5, [1.0, 0.0, 0.0]));
        let a = render_model(&model(vertices.clone()), 128, 128, front());
        vertices.rotate_left(3);
        let b = render_model(&model(vertices), 128, 128, front());
        assert_eq!(a.rgba, b.rgba);
        assert!(center(&a)[0] > 100);
        assert_eq!(center(&a)[2], 0);
        assert_eq!(&a.rgba[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn reversed_winding_still_renders_and_interpolates_colors() {
        let mut vertices = triangle(0.0, [1.0, 0.0, 0.0]);
        vertices[1].color = [0.0, 1.0, 0.0];
        vertices[2].color = [0.0, 0.0, 1.0];
        vertices.swap(0, 2);
        let raster = render_model(&model(vertices), 96, 96, front());
        let pixel = center(&raster);
        assert!(pixel[..3].iter().all(|&channel| channel > 20));
        assert_eq!(pixel[3], 255);
    }

    #[test]
    fn orbit_changes_visible_depth_and_keeps_z_up() {
        let mut vertices = triangle(-0.5, [0.0, 0.0, 1.0]);
        vertices.extend(triangle(0.5, [1.0, 0.0, 0.0]));
        let model = model(vertices);
        let raster = render_model(
            &model,
            96,
            96,
            Camera {
                yaw: std::f32::consts::PI,
                ..front()
            },
        );
        assert!(center(&raster)[2] > 100);
        assert_eq!(center(&raster)[0], 0);
        let filled_top = raster.rgba[..48 * 96 * 4]
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] != 0)
            .count();
        let filled_bottom = raster.rgba[48 * 96 * 4..]
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] != 0)
            .count();
        assert!(filled_bottom > filled_top);
    }

    #[test]
    fn dimensions_and_invalid_geometry_are_bounded() {
        assert_eq!(bounded_dimensions(0, 0), (1, 1));
        assert_eq!(bounded_dimensions(u32::MAX, u32::MAX), (4096, 4096));
        let mut vertices = triangle(0.0, [1.0; 3]);
        vertices[0].position[0] = f32::NAN;
        let raster = render_model(&model(vertices), 0, 0, front());
        assert_eq!(raster.rgba, [0; 4]);
    }

    #[test]
    fn camera_pan_rerasterizes_geometry_outside_the_previous_frame() {
        let vertices = [[0.0, 0.7, -0.15], [0.0, 1.0, -0.15], [0.0, 0.85, 0.15]]
            .map(|position| Vertex {
                position,
                color: [1.0, 0.0, 0.0],
            })
            .to_vec();
        let model = model(vertices);
        let mut camera = Camera {
            zoom: 8.0,
            ..front()
        };
        let hidden = render_model(&model, 128, 128, camera);
        assert!(hidden.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0));

        let fitted = render_model(&model, 128, 128, front());
        assert!(fitted.rgba.as_chunks::<4>().0.iter().any(|p| p[0] > 100));
        // The triangle was never present in the prior raster. Projection-space
        // panning brings it back, including coverage right up to the canvas edge.
        camera.pan[0] = -1.3;
        let edge = render_model(&model, 128, 128, camera);
        assert!(edge.rgba.as_chunks::<4>().0.iter().any(|p| p[0] > 100));
        assert!(
            (0..edge.height).any(|y| {
                edge.rgba[((y * edge.width + edge.width - 1) * 4 + 3) as usize] == 255
            })
        );
        camera.pan[0] = -0.85 * 0.45 * camera.zoom / 3.0_f32.sqrt();
        let centered = render_model(&model, 128, 128, camera);
        assert!(center(&centered)[0] > 100);
        assert_eq!(center(&centered)[3], 255);
    }

    #[test]
    fn rectangular_viewports_fit_without_clipping_or_distortion() {
        let model = model(triangle(0.0, [1.0; 3]));
        for (width, height) in [(320, 96), (96, 320)] {
            let raster = render_model(&model, width, height, front());
            assert_eq!((raster.width, raster.height), (width, height));
            let mut min = [width, height];
            let mut max = [0, 0];
            for (index, pixel) in raster.rgba.as_chunks::<4>().0.iter().enumerate() {
                if pixel[3] != 0 {
                    let x = index as u32 % width;
                    let y = index as u32 / width;
                    min[0] = min[0].min(x);
                    min[1] = min[1].min(y);
                    max[0] = max[0].max(x);
                    max[1] = max[1].max(y);
                }
            }
            assert!(min[0] > 0 && min[1] > 0);
            assert!(max[0] < width - 1 && max[1] < height - 1);
            assert!((max[0] - min[0]).abs_diff(max[1] - min[1]) <= 1);
        }
        assert_eq!(bounded_dimensions(8000, 2000), (4096, 1024));
        assert_eq!(bounded_dimensions(2000, 8000), (1024, 4096));
    }

    #[test]
    fn cursor_zoom_preserves_anchor_across_the_full_zoom_range() {
        let mut camera = front();
        camera.pan = [0.1, -0.2];
        let cursor = [0.3, -0.4];
        let anchor = [
            (cursor[0] - camera.pan[0]) / camera.zoom,
            (cursor[1] - camera.pan[1]) / camera.zoom,
        ];
        for factor in [10.0, 10.0, 100.0, 0.000001] {
            camera.zoom_at(factor, cursor);
            for axis in 0..2 {
                let projected = camera.pan[axis] + anchor[axis] * camera.zoom;
                assert!((projected - cursor[axis]).abs() < 0.0001);
            }
        }
        assert_eq!(camera.zoom, 0.02);
        camera.zoom_at(1.0e9, cursor);
        assert_eq!(camera.zoom, 1000.0);
    }

    fn colored_cube(subdivisions: u32) -> Model {
        let mut vertices = Vec::new();
        for u in 0..subdivisions {
            for v in 0..subdivisions {
                for axis in 0..3 {
                    let mut color = [0.0; 3];
                    color[axis] = 1.0;
                    for side in [-1.0, 1.0] {
                        let point = |u: u32, v: u32| {
                            let mut position = [0.0; 3];
                            position[axis] = side;
                            position[(axis + 1) % 3] = -1.0 + 2.0 * u as f32 / subdivisions as f32;
                            position[(axis + 2) % 3] = -1.0 + 2.0 * v as f32 / subdivisions as f32;
                            Vertex { position, color }
                        };
                        let a = point(u, v);
                        let b = point(u + 1, v);
                        let c = point(u + 1, v + 1);
                        let d = point(u, v + 1);
                        vertices.extend([a, b, c, a, c, d]);
                    }
                }
            }
        }
        model(vertices)
    }

    fn write_colored_stl(path: &Path, model: &Model) {
        use std::io::Write;
        let mut writer = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
        writer.write_all(&[0; 80]).unwrap();
        writer
            .write_all(&(model.triangle_count as u32).to_le_bytes())
            .unwrap();
        for triangle in model.vertices.as_chunks::<3>().0 {
            writer.write_all(&[0; 12]).unwrap();
            for vertex in triangle {
                for value in vertex.position {
                    writer.write_all(&value.to_le_bytes()).unwrap();
                }
            }
            let color = triangle[0].color;
            let attribute = 0x8000_u16
                | ((color[0] * 31.0) as u16) << 10
                | ((color[1] * 31.0) as u16) << 5
                | (color[2] * 31.0) as u16;
            writer.write_all(&attribute.to_le_bytes()).unwrap();
        }
        writer.flush().unwrap();
    }

    #[test]
    fn dense_closed_stl_thumbnail_preserves_surface_coverage_and_colors() {
        // 248,832 real facets exceed the former 120,000-facet thumbnail budget.
        // Interleaved, finely tessellated faces make stride sampling perforate the cube.
        let dense = colored_cube(144);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dense-colored-cube.STL");
        write_colored_stl(&path, &dense);
        let full = crate::model::load_model_full(&path).unwrap();
        let expected = render_model(&full, 192, 192, Camera::default());
        let streamed = render_file_thumbnail(&path, 192, 192).unwrap();
        assert_eq!((streamed.width, streamed.height), (192, 192));
        assert_eq!(streamed.rgba, expected.rgba);
        // Check decoded source colors too, not merely agreement between two consumers.
        let direct = render_model(&dense, 192, 192, Camera::default());
        assert_eq!(streamed.rgba, direct.rgba);
        for channel in 0..3 {
            assert!(
                streamed.rgba.as_chunks::<4>().0.iter().any(|pixel| {
                    pixel[channel] > 0
                        && pixel[(channel + 1) % 3] == 0
                        && pixel[(channel + 2) % 3] == 0
                        && pixel[3] == 255
                }),
                "missing visible face color {channel}"
            );
        }

        let coarse = render_model(&colored_cube(1), 192, 192, Camera::default());
        let stride = (dense.triangle_count as usize).div_ceil(120_000);
        let sparse = model(
            dense
                .vertices
                .as_chunks::<3>()
                .0
                .iter()
                .step_by(stride)
                .flat_map(|triangle| triangle.iter().copied())
                .collect(),
        );
        let sparse = render_model(&sparse, 192, 192, Camera::default());
        let alpha = |raster: &Raster, x: usize, y: usize| raster.rgba[(y * 192 + x) * 4 + 3];
        let mut interior = 0;
        let mut sparse_holes = 0;
        for y in 1..191 {
            for x in 1..191 {
                // Ignore the silhouette's one-pixel numerical boundary.
                if (y - 1..=y + 1).all(|yy| (x - 1..=x + 1).all(|xx| alpha(&coarse, xx, yy) == 255))
                {
                    interior += 1;
                    assert_eq!(alpha(&streamed, x, y), 255, "surface hole at {x},{y}");
                    if alpha(&sparse, x, y) == 0 {
                        sparse_holes += 1;
                    }
                }
            }
        }
        assert!(interior > 3_000);
        assert!(sparse_holes > 100, "fixture must expose sparse-facet holes");
    }
}
