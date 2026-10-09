#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use chrono::{DateTime, Local};
use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};
use ignore::WalkBuilder;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Instant, UNIX_EPOCH};

// Preset extensions shown as checkboxes (common files).
const PRESET_EXTS: &[&str] = &[
    "py", "js", "ts", "tsx", "jsx", "rs", "go", "java", "c", "h", "cpp", "cs", "html", "css",
    "json", "md", "txt", "xml", "csv", "log", "toml", "yaml", "yml", "sql",
];

const ACCENT: egui::Color32 = egui::Color32::from_rgb(59, 130, 246);
const GREEN: egui::Color32 = egui::Color32::from_rgb(34, 197, 94);
const RED: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);

#[derive(Clone, Debug)]
struct SearchHit {
    path: String,
    modified_unix: i64,
    size: u64,
    matches: usize,
    preview: String,
}

enum Msg {
    Hit(SearchHit),
    Done {
        elapsed_ms: u64,
        walk_ms: u64,
        match_ms: u64,
        walked: usize,
    },
    Error(String),
}

#[derive(Clone, Debug)]
struct SearchParams {
    root: PathBuf,
    query: String,
    query_lower: String,
    recursive: bool,
    case_sensitive: bool,
    exts: HashSet<String>, // lowercase, no dot. Empty = match all.
    exclude_dirs: Vec<String>,
    exclude_file_substr: Vec<String>,
    max_bytes: u64,
    use_gitignore: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
enum SortBy {
    Modified,
    Path,
    Size,
}

impl Default for SortBy {
    fn default() -> Self {
        SortBy::Modified
    }
}

/// Persisted UI state: last folder, last search term, filters.
/// Stored next to the binary as prefs.json (e.g. beside fv-search.exe).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Prefs {
    root: String,
    query: String,
    recursive: bool,
    case_sensitive: bool,
    ext_on: Vec<String>,
    custom_exts: String,
    exclude_dirs: String,
    exclude_file_substr: String,
    max_mb: String,
    use_gitignore: bool,
    sort_by: SortBy,
}

fn legacy_prefs_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("fv-search").join("prefs.json"))
}

fn prefs_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return Some(dir.join("prefs.json"));
        }
    }
    legacy_prefs_path()
}

fn read_prefs_file(p: &Path) -> Option<Prefs> {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Prefs>(&s).ok())
}

fn load_prefs() -> Prefs {
    // Binary folder first; fall back to legacy config-dir file (pre-move).
    if let Some(p) = prefs_path() {
        if let Some(pr) = read_prefs_file(&p) {
            return pr;
        }
    }
    if let Some(legacy) = legacy_prefs_path() {
        if Some(&legacy) != prefs_path().as_ref() {
            if let Some(pr) = read_prefs_file(&legacy) {
                return pr;
            }
        }
    }
    Prefs::default()
}

fn save_prefs(app: &SearchApp) {
    let pr = Prefs {
        root: app.root.clone(),
        query: app.query.clone(),
        recursive: app.recursive,
        case_sensitive: app.case_sensitive,
        ext_on: app
            .ext_checks
            .iter()
            .filter(|(_, on)| *on)
            .map(|(e, _)| e.clone())
            .collect(),
        custom_exts: app.custom_exts.clone(),
        exclude_dirs: app.exclude_dirs.clone(),
        exclude_file_substr: app.exclude_file_substr.clone(),
        max_mb: app.max_mb.clone(),
        use_gitignore: app.use_gitignore,
        sort_by: app.sort_by,
    };
    let Ok(s) = serde_json::to_string_pretty(&pr) else {
        return;
    };
    // Binary folder first; fall back to legacy config dir (e.g. Program Files read-only).
    if let Some(path) = prefs_path() {
        if std::fs::write(&path, &s).is_ok() {
            return;
        }
    }
    if let Some(legacy) = legacy_prefs_path() {
        if Some(&legacy) != prefs_path().as_ref() {
            if let Some(parent) = legacy.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&legacy, &s);
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 740.0])
            .with_min_inner_size([880.0, 600.0])
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "fv-search — fast cross-platform file search",
        options,
        Box::new(|cc| Ok(Box::new(SearchApp::new(cc)))),
    )
}

/// Window/taskbar icon, embedded from assets/icon.png.
fn app_icon() -> egui::IconData {
    let img = image::load_from_memory(include_bytes!("../assets/icon.png"))
        .expect("bundled icon.png must decode")
        .to_rgba8();
    let (w, h) = (img.width(), img.height());
    egui::IconData {
        rgba: img.into_raw(),
        width: w,
        height: h,
    }
}

/// Standard action button: uniform height everywhere, per-label min width.
const BTN_H: f32 = 30.0;

fn btn(label: &str, min_w: f32) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(label).size(14.0))
        .min_size(egui::vec2(min_w, BTN_H))
}

struct SearchApp {
    root: String,
    query: String,
    recursive: bool,
    case_sensitive: bool,
    ext_checks: Vec<(String, bool)>,
    custom_exts: String,
    exclude_dirs: String,
    exclude_file_substr: String,
    max_mb: String,
    use_gitignore: bool,
    sort_by: SortBy,

    results: Vec<SearchHit>,
    searching: bool,
    scanned: Arc<AtomicUsize>,
    elapsed_ms: u64,
    walk_ms: u64,
    match_ms: u64,
    walked: usize,
    error: String,
    selected: Option<usize>,

    rx: Option<Receiver<Msg>>,
    cancel: Option<Arc<AtomicBool>>,
}

impl SearchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // ---- Global look ----
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 6.0);
        style.spacing.indent = 18.0;
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(22.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(14.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(11.5, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(12.5, egui::FontFamily::Monospace),
        );
        cc.egui_ctx.set_style(style);
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = egui::Color32::from_rgb(37, 99, 235);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(55, 65, 81);
        cc.egui_ctx.set_visuals(visuals);

        // Restore last session; first launch starts blank (no preset path/term).
        let prefs = load_prefs();
        let first_run = prefs_path().map(|p| !p.exists()).unwrap_or(true)
            && legacy_prefs_path().map(|p| !p.exists()).unwrap_or(true);
        let ext_on: HashSet<String> = prefs.ext_on.into_iter().collect();

        Self {
            root: if first_run {
                String::new()
            } else {
                prefs.root
            },
            query: if first_run {
                String::new()
            } else {
                prefs.query
            },
            recursive: if first_run { true } else { prefs.recursive },
            case_sensitive: if first_run {
                true
            } else {
                prefs.case_sensitive
            },
            ext_checks: PRESET_EXTS
                .iter()
                .map(|e| {
                    let on = if first_run {
                        *e == "py" // default: code search
                    } else {
                        ext_on.contains(*e)
                    };
                    (e.to_string(), on)
                })
                .collect(),
            custom_exts: prefs.custom_exts,
            exclude_dirs: if first_run {
                String::from("node_module,venv,dist,build")
            } else {
                prefs.exclude_dirs
            },
            exclude_file_substr: if first_run {
                String::from("util,clean")
            } else {
                prefs.exclude_file_substr
            },
            max_mb: if first_run {
                String::from("10")
            } else {
                prefs.max_mb
            },
            use_gitignore: if first_run {
                true
            } else {
                prefs.use_gitignore
            },
            sort_by: prefs.sort_by,
            results: Vec::new(),
            searching: false,
            scanned: Arc::new(AtomicUsize::new(0)),
            elapsed_ms: 0,
            walk_ms: 0,
            match_ms: 0,
            walked: 0,
            error: String::new(),
            selected: None,
            rx: None,
            cancel: None,
        }
    }

    fn build_params(&self) -> Result<SearchParams, String> {
        let root = PathBuf::from(self.root.trim());
        if self.root.trim().is_empty() {
            return Err(String::from("Pick a folder first."));
        }
        if !root.exists() {
            return Err(format!("Folder does not exist: {}", root.display()));
        }
        if !root.is_dir() {
            return Err(format!("Not a folder: {}", root.display()));
        }

        let mut exts: HashSet<String> = HashSet::new();
        for (e, on) in &self.ext_checks {
            if *on {
                exts.insert(e.to_lowercase());
            }
        }
        for part in self.custom_exts.split([',', ';', ' ', '|']) {
            let t = part.trim().trim_start_matches('.').to_lowercase();
            if !t.is_empty() {
                exts.insert(t);
            }
        }

        let exclude_dirs: Vec<String> = self
            .exclude_dirs
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let exclude_file_substr: Vec<String> = self
            .exclude_file_substr
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let max_mb: f64 = self.max_mb.trim().parse().unwrap_or(10.0);
        let max_bytes = ((max_mb.max(0.0)) * 1024.0 * 1024.0) as u64;

        Ok(SearchParams {
            root,
            query: self.query.clone(),
            query_lower: self.query.to_lowercase(),
            recursive: self.recursive,
            case_sensitive: self.case_sensitive,
            exts,
            exclude_dirs,
            exclude_file_substr,
            max_bytes,
            use_gitignore: self.use_gitignore,
        })
    }

    /// Native folder browser. Called on UI thread (required on Windows for COM).
    fn browse_folder(&mut self) {
        let mut dlg = rfd::FileDialog::new().set_title("Choose folder to search");
        let cur = self.root.trim().to_string();
        if !cur.is_empty() {
            let p = PathBuf::from(&cur);
            let base = if p.is_dir() {
                p
            } else {
                p.parent().map(|x| x.to_path_buf()).unwrap_or(p)
            };
            if base.is_dir() {
                dlg = dlg.set_directory(base);
            }
        }
        if let Some(p) = dlg.pick_folder() {
            self.root = p.to_string_lossy().to_string();
            self.error.clear();
            save_prefs(self);
        }
    }

    fn start_search(&mut self) {
        let params = match self.build_params() {
            Ok(p) => p,
            Err(e) => {
                self.error = e;
                return;
            }
        };
        self.error.clear();
        self.results.clear();
        self.selected = None;
        self.elapsed_ms = 0;
        self.walk_ms = 0;
        self.match_ms = 0;
        self.walked = 0;
        self.searching = true;
        save_prefs(self); // remember folder + term + filters

        let (tx, rx): (Sender<Msg>, Receiver<Msg>) = unbounded();
        self.rx = Some(rx);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let scanned = self.scanned.clone();
        scanned.store(0, Ordering::Relaxed);

        std::thread::spawn(move || {
            let t0 = Instant::now();
            match run_search_blocking(&params, &tx, cancel, &scanned) {
                Err(e) => {
                    let _ = tx.send(Msg::Error(e));
                }
                Ok(stats) => {
                    let _ = tx.send(Msg::Done {
                        elapsed_ms: t0.elapsed().as_millis() as u64,
                        walk_ms: stats.walk_ms,
                        match_ms: stats.match_ms,
                        walked: stats.walked,
                    });
                }
            }
        });
    }

    fn stop_search(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
    }

    fn poll_rx(&mut self) {
        // Drain up to N msgs per frame so UI stays responsive.
        if let Some(rx) = self.rx.clone() {
            for _ in 0..500 {
                match rx.try_recv() {
                    Ok(Msg::Hit(h)) => self.results.push(h),
                    Ok(Msg::Done {
                        elapsed_ms,
                        walk_ms,
                        match_ms,
                        walked,
                    }) => {
                        self.elapsed_ms = elapsed_ms;
                        self.walk_ms = walk_ms;
                        self.match_ms = match_ms;
                        self.walked = walked;
                        self.searching = false;
                        self.rx = None;
                        self.cancel = None;
                        self.apply_sort();
                        break;
                    }
                    Ok(Msg::Error(e)) => {
                        self.error = e;
                        self.searching = false;
                        self.rx = None;
                        self.cancel = None;
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
    }

    fn apply_sort(&mut self) {
        match self.sort_by {
            SortBy::Modified => self.results.sort_by_key(|h| h.modified_unix),
            SortBy::Path => self.results.sort_by(|a, b| a.path.cmp(&b.path)),
            SortBy::Size => self.results.sort_by_key(|h| h.size),
        }
    }

    fn folder_valid(&self) -> bool {
        !self.root.trim().is_empty() && Path::new(self.root.trim()).is_dir()
    }

    /// Left sidebar: folder picker + filters + options.
    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.heading("Filters");
        ui.separator();

        // ---- Folder ----
        ui.label(egui::RichText::new("Folder").strong());
        ui.add(
            egui::TextEdit::singleline(&mut self.root)
                .desired_width(f32::INFINITY)
                .hint_text("pick a folder…"),
        );
        ui.horizontal(|ui| {
            if ui.add(btn("Browse…", 120.0)).clicked() {
                self.browse_folder();
            }
            if ui.add(btn("Reveal", 100.0)).clicked() && self.folder_valid() {
                let _ = open::that(self.root.trim());
            }
        });
        if self.root.trim().is_empty() {
            ui.label(egui::RichText::new("Pick a folder to start.").weak());
        } else if self.folder_valid() {
            ui.label(egui::RichText::new("valid folder").color(GREEN).small());
        } else {
            ui.label(egui::RichText::new("folder not found").color(RED).small());
        }

        ui.separator();

        // ---- Extensions ----
        ui.label(egui::RichText::new("Extensions").strong());
        ui.label(
            egui::RichText::new("Empty selection = all files")
                .weak()
                .small(),
        );
        ui.horizontal(|ui| {
            if ui.small_button("All").clicked() {
                for (_, on) in self.ext_checks.iter_mut() {
                    *on = true;
                }
            }
            if ui.small_button("None").clicked() {
                for (_, on) in self.ext_checks.iter_mut() {
                    *on = false;
                }
            }
            if ui.small_button("Code").clicked() {
                let code: HashSet<&str> = [
                    "py", "js", "ts", "tsx", "jsx", "rs", "go", "java", "c", "h", "cpp",
                    "cs",
                ]
                .into_iter()
                .collect();
                for (e, on) in self.ext_checks.iter_mut() {
                    *on = code.contains(e.as_str());
                }
            }
        });
        egui::Grid::new("exts")
            .num_columns(4)
            .spacing([6.0, 2.0])
            .show(ui, |ui| {
                for (i, (ext, on)) in self.ext_checks.iter_mut().enumerate() {
                    ui.checkbox(on, format!(".{ext}"));
                    if (i + 1) % 4 == 0 {
                        ui.end_row();
                    }
                }
            });
        ui.horizontal(|ui| {
            ui.label("Custom:");
            ui.add(
                egui::TextEdit::singleline(&mut self.custom_exts)
                    .desired_width(140.0)
                    .hint_text("env, cfg"),
            );
        });

        ui.separator();

        // ---- Options ----
        ui.label(egui::RichText::new("Options").strong());
        ui.checkbox(&mut self.case_sensitive, "Case sensitive");
        ui.checkbox(&mut self.recursive, "Recursive (nested folders)");
        ui.checkbox(&mut self.use_gitignore, "Respect .gitignore");

        ui.collapsing("Excludes & limits", |ui| {
            ui.label("Skip dirs (substring, comma):");
            ui.add(
                egui::TextEdit::singleline(&mut self.exclude_dirs)
                    .desired_width(f32::INFINITY)
                    .hint_text("node_module,venv,dist,build"),
            );
            ui.label("Skip path (substring, comma):");
            ui.add(
                egui::TextEdit::singleline(&mut self.exclude_file_substr)
                    .desired_width(f32::INFINITY)
                    .hint_text("util,clean"),
            );
            ui.horizontal(|ui| {
                ui.label("Max MB:");
                ui.add(egui::TextEdit::singleline(&mut self.max_mb).desired_width(50.0));
            });
        });
    }

    /// Search bar card on top of central panel. Returns true if search started.
    fn show_searchbar(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Contains").strong());
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.query)
                        .desired_width(340.0)
                        .hint_text("keyword — empty lists all files"),
                );
                if resp.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && !self.searching
                {
                    self.start_search();
                }
                if !self.searching {
                    let search = egui::Button::new(
                        egui::RichText::new("Search").size(15.0).strong(),
                    )
                    .fill(ACCENT)
                    .min_size(egui::vec2(140.0, BTN_H + 2.0));
                    if ui.add_enabled(self.folder_valid(), search).clicked() {
                        self.start_search();
                    }
                } else {
                    if ui.add(btn("Stop", 140.0)).clicked() {
                        self.stop_search();
                    }
                    ui.spinner();
                }
                if ui.add(btn("Clear", 100.0)).clicked() {
                    self.results.clear();
                    self.error.clear();
                    self.elapsed_ms = 0;
                    self.walk_ms = 0;
                    self.match_ms = 0;
                    self.walked = 0;
                    self.scanned.store(0, Ordering::Relaxed);
                    self.selected = None;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Sort:");
                egui::ComboBox::from_id_salt("sort")
                    .selected_text(match self.sort_by {
                        SortBy::Modified => "modified time",
                        SortBy::Path => "path",
                        SortBy::Size => "size",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.sort_by,
                            SortBy::Modified,
                            "modified time",
                        );
                        ui.selectable_value(&mut self.sort_by, SortBy::Path, "path");
                        ui.selectable_value(&mut self.sort_by, SortBy::Size, "size");
                    });
                if ui.small_button("Re-sort").clicked() {
                    self.apply_sort();
                }
                ui.label(
                    egui::RichText::new("Tip: click any result to open it")
                        .weak()
                        .small(),
                );
            });
        });

        if !self.error.is_empty() {
            ui.colored_label(RED, &self.error);
        }
    }
}

fn path_excluded(path: &Path, root: &Path, exclude_dirs: &[String]) -> bool {
    if exclude_dirs.is_empty() {
        return false;
    }
    // Substring check on the path under root ("node_module" in path).
    let s = path.to_string_lossy();
    // Only test the part under root to avoid drive-letter false positives.
    let rel = path
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or(s.to_string());
    exclude_dirs.iter().any(|d| rel.contains(d))
}

fn file_name_excluded(path: &Path, pats: &[String]) -> bool {
    if pats.is_empty() {
        return false;
    }
    let s = path.to_string_lossy();
    pats.iter().any(|p| s.contains(p))
}

fn ext_allowed(path: &Path, exts: &HashSet<String>) -> bool {
    if exts.is_empty() {
        return true; // nothing checked = all files
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => exts.contains(&e.to_lowercase()),
        None => false,
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct WalkStats {
    walk_ms: u64,
    match_ms: u64,
    walked: usize,
}

fn run_search_blocking(
    params: &SearchParams,
    tx: &Sender<Msg>,
    cancel: Arc<AtomicBool>,
    scanned: &AtomicUsize,
) -> Result<WalkStats, String> {
    let t_walk = Instant::now();

    // Prune excluded subtrees at walk time: never descend into them.
    // (Old code visited every file inside node_modules/.venv and skipped one by one.)
    let ex_root = params.root.clone();
    let ex_dirs = params.exclude_dirs.clone();
    let mut builder = WalkBuilder::new(&params.root);
    builder
        .hidden(false)
        .git_ignore(params.use_gitignore)
        .parents(params.use_gitignore)
        .ignore(params.use_gitignore)
        .git_exclude(params.use_gitignore)
        .follow_links(false)
        .require_git(false)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true; // never prune the explicitly picked root
            }
            !path_excluded(e.path(), &ex_root, &ex_dirs)
        });
    if !params.recursive {
        builder.max_depth(Some(1));
    }

    // Stage 1: PARALLEL directory walk, collect candidate files.
    let walked = Arc::new(AtomicUsize::new(0));
    let (cand_tx, cand_rx) = unbounded::<PathBuf>();
    let shared = Arc::new(params.clone());
    {
        let cancel_w = Arc::clone(&cancel);
        let walked_w = Arc::clone(&walked);
        builder.build_parallel().run(move || {
            let tx = cand_tx.clone();
            let params = Arc::clone(&shared);
            let cancel = Arc::clone(&cancel_w);
            let walked = Arc::clone(&walked_w);
            Box::new(
                move |entry: Result<ignore::DirEntry, ignore::Error>| {
                    if cancel.load(Ordering::Relaxed) {
                        return ignore::WalkState::Quit;
                    }
                    let entry = match entry {
                        Ok(e) => e,
                        Err(_) => return ignore::WalkState::Continue,
                    };
                    walked.fetch_add(1, Ordering::Relaxed);
                    let ft = entry.file_type();
                    if ft.map(|t| t.is_dir()).unwrap_or(false) {
                        return ignore::WalkState::Continue;
                    }
                    if !ft.map(|t| t.is_file()).unwrap_or(true) {
                        return ignore::WalkState::Continue;
                    }
                    let p = entry.path().to_path_buf();
                    if file_name_excluded(&p, &params.exclude_file_substr) {
                        return ignore::WalkState::Continue;
                    }
                    if !ext_allowed(&p, &params.exts) {
                        return ignore::WalkState::Continue;
                    }
                    // Cheap size pre-filter (metadata already stat'd by walker).
                    if params.max_bytes > 0 {
                        if let Ok(md) = entry.metadata() {
                            if md.len() > params.max_bytes {
                                return ignore::WalkState::Continue;
                            }
                        }
                    }
                    let _ = tx.send(p);
                    ignore::WalkState::Continue
                },
            )
        });
    }
    let candidates: Vec<PathBuf> = cand_rx.iter().collect();
    let walk_ms = t_walk.elapsed().as_millis() as u64;
    let walked_n = walked.load(Ordering::Relaxed);

    if candidates.is_empty() || cancel.load(Ordering::Relaxed) {
        return Ok(WalkStats {
            walk_ms,
            match_ms: 0,
            walked: walked_n,
        });
    }

    // Stage 2: parallel content read + match (rayon), byte-level via memchr.
    // Old code did from_utf8_lossy (full copy) + to_lowercase (full copy)
    // + ANOTHER to_lowercase per line for the preview. Now: zero-copy
    // memmem scan on bytes, lossy-convert only the single preview line.
    let t_match = Instant::now();
    let query_empty = params.query.is_empty();
    let qbytes = params.query.as_bytes().to_vec();
    let qlower = params.query_lower.as_bytes().to_vec();
    let ascii_mode = params.query.bytes().all(|b| b.is_ascii());
    candidates.par_iter().for_each(|p| {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        scanned.fetch_add(1, Ordering::Relaxed);

        // Re-check size from filesystem (walk metadata can lie on Windows).
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        if params.max_bytes > 0 && size > params.max_bytes {
            return;
        }

        let modified = || {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        };

        if query_empty {
            let _ = tx.send(Msg::Hit(SearchHit {
                path: p.to_string_lossy().to_string(),
                modified_unix: modified(),
                size,
                matches: 0,
                preview: String::new(),
            }));
            return;
        }

        let bytes = match std::fs::read(p) {
            Ok(b) => b,
            Err(_) => return,
        };
        // Skip likely-binary early (NUL in first 1KB).
        if bytes.len() > 1024 && bytes[..1024].contains(&0) {
            return;
        }

        // Lowered view for case-insensitive ASCII (length-preserving, so
        // match offsets map 1:1 back onto the original bytes).
        let lowered: Vec<u8>;
        let (haystack, needle) = if params.case_sensitive {
            (&bytes[..], &qbytes[..])
        } else if ascii_mode {
            lowered = bytes.iter().map(|b| b.to_ascii_lowercase()).collect();
            (&lowered[..], &qlower[..])
        } else {
            // Rare unicode-query fallback: single lowercase pass, reused
            // for both count and preview (no per-line re-lowercasing).
            let text = String::from_utf8_lossy(&bytes);
            let lower = text.to_lowercase();
            let c = lower.matches(params.query_lower.as_str()).count();
            if c == 0 {
                return;
            }
            let preview = match lower.find(params.query_lower.as_str()) {
                Some(off) => {
                    let start = lower[..off].rfind('\n').map(|i| i + 1).unwrap_or(0);
                    let end = lower[off..]
                        .find('\n')
                        .map(|i| off + i)
                        .unwrap_or(lower.len());
                    truncate(lower[start..end].trim(), 140)
                }
                None => String::new(),
            };
            let _ = tx.send(Msg::Hit(SearchHit {
                path: p.to_string_lossy().to_string(),
                modified_unix: modified(),
                size,
                matches: c,
                preview,
            }));
            return;
        };

        let mut it = memchr::memmem::find_iter(haystack, needle);
        let first = match it.next() {
            Some(off) => off,
            None => return,
        };
        let count = 1 + it.count();
        let preview = line_around(&bytes, first);

        let _ = tx.send(Msg::Hit(SearchHit {
            path: p.to_string_lossy().to_string(),
            modified_unix: modified(),
            size,
            matches: count,
            preview,
        }));
    });
    let match_ms = t_match.elapsed().as_millis() as u64;

    Ok(WalkStats {
        walk_ms,
        match_ms,
        walked: walked_n,
    })
}

/// Extract the (lossy) line containing byte offset `off`. Only this slice
/// is UTF-8 converted — not the whole file.
fn line_around(bytes: &[u8], off: usize) -> String {
    let off = off.min(bytes.len());
    let start = memchr::memrchr(b'\n', &bytes[..off])
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = memchr::memchr(b'\n', &bytes[off..])
        .map(|i| off + i)
        .unwrap_or(bytes.len());
    truncate(String::from_utf8_lossy(&bytes[start..end]).trim(), 140)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, c) in s.char_indices() {
        if i >= max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

fn fmt_time(unix: i64) -> String {
    DateTime::<Local>::from(
        UNIX_EPOCH
            .checked_add(std::time::Duration::from_secs(unix.max(0) as u64))
            .unwrap_or(UNIX_EPOCH),
    )
    .format("%Y-%m-%d %H:%M:%S")
    .to_string()
}

fn fmt_size(b: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if b < 1024 {
        format!("{} B", b)
    } else if (b as f64) < MB {
        format!("{:.1} KB", b as f64 / KB)
    } else {
        format!("{:.2} MB", b as f64 / MB)
    }
}

fn split_path(p: &str) -> (String, String) {
    let path = Path::new(p);
    let fname = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.to_string());
    let folder = path
        .parent()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    (fname, folder)
}

/// Short uppercase tag + color per extension. Pure ASCII text, so it
/// renders with any font (no emoji-font dependency).
fn ext_badge(p: &str) -> (String, egui::Color32) {
    let ext = Path::new(p)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();
    let green = egui::Color32::from_rgb(74, 222, 128);
    let yellow = egui::Color32::from_rgb(250, 204, 21);
    let blue = egui::Color32::from_rgb(96, 165, 250);
    let orange = egui::Color32::from_rgb(251, 146, 60);
    let cyan = egui::Color32::from_rgb(34, 211, 238);
    let red = egui::Color32::from_rgb(248, 113, 113);
    let purple = egui::Color32::from_rgb(192, 132, 252);
    let pink = egui::Color32::from_rgb(244, 114, 182);
    let teal = egui::Color32::from_rgb(45, 212, 191);
    let gray = egui::Color32::from_rgb(148, 163, 184);
    match ext.as_str() {
        "py" => ("PY".into(), green),
        "js" | "jsx" => ("JS".into(), yellow),
        "ts" => ("TS".into(), blue),
        "tsx" => ("TSX".into(), blue),
        "rs" => ("RS".into(), orange),
        "go" => ("GO".into(), cyan),
        "java" => ("JAVA".into(), red),
        "c" | "h" => (ext.to_uppercase(), purple),
        "cpp" => ("C++".into(), purple),
        "cs" => ("C#".into(), purple),
        "html" => ("HTML".into(), pink),
        "css" => ("CSS".into(), pink),
        "json" | "xml" | "yaml" | "yml" | "toml" => (ext.to_uppercase(), gray),
        "csv" | "sql" => (ext.to_uppercase(), teal),
        "md" | "txt" | "log" => (ext.to_uppercase(), gray),
        _ if ext.is_empty() => ("FILE".into(), gray),
        _ => (
            ext.to_uppercase().chars().take(4).collect(),
            gray,
        ),
    }
}

impl eframe::App for SearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.searching {
            self.poll_rx();
            ctx.request_repaint(); // live results
        } else if self.rx.is_some() {
            self.poll_rx();
        }

        // ---- Header ----
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("fv-search");
                ui.label(
                    egui::RichText::new("fast parallel content search")
                        .weak()
                        .small(),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let scanned = self.scanned.load(Ordering::Relaxed);
                        if self.searching {
                            ui.spinner();
                            ui.label(format!(
                                "{} scanned • {} matched",
                                scanned,
                                self.results.len()
                            ));
                        } else if self.elapsed_ms > 0 || !self.results.is_empty() {
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} visited • {} read • {} matched • {} ms (walk {} + match {})",
                                    self.walked,
                                    scanned,
                                    self.results.len(),
                                    self.elapsed_ms,
                                    self.walk_ms,
                                    self.match_ms
                                ))
                                .color(GREEN)
                                .small(),
                            );
                        } else {
                            ui.label(
                                    egui::RichText::new("idle — pick folder, then Search")
                                    .weak()
                                    .small(),
                            );
                        }
                    },
                );
            });
        });

        // ---- Sidebar ----
        egui::SidePanel::left("side")
            .resizable(true)
            .default_width(290.0)
            .width_range(240.0..=420.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.show_sidebar(ui);
                });
            });

        // ---- Main ----
        egui::CentralPanel::default().show(ctx, |ui| {
            self.show_searchbar(ui);
            ui.add_space(4.0);

            // Selected file action bar.
            if let Some(hit) = self.selected.and_then(|i| self.results.get(i)).cloned()
            {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Selected:").strong());
                        ui.label(
                            egui::RichText::new(hit.path.clone())
                                .monospace()
                                .small(),
                        );
                    });
                    if !hit.preview.is_empty() {
                        ui.monospace(format!("› {}", hit.preview));
                    }
                    ui.horizontal(|ui| {
                        if ui.add(btn("Open file", 120.0).fill(ACCENT)).clicked()
                        {
                            if let Err(e) = open::that(&hit.path) {
                                self.error =
                                    format!("Cannot open {}: {}", hit.path, e);
                            }
                        }
                        if ui.add(btn("Folder", 100.0)).clicked() {
                            if let Some(parent) = Path::new(&hit.path).parent() {
                                let _ = open::that(parent);
                            }
                        }
                        if ui.add(btn("Copy path", 110.0)).clicked() {
                            ctx.copy_text(hit.path.clone());
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{} • {}",
                                fmt_time(hit.modified_unix),
                                fmt_size(hit.size)
                            ))
                            .weak()
                            .small(),
                        );
                    });
                });
                ui.add_space(4.0);
            }

            // ---- Results ----
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("Results ({})", self.results.len()))
                        .strong()
                        .size(16.0),
                );
            });

            if self.results.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    if self.searching {
                        ui.spinner();
                        ui.label("Searching… results stream in live.");
                    } else {
                        ui.label(
                            egui::RichText::new(
                                "No results yet.\nPick a folder, type a keyword, hit Search.\nClick any result to open the file.",
                            )
                            .weak(),
                        );
                    }
                });
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    // Cap rendered rows to keep UI fast on huge result sets.
                    const RENDER_CAP: usize = 5000;
                    let n = self.results.len().min(RENDER_CAP);
                    let mut to_open: Option<String> = None;
                    let mut select: Option<usize> = None;

                    for i in 0..n {
                        let h = &self.results[i];
                        let (fname, folder) = split_path(&h.path);
                        let sel = self.selected == Some(i);
                        let bg = if sel {
                            egui::Color32::from_rgb(30, 58, 138)
                        } else if i % 2 == 0 {
                            egui::Color32::from_rgb(24, 24, 27)
                        } else {
                            egui::Color32::from_rgb(30, 30, 34)
                        };

                        // Frame response only senses hover — add explicit
                        // click area over the same rect so clicks always open.
                        let frame_rect = egui::Frame::new()
                            .fill(bg)
                            .corner_radius(6)
                            .inner_margin(egui::Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let (tag, tag_color) = ext_badge(&h.path);
                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(18, 18, 22))
                                        .corner_radius(4)
                                        .inner_margin(egui::Margin::symmetric(6, 3))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(tag)
                                                    .monospace()
                                                    .strong()
                                                    .size(12.0)
                                                    .color(tag_color),
                                            );
                                        });
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                egui::RichText::new(fname)
                                                    .strong()
                                                    .size(13.5),
                                            );
                                            if h.matches > 0 {
                                                ui.label(
                                                    egui::RichText::new(format!(
                                                        "{}×",
                                                        h.matches
                                                    ))
                                                    .color(GREEN)
                                                    .small()
                                                    .strong(),
                                                );
                                            }
                                        });
                                        ui.label(
                                            egui::RichText::new(folder)
                                                .weak()
                                                .small()
                                                .monospace(),
                                        );
                                        if !h.preview.is_empty() {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "› {}",
                                                    h.preview
                                                ))
                                                .small(),
                                            );
                                        }
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(
                                            egui::Align::Center,
                                        ),
                                        |ui| {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{}\n{}",
                                                    fmt_size(h.size),
                                                    fmt_time(h.modified_unix)
                                                ))
                                                .weak()
                                                .small(),
                                            );
                                        },
                                    );
                                });
                            })
                            .response
                            .rect;

                        let resp = ui
                            .interact(
                                frame_rect,
                                ui.make_persistent_id(("hit", i)),
                                egui::Sense::click(),
                            )
                            .on_hover_text("Click to open file");
                        if resp.hovered() {
                            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
                        }

                        // SINGLE CLICK = open file (per user request).
                        if resp.clicked() {
                            select = Some(i);
                            to_open = Some(h.path.clone());
                        }
                        resp.context_menu(|ui| {
                            if ui.button("Open file").clicked() {
                                to_open = Some(h.path.clone());
                                ui.close();
                            }
                            if ui.button("Open containing folder").clicked() {
                                if let Some(parent) =
                                    Path::new(&h.path).parent()
                                {
                                    let _ = open::that(parent);
                                }
                                ui.close();
                            }
                            if ui.button("Copy path").clicked() {
                                ctx.copy_text(h.path.clone());
                                ui.close();
                            }
                        });
                    }

                    if let Some(i) = select {
                        self.selected = Some(i);
                    }
                    if let Some(p) = to_open {
                        if let Err(e) = open::that(&p) {
                            self.error = format!("Cannot open {}: {}", p, e);
                        }
                    }

                    if self.results.len() > RENDER_CAP {
                        ui.label(
                            egui::RichText::new(format!(
                                "… +{} more (refine query)",
                                self.results.len() - RENDER_CAP
                            ))
                            .weak(),
                        );
                    }
                });
        });
    }
}
