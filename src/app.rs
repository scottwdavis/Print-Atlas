use crate::{
    library::{self, Catalog},
    model,
    preview::{self, Preview},
    render,
    types::*,
};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use eframe::egui::{self, Color32, RichText, TextureHandle, Vec2};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
enum TreeRow {
    Folder(PathBuf, usize),
    File(usize, usize),
}
#[derive(Clone, Copy, PartialEq)]
enum Scope {
    All,
    Favorites,
    Recent,
    Folder,
}
#[derive(Clone, Copy, PartialEq)]
enum Filter {
    All,
    Models,
    Images,
    Documents,
    Other,
    Stl,
    ThreeMf,
}
impl Filter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All files",
            Self::Models => "3D models",
            Self::Images => "Images",
            Self::Documents => "Documents",
            Self::Other => "Other files",
            Self::Stl => "STL",
            Self::ThreeMf => "3MF",
        }
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Layout {
    Compact,
    Comfortable,
    Large,
    Details,
}
#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Name,
    Newest,
    Oldest,
    Largest,
    Smallest,
}
impl Sort {
    fn label(self) -> &'static str {
        match self {
            Self::Name => "Name A-Z",
            Self::Newest => "Newest first",
            Self::Oldest => "Oldest first",
            Self::Largest => "Largest first",
            Self::Smallest => "Smallest first",
        }
    }
}
struct Asset {
    texture: TextureHandle,
}
impl Asset {
    fn from(ctx: &egui::Context, name: &str, r: Raster) -> Self {
        Self {
            texture: ctx.load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied(
                    [r.width as usize, r.height as usize],
                    &r.rgba,
                ),
                egui::TextureOptions::LINEAR,
            ),
        }
    }
}
struct Thumb {
    asset: Option<Asset>,
    text: String,
}
struct Plate {
    name: String,
    encoded: Arc<Vec<u8>>,
}
enum DetailData {
    Image(Raster),
    Text(String, bool, String),
    Pdf(Raster, String, usize, usize),
    Plates(Vec<Plate>, Raster),
    Model(Arc<Model>, Raster),
    Other,
}
enum Job {
    Thumb(FileEntry, String),
    Inspect(FileEntry, u64, usize),
    Model(FileEntry, u64),
    Orbit(Arc<Model>, u64, Camera, (u32, u32)),
    Plate(Arc<Vec<u8>>, u64),
}
enum Event {
    Scan(u64, LibraryIndex),
    Thumb(String, Result<(Option<Raster>, String), String>),
    Detail(u64, Result<DetailData, String>),
    Orbit(u64, Result<Raster, String>),
}
struct Pool {
    high: Sender<Job>,
    low: Sender<Job>,
    events: Receiver<Event>,
    send: Sender<Event>,
}
impl Pool {
    fn new(ctx: egui::Context) -> Self {
        let (hs, hr) = bounded::<Job>(8);
        let (ls, lr) = bounded::<Job>(64);
        let (es, er) = unbounded::<Event>();
        for _ in 0..2 {
            let h = hr.clone();
            let l = lr.clone();
            let out = es.clone();
            let c = ctx.clone();
            std::thread::spawn(move || {
                loop {
                    let job = crossbeam_channel::select_biased! {recv(h)->v=>match v{Ok(v)=>v,Err(_)=>break},recv(l)->v=>match v{Ok(v)=>v,Err(_)=>break}};
                    let event = match job {
                        Job::Thumb(file, key) => Event::Thumb(
                            key.clone(),
                            thumbnail(&file, &key).map_err(|e| e.to_string()),
                        ),
                        Job::Inspect(file, id, page) => {
                            Event::Detail(id, inspect_file(&file, page).map_err(|e| e.to_string()))
                        }
                        Job::Model(file, id) => Event::Detail(
                            id,
                            (|| {
                                let m = Arc::new(model::load_model_full(&file.path)?);
                                let r = render::render_model(&m, 1000, 760, Camera::default());
                                Ok(DetailData::Model(m, r))
                            })()
                            .map_err(|e: anyhow::Error| e.to_string()),
                        ),
                        Job::Orbit(m, id, camera, (width, height)) => {
                            Event::Orbit(id, Ok(render::render_model(&m, width, height, camera)))
                        }
                        Job::Plate(bytes, id) => Event::Detail(
                            id,
                            raster(&bytes, 2000)
                                .map(DetailData::Image)
                                .map_err(|e| e.to_string()),
                        ),
                    };
                    if out.send(event).is_err() {
                        break;
                    }
                    c.request_repaint();
                }
            });
        }
        Self {
            high: hs,
            low: ls,
            events: er,
            send: es,
        }
    }
}
fn raster(bytes: &[u8], side: u32) -> anyhow::Result<Raster> {
    preview::decode_image(bytes, side)
}
fn key(file: &FileEntry) -> String {
    format!(
        "{:x}",
        Sha256::digest(format!(
            "native-3-full-surfaces:{}:{}:{}",
            file.path.display(),
            file.size,
            file.mtime
        ))
    )
}
fn thumbnail(file: &FileEntry, key: &str) -> anyhow::Result<(Option<Raster>, String)> {
    if file.kind == FileKind::Model {
        let cache = library::data_dir()
            .join("native-thumbnails")
            .join(format!("{key}.png"));
        if let Ok(im) = image::open(&cache) {
            let r = im.into_rgba8();
            return Ok((
                Some(Raster {
                    width: r.width(),
                    height: r.height(),
                    rgba: r.into_raw(),
                }),
                String::new(),
            ));
        }
        let r = if file.ext == "3mf" {
            let plates = model::embedded_plates(&file.path)?;
            if let Some((_, bytes)) = plates.first() {
                raster(bytes, 420)?
            } else {
                render::render_file_thumbnail(&file.path, 420, 320)?
            }
        } else {
            render::render_file_thumbnail(&file.path, 420, 320)?
        };
        if let Some(parent) = cache.parent() {
            std::fs::create_dir_all(parent)?;
        }
        image::save_buffer(&cache, &r.rgba, r.width, r.height, image::ColorType::Rgba8)?;
        return Ok((Some(r), String::new()));
    }
    if file.kind == FileKind::Pdf || file.kind == FileKind::Other {
        return Ok((
            None,
            if file.kind == FileKind::Pdf {
                "PDF document"
            } else {
                "Open in associated app"
            }
            .into(),
        ));
    }
    match preview::load_file(&file.path, file.kind, 420)? {
        Preview::Image(r) => Ok((
            Some(r),
            if file.kind == FileKind::Blender {
                "Saved embedded thumbnail".into()
            } else {
                String::new()
            },
        )),
        Preview::Text { text, .. } => Ok((None, text.chars().take(260).collect())),
        Preview::Other if file.kind == FileKind::Blender => {
            Ok((None, "No embedded thumbnail".into()))
        }
        _ => Ok((None, String::new())),
    }
}
fn inspect_file(file: &FileEntry, page: usize) -> anyhow::Result<DetailData> {
    if file.kind == FileKind::Model {
        if file.ext == "3mf" {
            let plates = model::embedded_plates(&file.path)?;
            if !plates.is_empty() {
                let first = raster(&plates[0].1, 2000)?;
                let plates = plates
                    .into_iter()
                    .map(|(name, encoded)| Plate {
                        name,
                        encoded: Arc::new(encoded),
                    })
                    .collect();
                return Ok(DetailData::Plates(plates, first));
            }
        }
        let m = Arc::new(model::load_model_full(&file.path)?);
        let r = render::render_model(&m, 1000, 760, Camera::default());
        return Ok(DetailData::Model(m, r));
    }
    if file.kind == FileKind::Pdf {
        let p = preview::load_pdf_page(&file.path, page, 1800)?;
        return Ok(DetailData::Pdf(p.raster, p.text, p.page_count, p.page));
    }
    Ok(match preview::load_file(&file.path, file.kind, 2400)? {
        Preview::Image(r) => DetailData::Image(r),
        Preview::Text {
            text,
            truncated,
            encoding,
        } => DetailData::Text(text, truncated, encoding),
        Preview::Pdf(p) => DetailData::Pdf(p.raster, p.text, p.page_count, p.page),
        Preview::Other => DetailData::Other,
    })
}
fn size(n: u64) -> String {
    if n > 1_073_741_824 {
        format!("{:.1} GB", n as f64 / 1_073_741_824.)
    } else if n > 1_048_576 {
        format!("{:.1} MB", n as f64 / 1_048_576.)
    } else {
        format!("{} KB", n.div_ceil(1024))
    }
}
fn title(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

pub struct AtlasApp {
    catalog: Catalog,
    index: Arc<LibraryIndex>,
    by_path: HashMap<PathBuf, usize>,
    children: HashMap<PathBuf, (Vec<PathBuf>, Vec<usize>)>,
    tree_rows: Arc<Vec<TreeRow>>,
    expanded: HashSet<PathBuf>,
    pool: Pool,
    scan_generation: u64,
    scanning: bool,
    last_scan: Instant,
    scope: Scope,
    folder: PathBuf,
    query: String,
    filter: Filter,
    layout: Layout,
    sort: Sort,
    recursive: bool,
    filtered: Arc<Vec<usize>>,
    needs_filter: bool,
    thumbs: HashMap<String, Thumb>,
    thumb_order: std::collections::VecDeque<String>,
    pending: HashSet<String>,
    selected: Option<PathBuf>,
    selection: HashSet<PathBuf>,
    detail_id: u64,
    loading: bool,
    error: String,
    asset: Option<Asset>,
    plates: Vec<Plate>,
    plate: usize,
    current_model: Option<Arc<Model>>,
    camera: Camera,
    rendered_camera: Camera,
    viewport_size: (u32, u32),
    rendered_size: (u32, u32),
    orbit_pending: bool,
    view_zoom: f32,
    view_pan: Vec2,
    text: String,
    text_info: String,
    pdf_page: usize,
    pdf_pages: usize,
    pdf_text_mode: bool,
    annotation: Annotation,
    tags: String,
    dirty: bool,
    status: String,
    form: Option<(String, String)>,
    ctx: egui::Context,
}
impl AtlasApp {
    pub fn new(cc: &eframe::CreationContext<'_>, catalog: Catalog) -> Self {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        let mut style = (*cc.egui_ctx.style()).clone();
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = Color32::from_rgb(25, 29, 35);
        style.visuals.window_fill = Color32::from_rgb(34, 39, 46);
        style.visuals.selection.bg_fill = Color32::from_rgb(92, 74, 48);
        style.visuals.selection.stroke =
            egui::Stroke::new(1.0_f32, Color32::from_rgb(223, 181, 117));
        style.spacing.item_spacing = Vec2::new(8., 7.);
        cc.egui_ctx.set_style(style);
        let layout = match catalog.preferences.get("layout").and_then(|v| v.as_str()) {
            Some("compact") => Layout::Compact,
            Some("large") => Layout::Large,
            Some("details") => Layout::Details,
            _ => Layout::Comfortable,
        };
        let sort = match catalog.preferences.get("sort").and_then(|v| v.as_str()) {
            Some("newest") => Sort::Newest,
            Some("oldest") => Sort::Oldest,
            Some("largest") => Sort::Largest,
            Some("smallest") => Sort::Smallest,
            _ => Sort::Name,
        };
        let recursive = catalog
            .preferences
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let mut app = Self {
            pool: Pool::new(cc.egui_ctx.clone()),
            catalog,
            index: Arc::new(LibraryIndex::default()),
            by_path: HashMap::new(),
            children: HashMap::new(),
            tree_rows: Arc::new(Vec::new()),
            expanded: HashSet::from([PathBuf::new()]),
            scan_generation: 0,
            scanning: false,
            last_scan: Instant::now(),
            scope: Scope::All,
            folder: PathBuf::new(),
            query: String::new(),
            filter: Filter::All,
            layout,
            sort,
            recursive,
            filtered: Arc::new(Vec::new()),
            needs_filter: true,
            thumbs: HashMap::new(),
            thumb_order: std::collections::VecDeque::new(),
            pending: HashSet::new(),
            selected: None,
            selection: HashSet::new(),
            detail_id: 0,
            loading: false,
            error: String::new(),
            asset: None,
            plates: Vec::new(),
            plate: 0,
            current_model: None,
            camera: Camera::default(),
            rendered_camera: Camera::default(),
            viewport_size: (1000, 760),
            rendered_size: (1000, 760),
            orbit_pending: false,
            view_zoom: 1.,
            view_pan: Vec2::ZERO,
            text: String::new(),
            text_info: String::new(),
            pdf_page: 0,
            pdf_pages: 0,
            pdf_text_mode: false,
            annotation: Annotation::default(),
            tags: String::new(),
            dirty: false,
            status: String::new(),
            form: None,
            ctx: cc.egui_ctx.clone(),
        };
        app.rescan();
        app
    }
    fn rescan(&mut self) {
        if self.scanning {
            return;
        }
        self.scanning = true;
        self.scan_generation += 1;
        let generation = self.scan_generation;
        let root = self.catalog.root.clone();
        let out = self.pool.send.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let index = library::scan(&root, Path::new(""));
            let _ = out.send(Event::Scan(generation, index));
            ctx.request_repaint();
        });
    }
    fn rebuild_tree(&mut self) {
        fn visit(
            path: &Path,
            depth: usize,
            children: &HashMap<PathBuf, (Vec<PathBuf>, Vec<usize>)>,
            expanded: &HashSet<PathBuf>,
            out: &mut Vec<TreeRow>,
        ) {
            out.push(TreeRow::Folder(path.into(), depth));
            if !expanded.contains(path) {
                return;
            }
            if let Some((dirs, files)) = children.get(path) {
                for dir in dirs {
                    visit(dir, depth + 1, children, expanded, out);
                }
                for &file in files {
                    out.push(TreeRow::File(file, depth + 1));
                }
            }
        }
        let mut rows = Vec::new();
        visit(Path::new(""), 0, &self.children, &self.expanded, &mut rows);
        self.tree_rows = Arc::new(rows);
    }
    fn receive(&mut self) {
        while let Ok(event) = self.pool.events.try_recv() {
            match event {
                Event::Scan(id, index) if id == self.scan_generation => {
                    let index = Arc::new(index);
                    self.children.clear();
                    self.by_path.clear();
                    for (i, file) in index.files.iter().enumerate() {
                        self.by_path.insert(file.path.clone(), i);
                        self.children
                            .entry(file.folder.clone())
                            .or_default()
                            .1
                            .push(i);
                    }
                    for folder in &index.folders {
                        self.children
                            .entry(folder.parent().unwrap_or(Path::new("")).to_path_buf())
                            .or_default()
                            .0
                            .push(folder.clone());
                    }
                    for (dirs, files) in self.children.values_mut() {
                        dirs.sort_by_key(|p| title(p).to_lowercase());
                        files.sort_by_key(|&i| index.files[i].name.to_lowercase());
                    }
                    self.index = index;
                    self.rebuild_tree();
                    self.needs_filter = true;
                    self.scanning = false;
                    self.last_scan = Instant::now();
                }
                Event::Thumb(key, result) => {
                    self.pending.remove(&key);
                    let thumb = match result {
                        Ok((r, text)) => Thumb {
                            asset: r.map(|r| Asset::from(&self.ctx, &key, r)),
                            text,
                        },
                        Err(e) => Thumb {
                            asset: None,
                            text: format!("Preview unavailable: {e}"),
                        },
                    };
                    while self.thumbs.len() >= 100 {
                        if let Some(old) = self.thumb_order.pop_front() {
                            self.thumbs.remove(&old);
                        } else {
                            break;
                        }
                    }
                    self.thumb_order.push_back(key.clone());
                    self.thumbs.insert(key, thumb);
                }
                Event::Detail(id, result) if id == self.detail_id => {
                    self.loading = false;
                    self.error.clear();
                    match result {
                        Err(e) => self.error = e,
                        Ok(data) => match data {
                            DetailData::Image(r) => {
                                self.asset = Some(Asset::from(&self.ctx, "image-preview", r))
                            }
                            DetailData::Text(text, truncated, encoding) => {
                                self.text = text;
                                self.text_info = format!(
                                    "{encoding} · Read-only{}",
                                    if truncated {
                                        " · First 256 KiB only"
                                    } else {
                                        ""
                                    }
                                );
                            }
                            DetailData::Pdf(r, text, count, page) => {
                                self.asset = Some(Asset::from(&self.ctx, "pdf-page", r));
                                self.text = text;
                                self.pdf_pages = count;
                                self.pdf_page = page;
                                self.text_info = format!(
                                    "Page {} of {count} · PDF scripts and links disabled",
                                    page + 1
                                );
                            }
                            DetailData::Plates(plates, first) => {
                                self.plates = plates;
                                self.asset = Some(Asset::from(&self.ctx, "bambu-plate", first));
                            }
                            DetailData::Model(m, r) => {
                                self.current_model = Some(m);
                                self.rendered_size = (r.width, r.height);
                                self.asset = Some(Asset::from(&self.ctx, "model-preview", r));
                                self.camera = Camera::default();
                                self.rendered_camera = self.camera;
                                self.orbit_pending = false;
                            }
                            DetailData::Other => {}
                        },
                    }
                }
                Event::Orbit(id, result) if id == self.detail_id => {
                    self.orbit_pending = false;
                    match result {
                        Ok(r) => self.asset = Some(Asset::from(&self.ctx, "orbit", r)),
                        Err(e) => self.error = e,
                    }
                }
                _ => {}
            }
        }
    }
    fn filter_files(&mut self) {
        if !self.needs_filter {
            return;
        }
        let terms: Vec<_> = self
            .query
            .to_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let mut files: Vec<usize> = self
            .index
            .files
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                if self.scope == Scope::Folder
                    && !(f.folder == self.folder
                        || self.recursive && f.folder.starts_with(&self.folder))
                {
                    return None;
                }
                let ann = self.catalog.annotation_ref(&f.path);
                if self.scope == Scope::Favorites && !ann.is_some_and(|a| a.favorite) {
                    return None;
                }
                let matches = match self.filter {
                    Filter::All => true,
                    Filter::Models => matches!(f.kind, FileKind::Model | FileKind::Blender),
                    Filter::Images => f.kind == FileKind::Image,
                    Filter::Documents => matches!(f.kind, FileKind::Document | FileKind::Pdf),
                    Filter::Other => f.kind == FileKind::Other,
                    Filter::Stl => f.ext == "stl",
                    Filter::ThreeMf => f.ext == "3mf",
                };
                if !matches {
                    return None;
                }
                if terms.is_empty() {
                    return Some(i);
                }
                let hay = format!(
                    "{} {}",
                    f.relative.display(),
                    ann.map(|a| a.tags.join(" ")).unwrap_or_default()
                )
                .to_lowercase();
                if terms.iter().all(|s| hay.contains(s)) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        let order = if self.scope == Scope::Recent {
            Sort::Newest
        } else {
            self.sort
        };
        if order == Sort::Name {
            files.sort_by_cached_key(|&i| self.index.files[i].name.to_lowercase());
        } else {
            files.sort_by(|&a, &b| {
                let (a, b) = (&self.index.files[a], &self.index.files[b]);
                match order {
                    Sort::Newest => b.mtime.cmp(&a.mtime),
                    Sort::Oldest => a.mtime.cmp(&b.mtime),
                    Sort::Largest => b.size.cmp(&a.size),
                    Sort::Smallest => a.size.cmp(&b.size),
                    Sort::Name => unreachable!(),
                }
            });
        }
        self.filtered = Arc::new(files);
        self.needs_filter = false;
    }
    fn save_annotation(&mut self) {
        let Some(path) = &self.selected else {
            return;
        };
        self.annotation.tags = self
            .tags
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        match self.catalog.annotate(path, self.annotation.clone()) {
            Ok(()) => {
                self.dirty = false;
                self.status = "Tags and notes saved".into();
                self.needs_filter = true;
            }
            Err(e) => self.status = e.to_string(),
        }
    }
    fn open_file(&mut self, path: PathBuf) {
        if self.dirty {
            self.save_annotation();
            if self.dirty {
                return;
            }
        }
        let Some(&index) = self.by_path.get(&path) else {
            return;
        };
        let file = self.index.files[index].clone();
        self.selected = Some(path.clone());
        self.annotation = self.catalog.annotation(&path);
        self.tags = self.annotation.tags.join(", ");
        self.dirty = false;
        self.detail_id += 1;
        self.asset = None;
        self.plates.clear();
        self.plate = 0;
        self.text.clear();
        self.text_info.clear();
        self.pdf_page = 0;
        self.pdf_pages = 0;
        self.pdf_text_mode = false;
        self.current_model = None;
        self.camera = Camera::default();
        self.rendered_camera = self.camera;
        self.orbit_pending = false;
        self.view_zoom = 1.;
        self.view_pan = Vec2::ZERO;
        self.error.clear();
        self.loading = true;
        if self
            .pool
            .high
            .try_send(Job::Inspect(file, self.detail_id, 0))
            .is_err()
        {
            self.loading = false;
            self.error = "Preview queue busy. Select the file again shortly.".into();
        }
    }
    fn navigate(&mut self, scope: Scope, folder: PathBuf) {
        if self.dirty {
            self.save_annotation();
            if self.dirty {
                return;
            }
        }
        self.scope = scope;
        self.folder = folder;
        self.selected = None;
        self.selection.clear();
        self.needs_filter = true;
    }
    fn choose_root(&mut self) {
        if self.dirty {
            self.save_annotation();
            if self.dirty {
                return;
            }
        }
        if let Some(root) = rfd::FileDialog::new()
            .set_directory(&self.catalog.root)
            .pick_folder()
        {
            match self.catalog.set_root(&root) {
                Ok(()) => {
                    self.selected = None;
                    self.selection.clear();
                    self.expanded = HashSet::from([PathBuf::new()]);
                    self.scanning = false;
                    self.index = Arc::new(LibraryIndex::default());
                    self.children.clear();
                    self.by_path.clear();
                    self.rebuild_tree();
                    self.needs_filter = true;
                    self.rescan();
                }
                Err(e) => self.status = e.to_string(),
            }
        }
    }
    fn request_thumb(&mut self, file: &FileEntry) -> String {
        let key = key(file);
        if !self.thumbs.contains_key(&key)
            && !self.pending.contains(&key)
            && self
                .pool
                .low
                .try_send(Job::Thumb(file.clone(), key.clone()))
                .is_ok()
        {
            self.pending.insert(key.clone());
        }
        key
    }
    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("library-sidebar")
            .default_width(276.)
            .width_range(210.0..=480.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                ui.add_space(14.);
                ui.heading(RichText::new("Print Atlas").color(Color32::from_rgb(223, 181, 117)));
                ui.label(RichText::new("NATIVE MODEL LIBRARY").small().weak());
                ui.add_space(14.);
                if ui.button("Choose library folder…").clicked() {
                    self.choose_root();
                }
                ui.label(RichText::new(title(&self.catalog.root)).strong())
                    .on_hover_text(self.catalog.root.display().to_string());
                ui.separator();
                for (scope, label) in [
                    (Scope::All, "All files"),
                    (Scope::Favorites, "Favorites"),
                    (Scope::Recent, "Recently modified"),
                ] {
                    if ui
                        .selectable_label(self.scope == scope && self.selected.is_none(), label)
                        .clicked()
                    {
                        self.navigate(scope, PathBuf::new());
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("FILES & FOLDERS").small().weak());
                    if ui
                        .small_button("+")
                        .on_hover_text("Create folder")
                        .clicked()
                    {
                        self.form = Some(("New folder".into(), String::new()));
                    }
                    if ui.small_button("Refresh").clicked() {
                        self.rescan();
                    }
                });
                let rows = self.tree_rows.clone();
                let index = self.index.clone();
                let root = self.catalog.root.clone();
                let mut toggle = None;
                let mut folder_click = None;
                let mut file_click = None;
                let row_height = ui
                    .spacing()
                    .interact_size
                    .y
                    .max(ui.text_style_height(&egui::TextStyle::Button));
                // Expansion and selection must never change the scroll area's identity.
                egui::ScrollArea::vertical()
                    .id_salt(("persistent-file-tree", &self.catalog.root))
                    .auto_shrink([false, false])
                    .show_rows(ui, row_height, rows.len(), |ui, range| {
                        for row in &rows[range] {
                            match row {
                                TreeRow::Folder(path, depth) => {
                                    ui.horizontal(|ui| {
                                        ui.add_space(*depth as f32 * 13.);
                                        let open = self.expanded.contains(path);
                                        if ui.small_button(if open { "v" } else { ">" }).clicked() {
                                            toggle = Some(path.clone());
                                        }
                                        let label = if path.as_os_str().is_empty() {
                                            title(&root)
                                        } else {
                                            title(path)
                                        };
                                        if ui
                                            .selectable_label(
                                                self.scope == Scope::Folder
                                                    && self.folder == *path
                                                    && self.selected.is_none(),
                                                label,
                                            )
                                            .on_hover_text(root.join(path).display().to_string())
                                            .clicked()
                                        {
                                            folder_click = Some(path.clone());
                                        }
                                    });
                                }
                                TreeRow::File(i, depth) => {
                                    let file = &index.files[*i];
                                    ui.horizontal(|ui| {
                                        ui.add_space(*depth as f32 * 13. + 20.);
                                        let icon = match file.kind {
                                            FileKind::Model => "3D",
                                            FileKind::Blender => "BLEND",
                                            FileKind::Image => "IMG",
                                            FileKind::Pdf => "PDF",
                                            FileKind::Document => "TXT",
                                            FileKind::Other => "FILE",
                                        };
                                        if ui
                                            .selectable_label(
                                                self.selected.as_ref() == Some(&file.path),
                                                format!("[{icon}] {}", file.name),
                                            )
                                            .on_hover_text(file.relative.display().to_string())
                                            .clicked()
                                        {
                                            file_click = Some(file.path.clone());
                                        }
                                    });
                                }
                            }
                        }
                    });
                if let Some(path) = toggle {
                    if !self.expanded.remove(&path) {
                        self.expanded.insert(path);
                    }
                    self.rebuild_tree();
                }
                if let Some(path) = folder_click {
                    self.navigate(Scope::Folder, path);
                }
                if let Some(path) = file_click {
                    self.open_file(path);
                }
            });
    }
    fn save_view_preferences(&mut self) {
        self.catalog.preferences["layout"] = serde_json::json!(match self.layout {
            Layout::Compact => "compact",
            Layout::Comfortable => "comfortable",
            Layout::Large => "large",
            Layout::Details => "details",
        });
        self.catalog.preferences["sort"] = serde_json::json!(match self.sort {
            Sort::Name => "name",
            Sort::Newest => "newest",
            Sort::Oldest => "oldest",
            Sort::Largest => "largest",
            Sort::Smallest => "smallest",
        });
        self.catalog.preferences["recursive"] = serde_json::json!(self.recursive);
        if let Err(error) = self.catalog.save() {
            self.status = error.to_string();
        }
    }
    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let changed = ui
                .add(
                    egui::TextEdit::singleline(&mut self.query)
                        .id(egui::Id::new("global-search"))
                        .hint_text("Search files, paths, or tags…")
                        .desired_width((ui.available_width() - 500.).max(160.)),
                )
                .changed();
            self.needs_filter |= changed;
            egui::ComboBox::from_id_salt("format-filter")
                .selected_text(self.filter.label())
                .show_ui(ui, |ui| {
                    for f in [
                        Filter::All,
                        Filter::Models,
                        Filter::Images,
                        Filter::Documents,
                        Filter::Other,
                        Filter::Stl,
                        Filter::ThreeMf,
                    ] {
                        if ui
                            .selectable_value(&mut self.filter, f, f.label())
                            .changed()
                        {
                            self.needs_filter = true;
                        }
                    }
                });
            egui::ComboBox::from_id_salt("sort")
                .selected_text(self.sort.label())
                .show_ui(ui, |ui| {
                    for s in [
                        Sort::Name,
                        Sort::Newest,
                        Sort::Oldest,
                        Sort::Largest,
                        Sort::Smallest,
                    ] {
                        if ui.selectable_value(&mut self.sort, s, s.label()).changed() {
                            self.needs_filter = true;
                            self.save_view_preferences();
                        }
                    }
                });
            for (l, text) in [
                (Layout::Compact, "Small"),
                (Layout::Comfortable, "Grid"),
                (Layout::Large, "Large"),
                (Layout::Details, "List"),
            ] {
                if ui.selectable_value(&mut self.layout, l, text).changed() {
                    self.save_view_preferences();
                }
            }
        });
        if self.scope == Scope::Folder
            && ui
                .checkbox(&mut self.recursive, "Include subfolders")
                .changed()
        {
            self.needs_filter = true;
            self.save_view_preferences();
        }
        self.filter_files();
    }
    fn grid(&mut self, ui: &mut egui::Ui) {
        let heading = match self.scope {
            Scope::All => "All files".into(),
            Scope::Favorites => "Favorites".into(),
            Scope::Recent => "Recently modified".into(),
            Scope::Folder => {
                if self.folder.as_os_str().is_empty() {
                    title(&self.catalog.root)
                } else {
                    self.folder.display().to_string()
                }
            }
        };
        ui.heading(heading);
        ui.label(
            RichText::new("A place for every print — and everything that goes with it.").weak(),
        );
        ui.add_space(10.);
        self.toolbar(ui);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(format!("{} files", self.filtered.len()));
            if self.scanning {
                ui.spinner();
                ui.label("Scanning…");
            }
            if !self.selection.is_empty() {
                ui.label(format!("{} selected", self.selection.len()));
                if ui.button("Move selected…").clicked() {
                    self.move_selected();
                }
                if ui.button("Clear").clicked() {
                    self.selection.clear();
                }
            }
        });
        let min = match self.layout {
            Layout::Compact => 160.,
            Layout::Comfortable => 225.,
            Layout::Large => 330.,
            Layout::Details => ui.available_width(),
        };
        let columns = if self.layout == Layout::Details {
            1
        } else {
            ((ui.available_width() + 12.) / (min + 12.)).floor().max(1.) as usize
        };
        let width = (ui.available_width() - 12. * (columns - 1) as f32) / columns as f32;
        let height = if self.layout == Layout::Details {
            90.
        } else {
            width * 0.73 + 76.
        };
        let ids = self.filtered.clone();
        let index = self.index.clone();
        let rows = ids.len().div_ceil(columns);
        let mut clicked = None;
        let mut favorite = None;
        egui::ScrollArea::vertical()
            .id_salt(("file-grid", self.scope as u8, &self.folder))
            .auto_shrink([false, false])
            .show_rows(ui, height, rows, |ui, range| {
                for row in range {
                    ui.horizontal(|ui| {
                        for column in 0..columns {
                            let Some(&i) = ids.get(row * columns + column) else {
                                break;
                            };
                            let file = &index.files[i];
                            let key = self.request_thumb(file);
                            let tex = self
                                .thumbs
                                .get(&key)
                                .and_then(|t| t.asset.as_ref())
                                .map(|a| a.texture.clone());
                            let text = self
                                .thumbs
                                .get(&key)
                                .map(|t| t.text.clone())
                                .unwrap_or_else(|| "Loading preview…".into());
                            let selected = self.selection.contains(&file.path);
                            let frame = egui::Frame::new()
                                .fill(Color32::from_rgb(34, 39, 46))
                                .stroke(egui::Stroke::new(
                                    1.0_f32,
                                    if selected {
                                        Color32::from_rgb(223, 181, 117)
                                    } else {
                                        Color32::from_rgb(50, 56, 64)
                                    },
                                ))
                                .corner_radius(8.)
                                .inner_margin(9.);
                            let response = frame
                                .show(ui, |ui| {
                                    ui.with_layout(
                                        egui::Layout::top_down(egui::Align::Min),
                                        |ui| {
                                            ui.set_min_size(Vec2::new(
                                                (width - 20.).max(40.),
                                                height - 20.,
                                            ));
                                            ui.set_width((width - 20.0).max(40.0));
                                            if self.layout == Layout::Details {
                                                ui.horizontal(|ui| {
                                                    if let Some(tex) = &tex {
                                                        ui.add(
                                                            egui::Image::new((
                                                                tex.id(),
                                                                tex.size_vec2(),
                                                            ))
                                                            .fit_to_exact_size(Vec2::new(80., 60.)),
                                                        );
                                                    }
                                                    ui.vertical(|ui| {
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(&file.name).strong(),
                                                            )
                                                            .truncate(),
                                                        );
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(format!(
                                                                    "{} · {} · {}",
                                                                    file.kind.label(),
                                                                    size(file.size),
                                                                    file.folder.display()
                                                                ))
                                                                .small()
                                                                .weak(),
                                                            )
                                                            .truncate(),
                                                        );
                                                        if file.kind == FileKind::Blender {
                                                            ui.small(&text);
                                                        }
                                                    });
                                                });
                                            } else {
                                                ui.horizontal(|ui| {
                                                    ui.label(
                                                        RichText::new(if file.ext.is_empty() {
                                                            "FILE".into()
                                                        } else {
                                                            file.ext.to_uppercase()
                                                        })
                                                        .small()
                                                        .color(Color32::from_rgb(223, 181, 117)),
                                                    );
                                                    let is_favorite = self
                                                        .catalog
                                                        .annotation_ref(&file.path)
                                                        .is_some_and(|a| a.favorite);
                                                    if ui
                                                        .small_button(if is_favorite {
                                                            "★"
                                                        } else {
                                                            "☆"
                                                        })
                                                        .clicked()
                                                    {
                                                        favorite =
                                                            Some((file.path.clone(), !is_favorite));
                                                    }
                                                });
                                                let caption =
                                                    file.kind == FileKind::Blender && tex.is_some();
                                                let area = Vec2::new(
                                                    width - 20.,
                                                    (height - if caption { 105. } else { 87. })
                                                        .max(30.),
                                                );
                                                let (rect, _) = ui.allocate_exact_size(
                                                    area,
                                                    egui::Sense::hover(),
                                                );
                                                if let Some(tex) = &tex {
                                                    let scale = (rect.width() / tex.size_vec2().x)
                                                        .min(rect.height() / tex.size_vec2().y);
                                                    let image_rect = egui::Rect::from_center_size(
                                                        rect.center(),
                                                        tex.size_vec2() * scale,
                                                    );
                                                    ui.painter().image(
                                                        tex.id(),
                                                        image_rect,
                                                        egui::Rect::from_min_max(
                                                            egui::Pos2::ZERO,
                                                            egui::pos2(1., 1.),
                                                        ),
                                                        Color32::WHITE,
                                                    );
                                                } else {
                                                    ui.painter().with_clip_rect(rect).text(
                                                        rect.center(),
                                                        egui::Align2::CENTER_CENTER,
                                                        if file.kind == FileKind::Pdf {
                                                            "PDF"
                                                        } else if file.kind == FileKind::Other {
                                                            "FILE"
                                                        } else if file.kind == FileKind::Blender
                                                            && text == "No embedded thumbnail"
                                                        {
                                                            "No embedded\nthumbnail"
                                                        } else {
                                                            &text
                                                        },
                                                        egui::FontId::monospace(11.),
                                                        Color32::from_gray(150),
                                                    );
                                                }
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(&file.name).strong(),
                                                    )
                                                    .truncate(),
                                                );
                                                ui.add(
                                                    egui::Label::new(
                                                        RichText::new(format!(
                                                            "{} · {}",
                                                            size(file.size),
                                                            file.folder.display()
                                                        ))
                                                        .small()
                                                        .weak(),
                                                    )
                                                    .truncate(),
                                                );
                                                if caption {
                                                    ui.add(
                                                        egui::Label::new(
                                                            RichText::new(&text).small().weak(),
                                                        )
                                                        .truncate(),
                                                    )
                                                    .on_hover_text(&text);
                                                }
                                            }
                                        },
                                    );
                                })
                                .response
                                .interact(egui::Sense::click());
                            response.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    &file.name,
                                )
                            });
                            if response.clicked() && favorite.is_none() {
                                if ui.input(|i| i.modifiers.ctrl) {
                                    if !self.selection.remove(&file.path) {
                                        self.selection.insert(file.path.clone());
                                    }
                                } else {
                                    clicked = Some(file.path.clone());
                                }
                            }
                        }
                    });
                }
            });
        if let Some((path, value)) = favorite {
            let mut a = self.catalog.annotation(&path);
            a.favorite = value;
            if let Err(e) = self.catalog.annotate(&path, a) {
                self.status = e.to_string();
            }
            self.needs_filter = true;
        }
        if let Some(path) = clicked {
            self.open_file(path);
        }
    }
    fn move_selected(&mut self) {
        let paths: Vec<_> = self.selection.iter().cloned().collect();
        if let Some(dest) = rfd::FileDialog::new()
            .set_directory(&self.catalog.root)
            .pick_folder()
        {
            match self.catalog.move_files(&paths, &dest) {
                Ok(_) => {
                    self.selection.clear();
                    self.rescan();
                    self.status = "Files moved".into();
                }
                Err(e) => self.status = e.to_string(),
            }
        }
    }
    fn detail(&mut self, ui: &mut egui::Ui) {
        let Some(file) = self
            .selected
            .as_ref()
            .and_then(|p| self.by_path.get(p))
            .map(|&i| self.index.files[i].clone())
        else {
            ui.label("This file is no longer in the library. Refresh or choose another file.");
            if ui.button("Back to files").clicked() {
                self.selected = None;
            }
            return;
        };
        ui.horizontal(|ui| {
            if ui.button("< Back to files").clicked() {
                self.navigate(self.scope, self.folder.clone());
            }
            ui.label(RichText::new(file.kind.label()).color(Color32::from_rgb(223, 181, 117)));
        });
        ui.heading(&file.name);
        ui.label(RichText::new(file.relative.display().to_string()).weak());
        ui.separator();
        let mut new_page = None;
        ui.horizontal(|ui| {
            if file.kind == FileKind::Model {
                if ui.button("Interactive 3D").clicked() {
                    self.loading = true;
                    self.plates.clear();
                    self.detail_id += 1;
                    if self
                        .pool
                        .high
                        .try_send(Job::Model(file.clone(), self.detail_id))
                        .is_err()
                    {
                        self.loading = false;
                        self.error = "Preview queue busy; retry shortly".into();
                    }
                }
                if file.ext == "3mf" && ui.button("Saved plates").clicked() {
                    self.open_file(file.path.clone());
                }
                if self.current_model.is_some() && ui.button("Reset view").clicked() {
                    self.camera = Camera::default();
                    self.view_pan = Vec2::ZERO;
                }
            }
            if self.pdf_pages > 0 {
                if ui
                    .add_enabled(self.pdf_page > 0, egui::Button::new("< Previous"))
                    .clicked()
                {
                    new_page = Some(self.pdf_page - 1);
                }
                let mut number = self.pdf_page + 1;
                let changed = ui
                    .add(
                        egui::DragValue::new(&mut number)
                            .range(1..=self.pdf_pages)
                            .prefix("Page "),
                    )
                    .changed();
                ui.label(format!("of {}", self.pdf_pages));
                if changed {
                    new_page = Some(number - 1);
                }
                if ui
                    .add_enabled(
                        self.pdf_page + 1 < self.pdf_pages,
                        egui::Button::new("Next >"),
                    )
                    .clicked()
                {
                    new_page = Some(self.pdf_page + 1);
                }
                ui.toggle_value(&mut self.pdf_text_mode, "Page text / select & copy");
            }
            if (self.asset.is_some() || !self.plates.is_empty()) && self.current_model.is_none() {
                if ui.button("−").clicked() {
                    self.view_zoom = (self.view_zoom / 1.25).max(0.2);
                }
                if ui.button("Fit").clicked() {
                    self.view_zoom = 1.;
                    self.view_pan = Vec2::ZERO;
                }
                if ui.button("+").clicked() {
                    self.view_zoom = (self.view_zoom * 1.25).min(6.);
                }
            }
            if !self.plates.is_empty() {
                let previous = self.plate;
                egui::ComboBox::from_id_salt("plates")
                    .selected_text(&self.plates[self.plate].name)
                    .show_ui(ui, |ui| {
                        for (i, plate) in self.plates.iter().enumerate() {
                            ui.selectable_value(&mut self.plate, i, &plate.name);
                        }
                    });
                if self.plate != previous {
                    self.detail_id += 1;
                    self.loading = true;
                    self.error.clear();
                    if self
                        .pool
                        .high
                        .try_send(Job::Plate(
                            self.plates[self.plate].encoded.clone(),
                            self.detail_id,
                        ))
                        .is_err()
                    {
                        self.loading = false;
                        self.plate = previous;
                        self.error = "Preview queue busy; choose the plate again shortly.".into();
                    } else {
                        self.asset = None;
                    }
                }
            }
        });
        if let Some(page) = new_page {
            self.loading = true;
            self.detail_id += 1;
            self.pdf_page = page;
            self.error.clear();
            if self
                .pool
                .high
                .try_send(Job::Inspect(file.clone(), self.detail_id, page))
                .is_err()
            {
                self.loading = false;
                self.error = "Preview queue busy; retry shortly".into();
            }
        }
        let available = ui.available_size();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new((available.x - 304.0).max(250.0), available.y),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    if self.loading {
                        ui.horizontal(|ui| { ui.spinner(); ui.label("Reading preview…"); });
                    }
                    if !self.error.is_empty() {
                        ui.colored_label(Color32::LIGHT_RED, &self.error);
                    }
                    if (file.kind == FileKind::Document && !self.text_info.is_empty()) || self.pdf_text_mode {
                        if self.text.is_empty() && !self.loading {
                            ui.label(if file.kind == FileKind::Pdf { "This page has no extractable text." } else { "(Empty document)" });
                        } else {
                            egui::ScrollArea::both().id_salt(("document", self.detail_id)).show(ui, |ui| {
                                ui.add(egui::Label::new(RichText::new(&self.text).monospace()).selectable(true).wrap());
                            });
                        }
                    } else if let Some(texture) = self.asset.as_ref().map(|a| a.texture.clone()) {
                        if file.kind == FileKind::Blender {
                            ui.small("Saved embedded thumbnail from the Blender project · Not a live scene render");
                        }
                        let area = Vec2::new(ui.available_width(), (ui.available_height() - 75.0).max(150.0));
                        let (rect, response) = ui.allocate_exact_size(area, egui::Sense::click_and_drag());
                        ui.painter().rect_filled(rect, 8.0, Color32::from_rgb(32, 39, 47));
                        let destination = if self.current_model.is_some() {
                            // The camera projects into this exact viewport, never into a
                            // smaller image that is subsequently translated or cropped.
                            let pixels_per_point = ui.ctx().pixels_per_point();
                            self.viewport_size = render::bounded_dimensions(
                                (rect.width() * pixels_per_point).round() as u32,
                                (rect.height() * pixels_per_point).round() as u32,
                            );
                            rect
                        } else {
                            let scale = (rect.width() / texture.size_vec2().x)
                                .min(rect.height() / texture.size_vec2().y) * self.view_zoom;
                            egui::Rect::from_center_size(rect.center() + self.view_pan, texture.size_vec2() * scale)
                        };
                        ui.painter().with_clip_rect(rect).image(texture.id(), destination,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), Color32::WHITE);
                        if self.current_model.is_some() {
                            if response.dragged() {
                                let delta = ui.input(|i| i.pointer.delta());
                                if ui.input(|i| i.pointer.secondary_down() || i.modifiers.shift) {
                                    let side = rect.width().min(rect.height()).max(1.0);
                                    self.camera.pan[0] += delta.x / side;
                                    self.camera.pan[1] += delta.y / side;
                                } else {
                                    self.camera.yaw += delta.x * 0.009;
                                    self.camera.pitch = (self.camera.pitch + delta.y * 0.009).clamp(-1.45, 1.45);
                                }
                            }
                            if response.hovered() {
                                let delta = ui.input(|i| i.smooth_scroll_delta.y);
                                if delta != 0.0 {
                                    let cursor = ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.center());
                                    let offset = (cursor - rect.center()) / rect.width().min(rect.height()).max(1.0);
                                    self.camera.zoom_at((delta * 0.002).exp(), [offset.x, offset.y]);
                                }
                            }
                            if response.double_clicked() {
                                self.camera = Camera::default();
                                self.view_pan = Vec2::ZERO;
                            }
                            ui.small("Drag to orbit · Scroll to zoom · Right-drag to pan");
                        } else {
                            if response.dragged() { self.view_pan += ui.input(|i| i.pointer.delta()); }
                            ui.small("Drag to pan · Use + / − to zoom");
                        }
                    } else if !self.loading && self.error.is_empty() {
                        if file.kind == FileKind::Blender {
                            ui.label("No embedded thumbnail saved in this Blender project.");
                            ui.small("Only the saved thumbnail is read. Blender is not launched and the scene is not evaluated.");
                        } else {
                            ui.label("No inline preview for this format. Open the original in its associated application.");
                        }
                    }
                    if let Some(m) = &self.current_model {
                        ui.label(format!("{:.1} × {:.1} × {:.1} mm · {} source triangles · {} displayed",
                            m.bounds_max[0]-m.bounds_min[0], m.bounds_max[1]-m.bounds_min[1],
                            m.bounds_max[2]-m.bounds_min[2], m.triangle_count, m.vertices.len()/3));
                        for warning in &m.warnings { ui.small(warning); }
                    } else { ui.small(&self.text_info); }
                },
            );
            ui.separator();
            ui.allocate_ui_with_layout(Vec2::new(280.0, available.y), egui::Layout::top_down(egui::Align::Min), |ui| {
                egui::ScrollArea::vertical().id_salt("inline-file-details").show(ui, |ui| {
                    ui.heading("File details");
                    ui.label(format!("{} · {}", file.ext.to_uppercase(), size(file.size)));
                    if ui.checkbox(&mut self.annotation.favorite, "Favorite").changed() {
                        if let Err(e) = self.catalog.annotate(&file.path, self.annotation.clone()) {
                            self.status = e.to_string();
                        }
                        self.needs_filter = true;
                    }
                    if ui.button("Open in associated application ↗").clicked()
                        && let Err(e) = open::that(&file.path) { self.status = e.to_string(); }
                    if ui.button("Show in Explorer ↗").clicked() {
                        #[cfg(windows)]
                        if let Err(e) = std::process::Command::new("explorer.exe")
                            .arg(format!("/select,{}", file.path.display())).spawn() { self.status = e.to_string(); }
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Rename…").clicked() {
                            self.form = Some(("Rename".into(), file.name.clone()));
                        }
                        if ui.button("Move…").clicked()
                            && let Some(destination) = rfd::FileDialog::new().set_directory(&self.catalog.root).pick_folder() {
                            match self.catalog.move_files(std::slice::from_ref(&file.path), &destination) {
                                Ok(paths) => { self.selected = paths.first().cloned(); self.rescan(); }
                                Err(e) => self.status = e.to_string(),
                            }
                        }
                    });
                    ui.separator();
                    ui.label("Tags (comma separated)");
                    self.dirty |= ui.text_edit_singleline(&mut self.tags).changed();
                    ui.label("Notes");
                    self.dirty |= ui.add(egui::TextEdit::multiline(&mut self.annotation.notes)
                        .desired_width(260.0).desired_rows(8)).changed();
                    if ui.button(if self.dirty { "Save tags & notes *" } else { "Save tags & notes" }).clicked() {
                        self.save_annotation();
                    }
                    ui.small("Annotations are stored locally, not inside the original file.");
                });
            });
        });
        if let Some(m) = &self.current_model
            && (self.camera != self.rendered_camera || self.viewport_size != self.rendered_size)
            && !self.orbit_pending
            && self
                .pool
                .high
                .try_send(Job::Orbit(
                    m.clone(),
                    self.detail_id,
                    self.camera,
                    self.viewport_size,
                ))
                .is_ok()
        {
            self.rendered_camera = self.camera;
            self.rendered_size = self.viewport_size;
            self.orbit_pending = true;
        }
    }
    fn action_form(&mut self, ui: &mut egui::Ui) {
        let Some((label, mut name)) = self.form.clone() else {
            return;
        };
        let mut apply = false;
        let mut cancel = false;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.strong(&label);
                ui.text_edit_singleline(&mut name);
                apply = ui.button("Apply").clicked();
                cancel = ui.button("Cancel").clicked();
            });
        });
        self.form = Some((label.clone(), name.clone()));
        if cancel {
            self.form = None;
        }
        if apply {
            let result = if label == "Rename" {
                if let Some(source) = self.selected.clone() {
                    self.catalog.rename(&source, &name)
                } else {
                    Err(anyhow::anyhow!("Select a file first"))
                }
            } else {
                let parent = if self.scope == Scope::Folder {
                    self.catalog.root.join(&self.folder)
                } else {
                    self.catalog.root.clone()
                };
                self.catalog.create_folder(&parent, &name)
            };
            match result {
                Ok(path) => {
                    self.form = None;
                    if label == "Rename" {
                        self.selected = Some(path);
                    }
                    self.rescan();
                    self.status = "File operation completed".into();
                }
                Err(e) => self.status = e.to_string(),
            }
        }
    }
}
impl eframe::App for AtlasApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive();
        if ctx.input(|i| i.viewport().close_requested()) && self.dirty {
            self.save_annotation();
            if self.dirty {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        if ctx.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::K)) {
            self.navigate(self.scope, self.folder.clone());
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("global-search")));
        }
        if self.last_scan.elapsed() > Duration::from_secs(20) {
            self.rescan();
        }
        ctx.request_repaint_after(Duration::from_millis(if self.orbit_pending {
            16
        } else {
            1000
        }));
        if ctx.input(|i| i.key_pressed(egui::Key::F5)) {
            self.rescan();
        }
        egui::TopBottomPanel::bottom("status-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.small(format!(
                    "{} files · {} folders · Rust native",
                    self.index.files.len(),
                    self.index.folders.len()
                ));
                if !self.status.is_empty() {
                    ui.separator();
                    ui.small(&self.status);
                }
                if !self.index.errors.is_empty() {
                    ui.colored_label(
                        Color32::LIGHT_RED,
                        format!("{} scan issues", self.index.errors.len()),
                    )
                    .on_hover_text(self.index.errors.join("\n"));
                }
            });
        });
        self.sidebar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| {
            self.action_form(ui);
            if self.selected.is_some() {
                self.detail(ui);
            } else {
                self.grid(ui);
            }
        });
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.dirty {
            self.save_annotation();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn valid_first_plate_remains_viewable_when_an_unselected_plate_is_corrupt() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plates.3mf");
        let mut png = Cursor::new(Vec::new());
        let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]));
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("Metadata/plate_1.png", options).unwrap();
        zip.write_all(png.get_ref()).unwrap();
        zip.start_file("Metadata/plate_2.png", options).unwrap();
        zip.write_all(&png.get_ref()[..24]).unwrap();
        zip.finish().unwrap();
        let entry = FileEntry {
            path,
            relative: "plates.3mf".into(),
            name: "plates.3mf".into(),
            folder: PathBuf::new(),
            ext: "3mf".into(),
            kind: FileKind::Model,
            size: 0,
            mtime: 0,
        };
        let DetailData::Plates(plates, first) = inspect_file(&entry, 0).unwrap() else {
            panic!("Saved plates should be available without loading a mesh");
        };
        assert_eq!((first.width, first.height), (4, 4));
        assert_eq!(&first.rgba[..4], &[255, 0, 0, 255]);
        assert!(raster(&plates[1].encoded, 2000).is_err());
    }
}
