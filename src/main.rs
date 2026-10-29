#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod app;
mod blend;
mod library;
mod model;
mod preview;
mod render;
mod types;

use std::path::{Path, PathBuf};
fn default_root() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        for directory in exe.ancestors().skip(1).take(4) {
            if std::fs::read_to_string(directory.join("Cargo.toml"))
                .is_ok_and(|s| s.contains("name = \"print-atlas\""))
            {
                return directory.parent().unwrap_or(directory).to_path_buf();
            }
        }
    }
    let documents = dirs::document_dir()
        .or_else(|| dirs::home_dir().map(|p| p.join("Documents")))
        .unwrap_or_else(|| PathBuf::from("."));
    let printing = documents.join("3D Printing");
    if printing.is_dir() {
        printing
    } else {
        documents
    }
}
fn inspect(path: &Path) -> anyhow::Result<()> {
    use types::FileKind;
    let path = std::fs::canonicalize(path)?;
    let now = std::time::Instant::now();
    let value = match library::classify(&path) {
        FileKind::Model => {
            let m = model::load_model_full(&path)?;
            serde_json::json!({"kind":"model","triangles":m.triangle_count,"display_triangles":m.vertices.len()/3,"min":m.bounds_min,"max":m.bounds_max,"palette":m.palette,"warnings":m.warnings})
        }
        kind => match preview::load_file(&path, kind, 1400)? {
            preview::Preview::Image(r) => {
                if kind == FileKind::Blender {
                    serde_json::json!({"kind":"blender","preview":"image","source":"saved embedded thumbnail","width":r.width,"height":r.height})
                } else {
                    serde_json::json!({"kind":"image","width":r.width,"height":r.height})
                }
            }
            preview::Preview::Text {
                text,
                truncated,
                encoding,
            } => {
                serde_json::json!({"kind":"document","text":text,"truncated":truncated,"encoding":encoding})
            }
            preview::Preview::Pdf(p) => {
                serde_json::json!({"kind":"pdf","pages":p.page_count,"width":p.raster.width,"height":p.raster.height,"text":p.text})
            }
            preview::Preview::Other if kind == FileKind::Blender => {
                serde_json::json!({"kind":"blender","preview":null,"description":"No embedded thumbnail saved in this Blender project."})
            }
            preview::Preview::Other => serde_json::json!({"kind":"other"}),
        },
    };
    println!(
        "{}",
        serde_json::json!({"file":path,"elapsed_ms":now.elapsed().as_millis(),"preview":value})
    );
    Ok(())
}
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "--inspect") {
        let file = args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("Usage: print-atlas --inspect FILE"))?;
        return inspect(Path::new(file));
    }
    let root = if args.first().is_some_and(|a| a == "--root") {
        args.get(1).map(PathBuf::from).unwrap_or_else(default_root)
    } else {
        default_root()
    };
    let catalog = library::load(&root)?;
    let icon = image::load_from_memory(include_bytes!("../icon.png"))?.into_rgba8();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Print Atlas")
            .with_inner_size([1440.0, 960.0])
            .with_min_inner_size([1000.0, 680.0])
            .with_icon(eframe::egui::IconData {
                width: icon.width(),
                height: icon.height(),
                rgba: icon.into_raw(),
            }),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "Print Atlas",
        options,
        Box::new(move |cc| Ok(Box::new(app::AtlasApp::new(cc, catalog)))),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))
}
