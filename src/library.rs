//! Filesystem indexing and non-destructive library operations, independent of the UI.
use crate::types::{Annotation, FileEntry, FileKind, LibraryIndex};
use anyhow::{Context, Result, bail, ensure};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Print Atlas")
}

pub fn classify(path: &Path) -> FileKind {
    let ext = extension(path);
    match ext.as_str() {
        "stl" | "3mf" => FileKind::Model,
        "blend" => FileKind::Blender,
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "svg" | "ico" | "avif" | "tif"
        | "tiff" => FileKind::Image,
        "pdf" => FileKind::Pdf,
        "txt" | "md" | "markdown" | "csv" | "tsv" | "json" | "jsonl" | "xml" | "yaml" | "yml"
        | "ini" | "cfg" | "conf" | "config" | "log" | "gcode" | "py" | "js" | "cjs" | "mjs"
        | "ts" | "tsx" | "jsx" | "css" | "html" | "htm" | "scad" | "sql" | "sh" | "bat" | "ps1"
        | "toml" | "rst" => FileKind::Document,
        "" if path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name.eq_ignore_ascii_case("readme") || name.eq_ignore_ascii_case("license")
        }) =>
        {
            FileKind::Document
        }
        _ => FileKind::Other,
    }
}

fn extension(path: &Path) -> String {
    path.extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}

// Electron used lowercase absolute Windows paths. Normalize both slash styles and
// Win32 verbatim prefixes, even on non-Windows test hosts.
fn key(path: &Path) -> String {
    key_text(&path.to_string_lossy())
}

fn key_text(path: &str) -> String {
    let slashes = path.replace('\\', "/");
    let path = if let Some(rest) = slashes.strip_prefix("//?/UNC/") {
        format!("//{rest}")
    } else if let Some(rest) = slashes.strip_prefix("//?/") {
        rest.to_owned()
    } else {
        slashes
    };
    let mut result = path.to_lowercase();
    while result.len() > 1 && result.ends_with('/') && !result.ends_with(":/") {
        result.pop();
    }
    result
}

fn inside(root: &Path, path: &Path) -> bool {
    let root = key(root);
    let path = key(path);
    path == root
        || (path.starts_with(&root)
            && (root.ends_with('/') || path.as_bytes().get(root.len()) == Some(&b'/')))
}

fn is_link(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Includes junctions, mount-point reparse aliases and cloud placeholders.
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn ignored_component(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        ".git"
            | "target"
            | "node_modules"
            | "__macosx"
            | ".cache"
            | "cache"
            | "caches"
            | "$recycle.bin"
            | "system volume information"
    )
}

fn app_source(directory: &Path) -> bool {
    // Only inspect the small manifest, never an arbitrarily large unrelated file.
    let Ok(file) = File::open(directory.join("Cargo.toml")) else {
        return false;
    };
    let mut manifest = String::new();
    if file.take(65_537).read_to_string(&mut manifest).is_err() || manifest.len() > 65_536 {
        return false;
    }
    let mut package = false;
    manifest.lines().any(|line| {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            package = line == "[package]";
            return false;
        }
        package
            && line.split_once('=').is_some_and(|(name, value)| {
                name.trim() == "name"
                    && matches!(
                        value.trim().trim_matches(['\'', '"']),
                        "print-atlas" | "print_atlas"
                    )
            })
    })
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                ensure!(normalized.pop(), "Path escapes its filesystem root");
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn exclusions(exclude: &Path) -> Vec<PathBuf> {
    let mut paths = vec![data_dir()];
    #[cfg(debug_assertions)]
    paths.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    if !exclude.as_os_str().is_empty() {
        paths.push(exclude.to_owned());
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        paths.push(parent.to_owned());
    }
    paths
        .into_iter()
        .filter_map(|path| absolute(&path).ok())
        .map(|path| fs::canonicalize(&path).unwrap_or(path))
        .collect()
}

fn ignored(root: &Path, path: &Path, excluded: &[PathBuf]) -> bool {
    excluded.iter().any(|exclude| inside(exclude, path))
        || path.strip_prefix(root).is_ok_and(|relative| {
            relative
                .components()
                .any(|part| ignored_component(&part.as_os_str().to_string_lossy()))
        })
}

/// Synchronous by design: call this on a worker and publish its complete result.
/// Neither directory symlinks nor Windows reparse points are traversed.
pub fn scan(root: &Path, exclude: &Path) -> LibraryIndex {
    let mut index = LibraryIndex::default();
    let root = match absolute(root).and_then(|root| {
        let meta = fs::symlink_metadata(&root)?;
        ensure!(
            meta.is_dir() && !is_link(&meta),
            "Library root must be a real directory, not a link"
        );
        let canonical = fs::canonicalize(&root)?;
        ensure!(
            key(&canonical) == key(&root),
            "Library root contains a linked directory"
        );
        Ok(canonical)
    }) {
        Ok(root) => root,
        Err(error) => {
            index.errors.push(format!("{}: {error:#}", root.display()));
            return index;
        }
    };
    let excluded = exclusions(exclude);
    if ignored(&root, &root, &excluded) || app_source(&root) {
        return index;
    }
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        // Recheck queued directories: an external process may have replaced one
        // with a junction since its parent was enumerated.
        let directory_valid = fs::symlink_metadata(&directory).and_then(|metadata| {
            if !metadata.is_dir() || is_link(&metadata) {
                return Ok(false);
            }
            fs::canonicalize(&directory).map(|canonical| key(&canonical) == key(&directory))
        });
        match directory_valid {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                scan_error(&mut index, &directory, error);
                continue;
            }
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                scan_error(&mut index, &directory, error);
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    scan_error(&mut index, &directory, error);
                    continue;
                }
            };
            let path = entry.path();
            if ignored(&root, &path, &excluded) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    scan_error(&mut index, &path, error);
                    continue;
                }
            };
            if is_link(&metadata) {
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(path) => path,
                Err(error) => {
                    scan_error(&mut index, &path, error);
                    continue;
                }
            };
            if key(&canonical) != key(&path) || !inside(&root, &canonical) {
                continue;
            }
            let relative = path
                .strip_prefix(&root)
                .expect("entry belongs to scanned root")
                .to_owned();
            if metadata.is_dir() {
                if app_source(&path) {
                    continue;
                }
                index.folders.push(relative);
                pending.push(path);
            } else if metadata.is_file() {
                index.files.push(FileEntry {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    folder: relative.parent().unwrap_or(Path::new("")).to_owned(),
                    ext: extension(&path),
                    kind: classify(&path),
                    size: metadata.len(),
                    mtime: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                        .map(|time| time.as_millis().min(u64::MAX as u128) as u64)
                        .unwrap_or(0),
                    path,
                    relative,
                });
            }
        }
    }
    index
        .files
        .sort_unstable_by(|a, b| a.relative.cmp(&b.relative));
    index.folders.sort_unstable();
    index
}

fn scan_error(index: &mut LibraryIndex, path: &Path, error: io::Error) {
    if error.kind() != io::ErrorKind::NotFound && index.errors.len() < 100 {
        index.errors.push(format!("{}: {error}", path.display()));
    }
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub root: PathBuf,
    pub metadata: HashMap<String, Annotation>,
    pub preferences: Value,
    state_file: PathBuf,
    // Preserve future/legacy fields, including the optional Electron index. That
    // index is never trusted for scanning or deserialized into integer mtimes.
    extra: Map<String, Value>,
    annotation_extra: HashMap<String, Map<String, Value>>,
}

pub fn load(default_root: &Path) -> Result<Catalog> {
    load_from(default_root, &data_dir().join("library.json"))
}

fn load_from(default_root: &Path, state_file: &Path) -> Result<Catalog> {
    let mut catalog = Catalog {
        root: absolute(default_root)?,
        metadata: HashMap::new(),
        preferences: Value::Object(Map::new()),
        state_file: state_file.to_owned(),
        extra: Map::new(),
        annotation_extra: HashMap::new(),
    };
    let file = match File::open(state_file) {
        Ok(file) => Some(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("Cannot read {}", state_file.display()));
        }
    };
    if let Some(file) = file {
        let value: Value =
            serde_json::from_reader(io::BufReader::new(file)).with_context(|| {
                format!(
                    "Saved library is invalid; it has not been overwritten: {}",
                    state_file.display()
                )
            })?;
        let Value::Object(mut saved) = value else {
            bail!("Saved library must be a JSON object; it has not been overwritten");
        };
        if let Some(Value::String(root)) = saved.remove("root") {
            let root = PathBuf::from(root);
            if root.is_absolute() {
                catalog.root = root;
            }
        }
        if let Some(preferences) = saved.remove("preferences") {
            catalog.preferences = preferences;
        }
        if let Some(Value::Object(metadata)) = saved.remove("metadata") {
            for (path, value) in metadata {
                let normalized = key_text(&path);
                let (annotation, extra) = decode_annotation(value);
                if let Some(previous) = catalog.metadata.get_mut(&normalized) {
                    merge_annotation(previous, &annotation);
                } else {
                    catalog.metadata.insert(normalized.clone(), annotation);
                }
                let unknown = catalog.annotation_extra.entry(normalized).or_default();
                for (name, value) in extra {
                    unknown.entry(name).or_insert(value);
                }
            }
        }
        catalog.extra = saved;
        // Resolve a root selected through a link once, as Electron chooseRoot did.
        // Subsequent mutations require that this canonical root has not changed.
        catalog.root = fs::canonicalize(&catalog.root).unwrap_or(catalog.root);
        catalog
            .save()
            .context("Cannot atomically migrate the saved library")?;
    } else {
        catalog.root = fs::canonicalize(&catalog.root).unwrap_or(catalog.root);
    }
    Ok(catalog)
}

fn decode_annotation(value: Value) -> (Annotation, Map<String, Value>) {
    let mut fields = match value {
        Value::Object(fields) => fields,
        _ => Map::new(),
    };
    let favorite = fields
        .remove("favorite")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let tags = fields
        .remove("tags")
        .and_then(|value| match value {
            Value::Array(values) => Some(
                values
                    .into_iter()
                    .filter_map(|value| match value {
                        Value::String(tag) => Some(tag),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    let notes = match fields.remove("notes") {
        Some(Value::String(notes)) => notes,
        _ => String::new(),
    };
    (
        clean_annotation(Annotation {
            favorite,
            tags,
            notes,
        }),
        fields,
    )
}

fn clean_annotation(mut annotation: Annotation) -> Annotation {
    let mut seen = HashSet::new();
    annotation.tags = annotation
        .tags
        .into_iter()
        .filter_map(|tag| {
            let tag = tag.trim();
            (!tag.is_empty() && seen.insert(tag.to_owned())).then(|| tag.to_owned())
        })
        .collect();
    annotation
}

fn merge_annotation(destination: &mut Annotation, source: &Annotation) {
    destination.favorite |= source.favorite;
    for tag in &source.tags {
        if !destination.tags.contains(tag) {
            destination.tags.push(tag.clone());
        }
    }
    if !source.notes.is_empty() && destination.notes != source.notes {
        if !destination.notes.is_empty() {
            destination.notes.push_str("\n\n");
        }
        destination.notes.push_str(&source.notes);
    }
}

impl Catalog {
    pub fn annotation(&self, path: &Path) -> Annotation {
        self.metadata.get(&key(path)).cloned().unwrap_or_default()
    }

    pub fn annotation_ref(&self, path: &Path) -> Option<&Annotation> {
        self.metadata.get(&key(path))
    }

    pub fn annotate(&mut self, path: &Path, value: Annotation) -> Result<()> {
        let path = self.file(path)?;
        let name = key(&path);
        let previous = self.metadata.insert(name.clone(), clean_annotation(value));
        if let Err(error) = self.save() {
            restore(&mut self.metadata, name, previous);
            return Err(error);
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let parent = self
            .state_file
            .parent()
            .context("Library state has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create {}", parent.display()))?;
        let (temporary, mut file) = temporary_file(parent)?;
        let cleanup = RemoveOnDrop(temporary.clone());
        {
            let mut writer = BufWriter::new(&mut file);
            serde_json::to_writer(&mut writer, &SavedCatalog(self))
                .context("Cannot serialize library state")?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        file.sync_all().context("Cannot flush library state")?;
        drop(file);
        atomic_replace(&temporary, &self.state_file)
            .with_context(|| format!("Cannot atomically replace {}", self.state_file.display()))?;
        drop(cleanup);
        #[cfg(unix)]
        File::open(parent)?
            .sync_all()
            .context("Cannot flush library state directory")?;
        Ok(())
    }

    pub fn set_root(&mut self, root: &Path) -> Result<()> {
        ensure!(root.is_absolute(), "Choose an absolute folder");
        let canonical = fs::canonicalize(root).context("Cannot open selected library folder")?;
        ensure!(canonical.is_dir(), "Choose a folder");
        let previous = std::mem::replace(&mut self.root, canonical);
        if let Err(error) = self.save() {
            self.root = previous;
            return Err(error);
        }
        Ok(())
    }

    pub fn rename(&mut self, source: &Path, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        let source = self.file(source)?;
        let extension = source
            .extension()
            .map(|ext| format!(".{}", ext.to_string_lossy()))
            .unwrap_or_default();
        let mut name = name.to_owned();
        if !name.to_lowercase().ends_with(&extension.to_lowercase()) {
            name.push_str(&extension);
        }
        validate_name(&name)?;
        let destination = source
            .parent()
            .context("Source has no parent folder")?
            .join(name);
        if key(&source) == key(&destination) {
            ensure!(
                source == destination,
                "Choose a name that differs by more than letter casing"
            );
            return Ok(source);
        }
        self.transfer(vec![Transfer::new(source, destination.clone())?])?;
        Ok(destination)
    }

    pub fn move_files(&mut self, sources: &[PathBuf], destination: &Path) -> Result<Vec<PathBuf>> {
        ensure!(
            !sources.is_empty() && sources.len() <= 10_000,
            "Select between one and 10,000 files"
        );
        let folder = self.directory(destination)?;
        let mut seen = HashSet::with_capacity(sources.len());
        let mut plans = Vec::with_capacity(sources.len());
        let mut results = Vec::with_capacity(sources.len());
        for source in sources {
            let source = self.file(source)?;
            if !seen.insert(key(&source)) {
                continue;
            }
            let destination = folder.join(source.file_name().context("Source has no filename")?);
            results.push(destination.clone());
            if key(&source) != key(&destination) {
                plans.push(Transfer::new(source, destination)?);
            }
        }
        self.transfer(plans)?;
        Ok(results)
    }

    pub fn create_folder(&self, parent: &Path, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        ensure!(
            !ignored_component(name),
            "This folder name is excluded from the library"
        );
        let parent = self.directory(parent)?;
        let destination = parent.join(name);
        absent(&destination)?;
        fs::create_dir(&destination).with_context(|| {
            format!(
                "Cannot create {} (existing entries are never replaced)",
                destination.display()
            )
        })?;
        Ok(destination)
    }

    fn canonical_root(&self) -> Result<PathBuf> {
        let metadata = fs::symlink_metadata(&self.root).context("Cannot access library root")?;
        ensure!(
            metadata.is_dir() && !is_link(&metadata),
            "Library root is no longer a real folder; choose it again"
        );
        let canonical = fs::canonicalize(&self.root)?;
        ensure!(
            key(&canonical) == key(&self.root),
            "Library root has changed; choose it again"
        );
        Ok(canonical)
    }

    fn validate_path(&self, path: &Path) -> Result<PathBuf> {
        let root = self.canonical_root()?;
        let path = absolute(
            if path.is_absolute() {
                path.to_owned()
            } else {
                root.join(path)
            }
            .as_path(),
        )?;
        let excluded = exclusions(Path::new(""));
        ensure!(
            inside(&root, &path) && !ignored(&root, &path, &excluded),
            "This path is outside the library or excluded"
        );
        // Reject every linked ancestor, including aliases whose targets happen to
        // remain inside the root. Scanner and mutation eligibility stay identical.
        let relative = path
            .strip_prefix(&root)
            .context("Path spelling does not match library root")?;
        let mut ancestor = root.clone();
        ensure!(
            !app_source(&ancestor),
            "Application source files are excluded"
        );
        for component in relative.components() {
            ensure!(
                matches!(component, Component::Normal(_)),
                "Use a path inside the library"
            );
            ancestor.push(component);
            let metadata = fs::symlink_metadata(&ancestor)?;
            ensure!(
                !is_link(&metadata),
                "Symbolic links and junctions are not allowed"
            );
            if metadata.is_dir() {
                ensure!(
                    !app_source(&ancestor),
                    "Application source files are excluded"
                );
            }
        }
        let canonical = fs::canonicalize(&path)?;
        ensure!(
            inside(&root, &canonical) && key(&canonical) == key(&path),
            "Linked paths are not allowed"
        );
        Ok(canonical)
    }

    fn directory(&self, path: &Path) -> Result<PathBuf> {
        let path = self.validate_path(path)?;
        ensure!(
            fs::symlink_metadata(&path)?.is_dir(),
            "Destination must be an existing real folder"
        );
        Ok(path)
    }

    fn file(&self, path: &Path) -> Result<PathBuf> {
        let path = self.validate_path(path)?;
        ensure!(
            fs::symlink_metadata(&path)?.is_file(),
            "Source must be a regular library file"
        );
        Ok(path)
    }

    fn transfer(&mut self, plans: Vec<Transfer>) -> Result<()> {
        if plans.is_empty() {
            return Ok(());
        }
        let mut destinations = HashSet::with_capacity(plans.len());
        for plan in &plans {
            ensure!(
                destinations.insert(key(&plan.destination)),
                "Selected files have duplicate destination names"
            );
            self.directory(
                plan.destination
                    .parent()
                    .context("Destination has no parent")?,
            )?;
            absent(&plan.destination)?;
        }
        // Persist annotations at BOTH names before touching source bytes. A crash
        // or final persistence failure cannot orphan the only copy of a note.
        // Only affected entries are cloned; a large catalog/index is not copied.
        let mut undo = Vec::with_capacity(plans.len());
        for plan in &plans {
            let destination = key(&plan.destination);
            undo.push((
                destination.clone(),
                self.metadata.get(&destination).cloned(),
                self.annotation_extra.get(&destination).cloned(),
            ));
            self.copy_annotation(&plan.source, &plan.destination);
        }
        if let Err(error) = self.save() {
            for (name, annotation, extra) in undo {
                restore(&mut self.metadata, name.clone(), annotation);
                restore(&mut self.annotation_extra, name, extra);
            }
            return Err(error).context("No files moved because annotations could not be saved");
        }
        let mut completed = 0;
        let mut failure = None;
        for plan in &plans {
            let result = self.file(&plan.source).and_then(|_| {
                ensure!(
                    Snapshot::read(&plan.source)? == plan.snapshot,
                    "A selected file changed; refresh and try again"
                );
                exclusive_transfer(self, &plan.source, &plan.destination)
            });
            match result {
                Ok(()) => completed += 1,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        if let Some(error) = failure {
            let mut rollback_errors = Vec::new();
            let mut remaining = HashSet::new();
            for plan in plans[..completed].iter().rev() {
                if let Err(error) = exclusive_transfer(self, &plan.destination, &plan.source) {
                    remaining.insert(key(&plan.destination));
                    rollback_errors.push(format!(
                        "{} remains at {}: {error:#}",
                        plan.source.display(),
                        plan.destination.display()
                    ));
                }
            }
            for (name, annotation, extra) in undo {
                if !remaining.contains(&name) {
                    restore(&mut self.metadata, name.clone(), annotation);
                    restore(&mut self.annotation_extra, name, extra);
                }
            }
            for plan in &plans[..completed] {
                if remaining.contains(&key(&plan.destination)) {
                    self.metadata.remove(&key(&plan.source));
                    self.annotation_extra.remove(&key(&plan.source));
                }
            }
            let save_error = self.save().err().map(|error| format!(" Annotation recovery save failed: {error:#}; staged annotations remain on disk.")).unwrap_or_default();
            bail!(
                "{error:#}{}{}",
                if rollback_errors.is_empty() {
                    String::new()
                } else {
                    format!(
                        " Some moves could not be rolled back: {}.",
                        rollback_errors.join("; ")
                    )
                },
                save_error
            );
        }
        for plan in &plans {
            self.metadata.remove(&key(&plan.source));
            self.annotation_extra.remove(&key(&plan.source));
        }
        self.save().context("Files moved, but final library cleanup could not be saved; annotations are already safely stored at their new paths")
    }

    fn copy_annotation(&mut self, source: &Path, destination: &Path) {
        let source = key(source);
        let destination = key(destination);
        if let Some(annotation) = self.metadata.get(&source).cloned() {
            merge_annotation(
                self.metadata.entry(destination.clone()).or_default(),
                &annotation,
            );
        }
        if let Some(extra) = self.annotation_extra.get(&source).cloned() {
            let target = self.annotation_extra.entry(destination).or_default();
            for (name, value) in extra {
                target.entry(name).or_insert(value);
            }
        }
    }
}

fn restore<T>(map: &mut HashMap<String, T>, key: String, previous: Option<T>) {
    match previous {
        Some(value) => {
            map.insert(key, value);
        }
        None => {
            map.remove(&key);
        }
    }
}

fn validate_name(name: &str) -> Result<()> {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_lowercase();
    let reserved = matches!(
        stem.as_str(),
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) || ["com", "lpt"].into_iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    ensure!(
        !name.is_empty()
            && name.trim() == name
            && name.encode_utf16().count() <= 240
            && !matches!(name, "." | "..")
            && !name.ends_with(['.', ' '])
            && !reserved
            && !name.chars().any(|c| c.is_control()
                || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')),
        "Use a valid Windows file or folder name without a path"
    );
    Ok(())
}

fn absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => bail!("A file or folder already exists: {}", path.display()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    identity: (u64, u64),
    size: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}
impl Snapshot {
    fn read(path: &Path) -> Result<Self> {
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            meta.is_file() && !is_link(&meta),
            "Source is no longer a regular file"
        );
        Self::of(&File::open(path)?)
    }
    fn of(file: &File) -> Result<Self> {
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && !is_link(&metadata),
            "Source is no longer a regular file"
        );
        Ok(Self {
            identity: file_identity(file, &metadata)?,
            size: metadata.len(),
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        })
    }
}

struct Transfer {
    source: PathBuf,
    destination: PathBuf,
    snapshot: Snapshot,
}
impl Transfer {
    fn new(source: PathBuf, destination: PathBuf) -> Result<Self> {
        let snapshot = Snapshot::read(&source)?;
        Ok(Self {
            source,
            destination,
            snapshot,
        })
    }
}

fn exclusive_transfer(catalog: &Catalog, source: &Path, destination: &Path) -> Result<()> {
    catalog.file(source)?;
    catalog.directory(destination.parent().context("Destination has no parent")?)?;
    absent(destination)?;
    let mut input = File::open(source)?;
    let original = Snapshot::of(&input)?;
    let mut created: Option<Snapshot> = None;
    let result = (|| -> Result<()> {
        match fs::hard_link(source, destination) {
            Ok(()) => {
                created = Some(Snapshot::read(destination)?);
            }
            Err(error) if copy_fallback(&error) => {
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(destination)
                    .with_context(|| {
                        format!(
                            "Cannot create {} without overwriting",
                            destination.display()
                        )
                    })?;
                // Capture our file identity before copying, including on copy failure.
                created = Some(Snapshot::of(&output)?);
                let copied = io::copy(&mut input, &mut output)?;
                ensure!(copied == original.size, "Source changed during the copy");
                output.sync_all()?;
                fs::set_permissions(destination, input.metadata()?.permissions())?;
                created = Some(Snapshot::of(&output)?);
            }
            Err(error) => return Err(error).context("Cannot move file without overwriting"),
        }
        ensure!(
            Snapshot::of(&input)? == original && Snapshot::read(source)? == original,
            "Source changed during the operation; source has not been removed"
        );
        let destination_snapshot = Snapshot::read(destination)?;
        ensure!(
            created
                .as_ref()
                .is_some_and(|created| created.identity == destination_snapshot.identity)
                && destination_snapshot.size == original.size,
            "Destination changed during the operation"
        );
        catalog.file(source)?;
        catalog.file(destination)?;
        catalog.directory(source.parent().context("Source has no parent")?)?;
        catalog.directory(destination.parent().context("Destination has no parent")?)?;
        fs::remove_file(source).context("Cannot remove original after creating destination")?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Some(created) = created
            && Snapshot::read(destination).is_ok_and(|current| current.identity == created.identity)
            && let Err(cleanup) = fs::remove_file(destination)
        {
            bail!(
                "{error:#}. Original is retained, but destination cleanup failed at {}: {cleanup}",
                destination.display()
            );
        }
        return Err(error);
    }
    Ok(())
}

fn copy_fallback(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(error.raw_os_error(), Some(1 | 5 | 17 | 50 | 1314))
    }
    #[cfg(not(windows))]
    {
        matches!(error.raw_os_error(), Some(1 | 18 | 38 | 95))
    }
}

#[cfg(unix)]
fn file_identity(_file: &File, metadata: &Metadata) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn file_identity(file: &File, _metadata: &Metadata) -> Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct FileInformation {
        attributes: u32,
        created: FileTime,
        accessed: FileTime,
        written: FileTime,
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(
            handle: *mut std::ffi::c_void,
            info: *mut FileInformation,
        ) -> i32;
    }
    let mut info = FileInformation::default();
    // SAFETY: the file owns a live handle and info is the documented writable struct.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error()).context("Cannot identify source file");
    }
    Ok((
        u64::from(info.volume),
        (u64::from(info.index_high) << 32) | u64::from(info.index_low),
    ))
}

#[cfg(not(any(unix, windows)))]
compile_error!("Print Atlas library requires Windows or Unix file identity support");

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn temporary_file(parent: &Path) -> Result<(PathBuf, File)> {
    loop {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".library.{}.{}.tmp", std::process::id(), sequence));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("Cannot create temporary library state"),
        }
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both arguments are live NUL-terminated UTF-16 strings. The temporary
    // file is in the destination directory, so this cannot become a cross-volume copy.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0x1 | 0x8) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

struct SavedCatalog<'a>(&'a Catalog);
impl Serialize for SavedCatalog<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let catalog = self.0;
        let mut map = serializer.serialize_map(None)?;
        for (name, value) in &catalog.extra {
            if !matches!(name.as_str(), "root" | "metadata" | "preferences") {
                map.serialize_entry(name, value)?;
            }
        }
        if !catalog.extra.contains_key("version") {
            map.serialize_entry("version", &1)?;
        }
        map.serialize_entry("root", &catalog.root)?;
        map.serialize_entry("preferences", &catalog.preferences)?;
        map.serialize_entry("metadata", &SavedMetadata(catalog))?;
        map.end()
    }
}
struct SavedMetadata<'a>(&'a Catalog);
impl Serialize for SavedMetadata<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.metadata.len()))?;
        for (name, annotation) in &self.0.metadata {
            map.serialize_entry(
                name,
                &SavedAnnotation(annotation, self.0.annotation_extra.get(name)),
            )?;
        }
        map.end()
    }
}
struct SavedAnnotation<'a>(&'a Annotation, Option<&'a Map<String, Value>>);
impl Serialize for SavedAnnotation<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        if let Some(extra) = self.1 {
            for (name, value) in extra {
                if !matches!(name.as_str(), "favorite" | "tags" | "notes") {
                    map.serialize_entry(name, value)?;
                }
            }
        }
        map.serialize_entry("favorite", &self.0.favorite)?;
        map.serialize_entry("tags", &self.0.tags)?;
        map.serialize_entry("notes", &self.0.notes)?;
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Fixture {
        directory: PathBuf,
        root: PathBuf,
        state: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "print-atlas-library-{}-{stamp}-{}",
                std::process::id(),
                TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory).unwrap();
            let directory = fs::canonicalize(directory).unwrap();
            let root = directory.join("models");
            fs::create_dir(&root).unwrap();
            Self {
                state: directory.join("data").join("library.json"),
                directory,
                root,
            }
        }
        fn catalog(&self) -> Catalog {
            load_from(&self.root, &self.state).unwrap()
        }
        fn saved(&self) -> Value {
            serde_json::from_reader(File::open(&self.state).unwrap()).unwrap()
        }
        fn write_state(&self, value: &Value) {
            fs::create_dir_all(self.state.parent().unwrap()).unwrap();
            fs::write(&self.state, serde_json::to_vec(value).unwrap()).unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
    fn annotation() -> Annotation {
        Annotation {
            favorite: true,
            tags: vec![" functional ".into(), "functional".into(), "blue".into()],
            notes: "Keep this note 🦀".into(),
        }
    }

    #[test]
    fn windows_metadata_keys_normalize_slashes_case_and_verbatim_prefixes() {
        assert_eq!(
            key_text(r"C:\Users\Scott\Models\PART.STL"),
            "c:/users/scott/models/part.stl"
        );
        assert_eq!(
            key_text(r"\\?\C:\Users\Scott\Models\PART.STL"),
            "c:/users/scott/models/part.stl"
        );
        assert_eq!(
            key_text(r"\\?\UNC\SERVER\Share\PART.STL"),
            "//server/share/part.stl"
        );
        assert_eq!(
            key_text(r"\\SERVER\Share\PART.STL"),
            "//server/share/part.stl"
        );
        assert_eq!(key_text("C:/"), "c:/");
        assert!(!inside(
            Path::new("C:/models"),
            Path::new("C:/models-other/a.stl")
        ));
        assert!(inside(
            Path::new(r"C:\MODELS"),
            Path::new("c:/models/folder/a.stl")
        ));
    }

    #[test]
    fn electron_migration_preserves_root_preferences_unknowns_and_fractional_index() {
        let fixture = Fixture::new();
        let file = fixture.root.join("Part.STL");
        fs::write(&file, b"model bytes").unwrap();
        let old_key = file.to_string_lossy().replace('/', "\\").to_uppercase();
        let missing_key = fixture
            .root
            .join("missing.stl")
            .to_string_lossy()
            .into_owned();
        let old_index = json!({"root":fixture.root,"files":[{"path":file,"mtime":1234.567,"size":11}],"folders":[]});
        let mut metadata = Map::new();
        metadata.insert(old_key, json!({"favorite":true,"tags":[" functional ","functional",""],"notes":"Existing note","custom":{"color":"blue"}}));
        metadata.insert(
            missing_key.clone(),
            json!({"favorite":false,"tags":["archive"],"notes":"Not currently indexed"}),
        );
        fixture.write_state(&json!({
            "version":1,"root":fixture.root,"preferences":{"sort":"name","nested":{"a":1}},
            "metadata":metadata,"index":old_index,"future_feature":{"keep":[1,2,3]}
        }));
        let unrelated_default = fixture.directory.join("unused-default");
        let catalog = load_from(&unrelated_default, &fixture.state).unwrap();
        assert_eq!(catalog.root, fixture.root);
        assert!(catalog.annotation(&file).favorite);
        assert_eq!(catalog.annotation(&file).tags, ["functional"]);
        assert_eq!(catalog.annotation(&file).notes, "Existing note");
        assert_eq!(
            catalog
                .annotation(Path::new(&file.to_string_lossy().replace('/', "\\")))
                .notes,
            "Existing note"
        );
        assert_eq!(
            catalog.annotation(Path::new(&missing_key)).notes,
            "Not currently indexed"
        );
        let saved = fixture.saved();
        assert_eq!(saved["index"], old_index);
        assert_eq!(saved["future_feature"], json!({"keep":[1,2,3]}));
        assert_eq!(
            saved["preferences"],
            json!({"sort":"name","nested":{"a":1}})
        );
        assert_eq!(
            saved["metadata"][key(&file)]["custom"],
            json!({"color":"blue"})
        );
        assert_eq!(saved["metadata"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn duplicate_case_keys_merge_without_losing_either_note() {
        let fixture = Fixture::new();
        let file = fixture.root.join("part.stl");
        let lower = key(&file);
        let upper = file.to_string_lossy().replace('/', "\\").to_uppercase();
        let mut metadata = Map::new();
        metadata.insert(
            lower.clone(),
            json!({"favorite":false,"tags":["one"],"notes":"first"}),
        );
        metadata.insert(
            upper,
            json!({"favorite":true,"tags":["two"],"notes":"second"}),
        );
        fixture.write_state(&json!({"root":fixture.root,"metadata":metadata}));
        let catalog = fixture.catalog();
        assert_eq!(catalog.metadata.len(), 1);
        let merged = catalog.annotation(&file);
        assert!(merged.favorite);
        assert!(merged.tags.contains(&"one".into()) && merged.tags.contains(&"two".into()));
        assert!(merged.notes.contains("first") && merged.notes.contains("second"));
        assert!(fixture.saved()["metadata"].get(&lower).is_some());
    }

    #[test]
    fn malformed_state_is_reported_and_never_overwritten() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.state.parent().unwrap()).unwrap();
        let broken = b"{\"metadata\":";
        fs::write(&fixture.state, broken).unwrap();
        assert!(load_from(&fixture.root, &fixture.state).is_err());
        assert_eq!(fs::read(&fixture.state).unwrap(), broken);
        fs::write(&fixture.state, b"[]").unwrap();
        assert!(load_from(&fixture.root, &fixture.state).is_err());
        assert_eq!(fs::read(&fixture.state).unwrap(), b"[]");
    }

    #[test]
    fn atomic_repeated_saves_leave_no_temporary_files() {
        let fixture = Fixture::new();
        let mut catalog = fixture.catalog();
        catalog.save().unwrap();
        catalog.preferences = json!({"second":true});
        catalog.save().unwrap();
        assert_eq!(fixture.saved()["preferences"], json!({"second":true}));
        let names: Vec<_> = fs::read_dir(fixture.state.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("library.json")]);
    }

    #[test]
    fn rename_and_move_preserve_bytes_annotations_and_extension() {
        let fixture = Fixture::new();
        let source = fixture.root.join("original.STL");
        let bytes: Vec<u8> = (0..=255).cycle().take(65_539).collect();
        fs::write(&source, &bytes).unwrap();
        let mut catalog = fixture.catalog();
        catalog.annotate(&source, annotation()).unwrap();
        let renamed = catalog.rename(&source, "renamed").unwrap();
        assert_eq!(renamed.file_name().unwrap(), "renamed.STL");
        assert!(!source.exists());
        let folder = catalog.create_folder(Path::new(""), "destination").unwrap();
        let moved = catalog
            .move_files(&[renamed.clone(), renamed.clone()], &folder)
            .unwrap();
        assert_eq!(moved, [folder.join("renamed.STL")]);
        assert!(!renamed.exists());
        assert_eq!(fs::read(&moved[0]).unwrap(), bytes);
        assert!(catalog.annotation(&moved[0]).favorite);
        assert_eq!(catalog.annotation(&moved[0]).tags, ["functional", "blue"]);
        assert_eq!(catalog.annotation(&moved[0]).notes, "Keep this note 🦀");
        assert!(!catalog.metadata.contains_key(&key(&source)));
        assert!(!catalog.metadata.contains_key(&key(&renamed)));
        assert_eq!(
            fixture.catalog().annotation(&moved[0]).notes,
            "Keep this note 🦀"
        );
        let unchanged = catalog.move_files(&moved, &folder).unwrap();
        assert_eq!(unchanged, moved);
    }

    #[test]
    fn collisions_and_duplicate_batch_destinations_never_change_originals() {
        let fixture = Fixture::new();
        let mut catalog = fixture.catalog();
        let first = fixture.root.join("first.stl");
        let second = fixture.root.join("second.stl");
        fs::write(&first, b"first bytes").unwrap();
        fs::write(&second, b"second bytes").unwrap();
        catalog.annotate(&first, annotation()).unwrap();
        assert!(catalog.rename(&first, "second").is_err());
        assert!(catalog.rename(&first, "FIRST.STL").is_err());
        assert_eq!(fs::read(&first).unwrap(), b"first bytes");
        assert_eq!(fs::read(&second).unwrap(), b"second bytes");
        let destination = catalog.create_folder(&fixture.root, "destination").unwrap();
        fs::write(destination.join("second.stl"), b"existing").unwrap();
        assert!(
            catalog
                .move_files(&[first.clone(), second.clone()], &destination)
                .is_err()
        );
        assert!(!destination.join("first.stl").exists());
        assert_eq!(
            fs::read(destination.join("second.stl")).unwrap(),
            b"existing"
        );
        let another = catalog.create_folder(&fixture.root, "another").unwrap();
        let same_name = another.join("first.stl");
        fs::write(&same_name, b"different").unwrap();
        assert!(
            catalog
                .move_files(&[first.clone(), same_name.clone()], &destination)
                .is_err()
        );
        assert!(first.exists() && same_name.exists());
        assert_eq!(catalog.annotation(&first).notes, "Keep this note 🦀");
    }

    #[test]
    fn moves_keep_existing_destination_annotations_and_unknown_fields() {
        let fixture = Fixture::new();
        let source = fixture.root.join("part.stl");
        fs::write(&source, b"part").unwrap();
        let folder = fixture.root.join("destination");
        fs::create_dir(&folder).unwrap();
        let destination = folder.join("part.stl");
        let mut metadata = Map::new();
        metadata.insert(
            key(&source),
            json!({"favorite":true,"tags":["source"],"notes":"source note","future_source":1}),
        );
        metadata.insert(key(&destination), json!({"favorite":false,"tags":["old"],"notes":"old destination note","future_destination":2}));
        fixture.write_state(&json!({"root":fixture.root,"metadata":metadata}));
        let mut catalog = fixture.catalog();
        catalog
            .move_files(std::slice::from_ref(&source), &folder)
            .unwrap();
        let note = catalog.annotation(&destination);
        assert!(note.favorite);
        assert!(note.notes.contains("source note") && note.notes.contains("old destination note"));
        assert!(note.tags.contains(&"source".into()) && note.tags.contains(&"old".into()));
        let saved = fixture.saved();
        assert_eq!(saved["metadata"][key(&destination)]["future_source"], 1);
        assert_eq!(
            saved["metadata"][key(&destination)]["future_destination"],
            2
        );
        assert!(saved["metadata"].get(key(&source)).is_none());
    }

    #[test]
    fn state_write_failure_prevents_moves_and_restores_in_memory_annotations() {
        let fixture = Fixture::new();
        let source = fixture.root.join("part.stl");
        fs::write(&source, b"source").unwrap();
        let mut catalog = fixture.catalog();
        catalog.annotate(&source, annotation()).unwrap();
        // A regular file cannot be used as the state file's parent directory.
        let blocker = fixture.directory.join("blocked");
        fs::write(&blocker, b"not a directory").unwrap();
        catalog.state_file = blocker.join("library.json");
        assert!(catalog.rename(&source, "new").is_err());
        assert!(source.exists() && !fixture.root.join("new.stl").exists());
        assert_eq!(catalog.annotation(&source).notes, "Keep this note 🦀");
        assert!(
            !catalog
                .metadata
                .contains_key(&key(&fixture.root.join("new.stl")))
        );
        assert!(catalog.annotate(&source, Annotation::default()).is_err());
        assert_eq!(catalog.annotation(&source).notes, "Keep this note 🦀");
        let old_root = catalog.root.clone();
        assert!(catalog.set_root(&fixture.directory).is_err());
        assert_eq!(catalog.root, old_root);
    }

    #[test]
    fn names_traversal_external_files_and_excluded_files_are_rejected() {
        let fixture = Fixture::new();
        let mut catalog = fixture.catalog();
        let source = fixture.root.join("part.stl");
        fs::write(&source, b"inside").unwrap();
        let outside = fixture.directory.join("outside.stl");
        fs::write(&outside, b"outside").unwrap();
        for name in [
            "",
            " ",
            "..",
            "../escape",
            "bad/name",
            "bad\\name",
            "file.",
            "file ",
            "CON",
            "nul.txt",
            "LPT1.stl",
            "COM¹.txt",
            "bad:stream",
            "x\0y",
        ] {
            assert!(catalog.rename(&source, name).is_err(), "{name:?}");
            assert!(
                catalog.create_folder(&fixture.root, name).is_err(),
                "{name:?}"
            );
        }
        assert!(
            catalog
                .move_files(std::slice::from_ref(&source), Path::new(".."))
                .is_err()
        );
        assert!(
            catalog
                .move_files(std::slice::from_ref(&outside), &fixture.root)
                .is_err()
        );
        assert!(catalog.annotate(&outside, annotation()).is_err());
        assert!(catalog.rename(&fixture.root, "not-a-file").is_err());
        let excluded = fixture.root.join("target");
        fs::create_dir(&excluded).unwrap();
        let build_file = excluded.join("artifact.stl");
        fs::write(&build_file, b"build").unwrap();
        assert!(catalog.annotate(&build_file, annotation()).is_err());
        assert!(
            catalog
                .move_files(std::slice::from_ref(&source), &excluded)
                .is_err()
        );
        assert!(catalog.create_folder(&fixture.root, ".git").is_err());
        assert_eq!(fs::read(&source).unwrap(), b"inside");
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn scanner_includes_all_regular_files_empty_folders_and_hidden_models() {
        let fixture = Fixture::new();
        for name in [
            "part.STL",
            "print.3mf",
            "photo.TIFF",
            "notes.md",
            "manual.PDF",
            "arbitrary.bin",
            "README",
            ".hidden.STL",
        ] {
            fs::write(fixture.root.join(name), name.as_bytes()).unwrap();
        }
        fs::create_dir(fixture.root.join("empty")).unwrap();
        fs::create_dir(fixture.root.join("nested")).unwrap();
        fs::write(
            fixture.root.join("nested").join("whatever.unknown"),
            b"all files",
        )
        .unwrap();
        let root = fixture.root.clone();
        let index = std::thread::spawn(move || scan(&root, Path::new("")))
            .join()
            .unwrap();
        assert!(index.errors.is_empty(), "{:?}", index.errors);
        assert_eq!(index.files.len(), 9);
        assert_eq!(
            index.folders,
            [PathBuf::from("empty"), PathBuf::from("nested")]
        );
        for entry in &index.files {
            assert_eq!(entry.path, fixture.root.join(&entry.relative));
            assert_eq!(entry.folder, entry.relative.parent().unwrap());
            assert_eq!(entry.size, fs::metadata(&entry.path).unwrap().len());
            assert!(entry.mtime > 0);
        }
        for (name, kind) in [
            ("part.STL", FileKind::Model),
            ("scene.blend", FileKind::Blender),
            ("scene.BLEND", FileKind::Blender),
            ("scene.BlEnD", FileKind::Blender),
            ("scene.blend1", FileKind::Other),
            ("photo.TIFF", FileKind::Image),
            ("notes.md", FileKind::Document),
            ("manual.PDF", FileKind::Pdf),
            ("arbitrary.bin", FileKind::Other),
            ("README", FileKind::Document),
        ] {
            assert_eq!(classify(Path::new(name)), kind);
        }
    }

    #[test]
    fn scanner_excludes_app_source_build_cache_and_explicit_app_directory() {
        let fixture = Fixture::new();
        for name in [
            ".git",
            "target",
            "node_modules",
            "__MACOSX",
            ".cache",
            "cache",
            "Caches",
        ] {
            let directory = fixture.root.join(name);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("not-library.stl"), b"ignored").unwrap();
        }
        let source = fixture.root.join("native-source");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join("Cargo.toml"),
            b"[package]\nname = \"print-atlas\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        fs::write(source.join("fixture.stl"), b"ignored").unwrap();
        let unrelated = fixture.root.join("other-project");
        fs::create_dir(&unrelated).unwrap();
        fs::write(
            unrelated.join("Cargo.toml"),
            b"[package]\nname = \"unrelated\"\n",
        )
        .unwrap();
        fs::write(unrelated.join("part.stl"), b"included").unwrap();
        let app = fixture.root.join("installed-app");
        fs::create_dir(&app).unwrap();
        fs::write(app.join("application.exe"), b"excluded").unwrap();
        let index = scan(&fixture.root, &app);
        assert!(index.errors.is_empty());
        assert_eq!(index.files.len(), 2);
        assert_eq!(index.folders, [PathBuf::from("other-project")]);
        assert!(scan(&source, Path::new("")).files.is_empty());
    }

    #[test]
    fn late_batch_failure_rolls_back_prior_files_and_annotation_keys() {
        let fixture = Fixture::new();
        let first = fixture.root.join("first.stl");
        let second = fixture.root.join("second.stl");
        fs::write(&first, b"first").unwrap();
        fs::write(&second, b"second").unwrap();
        let mut catalog = fixture.catalog();
        catalog.annotate(&first, annotation()).unwrap();
        catalog.annotate(&second, annotation()).unwrap();
        let folder = catalog.create_folder(&fixture.root, "destination").unwrap();
        let plans = vec![
            Transfer::new(first.clone(), folder.join("first.stl")).unwrap(),
            Transfer::new(second.clone(), folder.join("second.stl")).unwrap(),
        ];
        // A deterministic edit after snapshot/preflight, before transfer execution.
        fs::write(&second, b"changed second bytes").unwrap();
        assert!(catalog.transfer(plans).is_err());
        assert_eq!(fs::read(&first).unwrap(), b"first");
        assert_eq!(fs::read(&second).unwrap(), b"changed second bytes");
        assert!(!folder.join("first.stl").exists() && !folder.join("second.stl").exists());
        assert_eq!(catalog.annotation(&first).notes, "Keep this note 🦀");
        assert_eq!(catalog.annotation(&second).notes, "Keep this note 🦀");
        assert!(
            !catalog
                .metadata
                .contains_key(&key(&folder.join("first.stl")))
        );
        assert!(
            !catalog
                .metadata
                .contains_key(&key(&folder.join("second.stl")))
        );
        let reloaded = fixture.catalog();
        assert_eq!(reloaded.annotation(&first).notes, "Keep this note 🦀");
        assert_eq!(reloaded.annotation(&second).notes, "Keep this note 🦀");
    }

    #[cfg(unix)]
    fn directory_link(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }
    #[cfg(windows)]
    fn directory_link(target: &Path, link: &Path) {
        // Directory junctions do not require Developer Mode or elevation.
        let output = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn scanner_and_mutations_never_traverse_internal_or_external_directory_links() {
        let fixture = Fixture::new();
        let real = fixture.root.join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("inside.stl"), b"inside").unwrap();
        let outside = fixture.directory.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("external.stl"), b"outside").unwrap();
        let internal_alias = fixture.root.join("internal-alias");
        let external_alias = fixture.root.join("external-alias");
        directory_link(&real, &internal_alias);
        directory_link(&outside, &external_alias);
        let index = scan(&fixture.root, Path::new(""));
        assert_eq!(index.folders, [PathBuf::from("real")]);
        assert_eq!(index.files.len(), 1);
        assert_eq!(index.files[0].path, real.join("inside.stl"));
        let mut catalog = fixture.catalog();
        assert!(
            catalog
                .annotate(&internal_alias.join("inside.stl"), annotation())
                .is_err()
        );
        assert!(
            catalog
                .annotate(&external_alias.join("external.stl"), annotation())
                .is_err()
        );
        assert!(
            catalog
                .move_files(&[real.join("inside.stl")], &internal_alias)
                .is_err()
        );
        assert!(
            catalog
                .create_folder(&external_alias, "must-not-exist")
                .is_err()
        );
        assert!(!outside.join("must-not-exist").exists());
        let linked_root_index = scan(&internal_alias, Path::new(""));
        assert!(linked_root_index.files.is_empty());
        assert!(!linked_root_index.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scanner_rejects_file_symlinks_but_keeps_regular_hardlinks() {
        let fixture = Fixture::new();
        let source = fixture.root.join("regular.stl");
        fs::write(&source, b"regular").unwrap();
        let symbolic = fixture.root.join("symbolic.stl");
        std::os::unix::fs::symlink(&source, &symbolic).unwrap();
        let hard = fixture.root.join("hard.stl");
        fs::hard_link(&source, &hard).unwrap();
        let index = scan(&fixture.root, Path::new(""));
        assert_eq!(index.files.len(), 2);
        assert!(index.files.iter().all(|entry| entry.path != symbolic));
        assert!(fixture.catalog().annotate(&symbolic, annotation()).is_err());
    }

    #[test]
    fn stale_source_snapshot_detects_replacement_even_at_same_size() {
        let fixture = Fixture::new();
        let source = fixture.root.join("part.stl");
        fs::write(&source, b"first").unwrap();
        let before = Snapshot::read(&source).unwrap();
        // Retain the original inode so immediate inode reuse cannot hide replacement.
        fs::hard_link(&source, fixture.root.join("retained.stl")).unwrap();
        fs::remove_file(&source).unwrap();
        fs::write(&source, b"other").unwrap();
        assert_ne!(Snapshot::read(&source).unwrap(), before);
    }
}
