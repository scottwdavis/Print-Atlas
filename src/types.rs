use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    Model,
    Blender,
    Image,
    Document,
    Pdf,
    Other,
}
impl FileKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Model => "3D model",
            Self::Blender => "Blender project",
            Self::Image => "Image",
            Self::Document => "Text document",
            Self::Pdf => "PDF",
            Self::Other => "File",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Annotation {
    pub favorite: bool,
    pub tags: Vec<String>,
    pub notes: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub name: String,
    pub folder: PathBuf,
    pub ext: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime: u64,
}
#[derive(Debug, Clone, Default)]
pub struct LibraryIndex {
    pub files: Vec<FileEntry>,
    pub folders: Vec<PathBuf>,
    pub errors: Vec<String>,
}
#[derive(Debug, Clone, Copy)]
pub struct Vertex {
    pub position: [f32; 3],
    pub color: [f32; 3],
}
#[derive(Debug, Clone)]
pub struct Model {
    pub vertices: Vec<Vertex>,
    pub triangle_count: u64,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
    pub palette: Vec<[u8; 3]>,
    pub warnings: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    pub yaw: f32,
    pub pitch: f32,
    pub zoom: f32,
    /// Screen-right/down offset in units of the viewport's shorter side.
    pub pan: [f32; 2],
}
impl Default for Camera {
    fn default() -> Self {
        Self {
            yaw: -0.95,
            pitch: 0.55,
            zoom: 1.0,
            pan: [0.0; 2],
        }
    }
}

impl Camera {
    /// Keep the model point under the cursor fixed while changing projection scale.
    /// `cursor` is relative to viewport center, in shorter-side units (Y down).
    pub fn zoom_at(&mut self, factor: f32, cursor: [f32; 2]) {
        let next = (self.zoom * factor).clamp(0.02, 1000.0);
        let ratio = next / self.zoom;
        for (pan, cursor) in self.pan.iter_mut().zip(cursor) {
            *pan = cursor + (*pan - cursor) * ratio;
        }
        self.zoom = next;
    }
}
