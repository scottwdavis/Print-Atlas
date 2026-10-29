use crate::types::{Camera, Model, Raster};
use glam::Vec3;

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
    let (width, height) = bounded_dimensions(width, height);
    let count = width as usize * height as usize;
    let mut raster = Raster {
        width,
        height,
        rgba: vec![0; count * 4],
    };
    if model.vertices.len() < 3 {
        return raster;
    }
    let mut center = [0.0_f64; 3];
    let mut radius_squared = 0.0;
    for (axis, c) in center.iter_mut().enumerate() {
        let lo = f64::from(model.bounds_min[axis]);
        let hi = f64::from(model.bounds_max[axis]);
        if !lo.is_finite() || !hi.is_finite() || hi < lo {
            return raster;
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
    let mut depth = vec![f32::NEG_INFINITY; count];
    for triangle in model.vertices.as_chunks::<3>().0 {
        let mut positions = [Vec3::ZERO; 3];
        let mut screen = [Vec3::ZERO; 3];
        let mut colors = [Vec3::ZERO; 3];
        let mut valid = true;
        for i in 0..3 {
            let p = triangle[i].position;
            positions[i] = Vec3::new(
                ((f64::from(p[0]) - center[0]) / radius) as f32,
                ((f64::from(p[1]) - center[1]) / radius) as f32,
                ((f64::from(p[2]) - center[2]) / radius) as f32,
            );
            valid &= positions[i].is_finite();
            screen[i] = Vec3::new(
                width as f32 * 0.5 + pan[0] + positions[i].dot(right) * scale,
                height as f32 * 0.5 + pan[1] - positions[i].dot(up) * scale,
                positions[i].dot(toward),
            );
            colors[i] = Vec3::from_array(triangle[i].color).clamp(Vec3::ZERO, Vec3::ONE);
            valid &= colors[i].is_finite();
        }
        if !valid {
            continue;
        }
        let normal = (positions[1] - positions[0]).cross(positions[2] - positions[0]);
        if !normal.is_finite() || normal.length_squared() == 0.0 {
            continue;
        }
        // Two-sided shading intentionally keeps open STL surfaces and mirrored parts visible.
        let mut normal = normal.normalize();
        if normal.dot(toward) < 0.0 {
            normal = -normal;
        }
        let shade = 0.38 + 0.62 * normal.dot(light).max(0.0);
        draw_triangle(&mut raster, &mut depth, screen, colors, shade);
    }
    raster
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
}
