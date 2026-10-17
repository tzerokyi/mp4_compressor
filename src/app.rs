use eframe::egui::{self, Color32, FontId, Pos2, Rect, Stroke, Vec2};
use mp4rust_native::encode::{Mode, Settings};
use mp4rust_native::worker::{self, Event};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread::JoinHandle;
use std::time::Duration;

const BG: Color32 = Color32::from_rgb(8, 17, 31);
const PANEL: Color32 = Color32::from_rgb(15, 29, 49);
const PANEL_LIGHT: Color32 = Color32::from_rgb(20, 38, 62);
const BORDER: Color32 = Color32::from_rgb(40, 68, 101);
const PRIMARY: Color32 = Color32::from_rgb(41, 139, 255);
const TEXT: Color32 = Color32::from_rgb(235, 243, 255);
const MUTED: Color32 = Color32::from_rgb(149, 172, 205);
const THUMB_W: usize = 160;
const THUMB_H: usize = 90;

struct Preview {
    path: PathBuf,
    dimensions: String,
    duration: Option<f64>,
    rgba: Option<Vec<u8>>,
}

struct JobRow {
    path: PathBuf,
    state: String,
    source_bytes: u64,
    duration: Option<f64>,
    dimensions: String,
    preview: Option<egui::TextureHandle>,
    output: Option<PathBuf>,
    output_bytes: Option<u64>,
    saved: Option<f64>,
}

pub struct App {
    icon: egui::TextureHandle,
    jobs: Vec<JobRow>,
    settings: Settings,
    preview_tx: mpsc::Sender<Preview>,
    preview_rx: mpsc::Receiver<Preview>,
    receive: Option<mpsc::Receiver<Event>>,
    cancel: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    running: bool,
    progress: f32,
    hint: String,
    completed: usize,
}

fn panel() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(15)
        .inner_margin(egui::Margin::same(16))
}

fn read_preview(path: &PathBuf) -> Preview {
    let media = mp4rust_native::probe::inspect(path).ok();
    let (dimensions, duration) = match media {
        Some(ref m) => (
            format!(
                "{} × {}",
                m.video.width.unwrap_or(0),
                m.video.height.unwrap_or(0)
            ),
            Some(m.duration),
        ),
        None => (String::new(), None),
    };
    let rgba = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-an",
            "-sn",
            "-vf",
            "scale=160:90:force_original_aspect_ratio=decrease,pad=160:90:(ow-iw)/2:(oh-ih)/2",
            "-pix_fmt",
            "rgba",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .and_then(|o| {
            (o.status.success() && o.stdout.len() == THUMB_W * THUMB_H * 4).then_some(o.stdout)
        });
    Preview {
        path: path.clone(),
        dimensions,
        duration,
        rgba,
    }
}

fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1_000_000_000.0 {
        format!("{:.2} GB", b / 1_000_000_000.0)
    } else if b >= 1_000_000.0 {
        format!("{:.1} MB", b / 1_000_000.0)
    } else {
        format!("{:.0} KB", b / 1000.0)
    }
}

fn format_duration(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = BG;
        visuals.window_fill = BG;
        visuals.extreme_bg_color = PANEL;
        visuals.override_text_color = Some(TEXT);
        visuals.widgets.inactive.bg_fill = PANEL_LIGHT;
        visuals.widgets.inactive.weak_bg_fill = PANEL_LIGHT;
        visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        visuals.widgets.hovered.bg_fill = Color32::from_rgb(30, 55, 86);
        visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(30, 55, 86);
        visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, PRIMARY);
        visuals.widgets.active.bg_fill = PRIMARY;
        visuals.selection.bg_fill = Color32::from_rgb(28, 80, 141);
        cc.egui_ctx.set_visuals(visuals);
        cc.egui_ctx.style_mut(|style| {
            style.spacing.item_spacing = Vec2::new(10.0, 10.0);
            style.spacing.button_padding = Vec2::new(15.0, 9.0);
            style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(10);
            style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(10);
            style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(10);
        });
        install_korean_font(&cc.egui_ctx);
        let (preview_tx, preview_rx) = mpsc::channel();
        Self {
            icon: crate::icon::texture(&cc.egui_ctx),
            jobs: Vec::new(),
            settings: Settings::default(),
            preview_tx,
            preview_rx,
            receive: None,
            cancel: Arc::new(AtomicBool::new(false)),
            thread: None,
            running: false,
            progress: 0.0,
            hint: String::new(),
            completed: 0,
        }
    }

    fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>, ctx: &egui::Context) {
        if self.running {
            return;
        }
        let mut new_paths = Vec::new();
        for path in paths {
            if !path.is_file() {
                continue;
            }
            let supported = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ["mp4", "mov", "mkv", "m4v"].contains(&ext.to_ascii_lowercase().as_str())
                });
            if !supported || self.jobs.iter().any(|job| job.path == path) {
                continue;
            }
            self.jobs.push(JobRow {
                source_bytes: std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                path: path.clone(),
                state: "Ready".to_owned(),
                duration: None,
                dimensions: String::new(),
                preview: None,
                output: None,
                output_bytes: None,
                saved: None,
            });
            new_paths.push(path);
            if self.jobs.len() >= 200 {
                break;
            }
        }
        if !new_paths.is_empty() {
            let tx = self.preview_tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                for path in new_paths {
                    let _ = tx.send(read_preview(&path));
                    ctx.request_repaint();
                }
            });
        }
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        while let Ok(p) = self.preview_rx.try_recv() {
            if let Some(row) = self.jobs.iter_mut().find(|row| row.path == p.path) {
                row.dimensions = p.dimensions;
                row.duration = p.duration;
                if let Some(rgba) = p.rgba {
                    let image = egui::ColorImage::from_rgba_unmultiplied([THUMB_W, THUMB_H], &rgba);
                    row.preview = Some(ctx.load_texture(
                        format!("video-thumb-{}", row.path.display()),
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                }
            }
        }
    }

    fn start(&mut self) {
        if self.running || self.jobs.is_empty() {
            return;
        }
        if let Some(previous) = self.thread.take() {
            let _ = previous.join();
        }
        let files = self.jobs.iter().map(|j| j.path.clone()).collect();
        let settings = self.settings.clone();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel);
        let (tx, rx) = mpsc::channel();
        self.receive = Some(rx);
        self.running = true;
        self.completed = 0;
        self.progress = 0.0;
        self.hint = "Preparing…".to_owned();
        for row in &mut self.jobs {
            row.state = "Queued".to_owned();
            row.saved = None;
            row.output = None;
            row.output_bytes = None;
        }
        self.thread = Some(std::thread::spawn(move || {
            worker::run_batch(files, settings, tx, cancel)
        }));
    }

    fn poll(&mut self) {
        let mut events = Vec::new();
        if let Some(receiver) = &self.receive {
            while let Ok(event) = receiver.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                Event::Started(path) => {
                    self.progress = 0.0;
                    self.set_state(&path, "Preparing".to_owned());
                }
                Event::Stage(path, label, progress) => {
                    self.hint = label.clone();
                    self.progress = progress.clamp(0.0, 1.0);
                    self.set_state(&path, label);
                }
                Event::Progress(path, progress) => {
                    self.progress = progress.clamp(0.0, 1.0);
                    self.set_state(&path, format!("Encoding {:.0}%", self.progress * 100.0));
                }
                Event::Finished {
                    input,
                    output,
                    before,
                    after,
                    note: _,
                } => {
                    self.set_state(&input, "Completed".to_owned());
                    if let Some(row) = self.jobs.iter_mut().find(|row| row.path == input) {
                        row.saved = Some((1.0 - after as f64 / before.max(1) as f64) * 100.0);
                        row.output_bytes = Some(after);
                        row.output = Some(output);
                    }
                    self.completed += 1;
                }
                Event::Skipped(path, why) => {
                    self.set_state(&path, format!("Skipped · {why}"));
                    self.completed += 1;
                }
                Event::Failed(path, why) => {
                    self.set_state(&path, format!("Failed · {why}"));
                    self.completed += 1;
                }
                Event::Done => {
                    self.running = false;
                    self.progress = 1.0;
                    self.hint = "Finished".to_owned();
                    self.receive = None;
                    if let Some(previous) = self.thread.take() {
                        let _ = previous.join();
                    }
                }
            }
        }
    }

    fn set_state(&mut self, path: &PathBuf, value: String) {
        if let Some(row) = self.jobs.iter_mut().find(|row| &row.path == path) {
            row.state = value;
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.image((self.icon.id(), Vec2::splat(40.0)));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("mp4 compressor")
                    .size(25.0)
                    .strong()
                    .color(TEXT),
            );
        });
    }

    fn drop_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, hovering: bool) {
        let background = if hovering {
            Color32::from_rgb(20, 48, 78)
        } else {
            PANEL
        };
        let inner = egui::Frame::new()
            .fill(background)
            .corner_radius(15)
            .inner_margin(egui::Margin::same(14));
        let response = inner.show(ui, |ui| {
            ui.set_min_height(142.0);
            ui.vertical_centered(|ui| {
                ui.add_space(11.0);
                let center = ui.cursor().center_top() + Vec2::new(0.0, 20.0);
                let painter = ui.painter();
                folder_symbol(painter, center, 1.5, if hovering { PRIMARY } else { MUTED });
                ui.add_space(42.0);
                ui.label(
                    egui::RichText::new(if hovering {
                        "Release to add videos"
                    } else {
                        "Drag & drop videos here"
                    })
                    .color(TEXT)
                    .size(17.0),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.add_space(((ui.available_width() - 268.0) / 2.0).max(0.0));
                    if ui
                        .add_enabled(
                            !self.running,
                            egui::Button::new("Add Files").min_size(Vec2::new(133.0, 32.0)),
                        )
                        .clicked()
                    {
                        if let Some(paths) = rfd::FileDialog::new()
                            .add_filter("Video", &["mp4", "mov", "mkv", "m4v"])
                            .pick_files()
                        {
                            self.add_paths(paths, ctx);
                        }
                    }
                    if ui
                        .add_enabled(
                            !self.running,
                            egui::Button::new("Add Folder").min_size(Vec2::new(122.0, 32.0)),
                        )
                        .clicked()
                    {
                        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                            if let Ok(entries) = std::fs::read_dir(folder) {
                                self.add_paths(
                                    entries.filter_map(Result::ok).map(|e| e.path()),
                                    ctx,
                                );
                            }
                        }
                    }
                });
            });
        });
        let rect = response.response.rect;
        let stroke = Stroke::new(1.0_f32, if hovering { PRIMARY } else { BORDER });
        dotted_frame(ui.painter(), rect, stroke);
    }

    fn queue(&mut self, ui: &mut egui::Ui, height: f32) {
        let available_width = ui.available_width();
        egui::ScrollArea::vertical()
            .max_height(height)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_min_width(available_width - 8.0);
                if self.jobs.is_empty() {
                    panel().show(ui, |ui| {
                        ui.set_min_height(68.0);
                        ui.centered_and_justified(|ui| {
                            ui.label(
                                egui::RichText::new("No videos added")
                                    .color(MUTED)
                                    .size(16.0),
                            );
                        });
                    });
                }
                let mut to_remove = None;
                for (index, job) in self.jobs.iter().enumerate() {
                    panel().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            if let Some(image) = &job.preview {
                                ui.add(
                                    egui::Image::new((image.id(), Vec2::new(118.0, 66.0)))
                                        .corner_radius(8),
                                );
                            } else {
                                let (rect, _) = ui.allocate_exact_size(
                                    Vec2::new(118.0, 66.0),
                                    egui::Sense::hover(),
                                );
                                ui.painter().rect_filled(rect, 8, PANEL_LIGHT);
                                draw_play(ui.painter(), rect.center(), 12.0, MUTED);
                            }
                            ui.add_space(8.0);
                            let reserve = if available_width < 925.0 {
                                155.0
                            } else {
                                205.0
                            };
                            let detail_width = (ui.available_width() - reserve).max(190.0);
                            ui.allocate_ui_with_layout(
                                Vec2::new(detail_width, 67.0),
                                egui::Layout::top_down(egui::Align::Min),
                                |ui| {
                                    let filename = job
                                        .path
                                        .file_name()
                                        .map(|x| x.to_string_lossy())
                                        .unwrap_or_default();
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(filename.as_ref())
                                                .size(16.0)
                                                .strong(),
                                        )
                                        .truncate(),
                                    );
                                    let details = format!(
                                        "{}   ·   {}{}",
                                        job.dimensions,
                                        format_bytes(job.source_bytes),
                                        job.duration
                                            .map(|s| format!("   ·   {}", format_duration(s)))
                                            .unwrap_or_default()
                                    );
                                    ui.label(egui::RichText::new(details).color(MUTED).size(12.5));
                                    if job.state != "Ready" && job.state != "Queued" {
                                        let color = if job.state.starts_with("Failed") {
                                            Color32::from_rgb(245, 135, 135)
                                        } else if job.output.is_some() {
                                            Color32::from_rgb(114, 216, 174)
                                        } else {
                                            MUTED
                                        };
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&job.state)
                                                    .color(color)
                                                    .size(11.5),
                                            )
                                            .truncate(),
                                        );
                                    }
                                },
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add_enabled(
                                            !self.running,
                                            egui::Button::new("✕").frame(false),
                                        )
                                        .on_hover_text("Remove video")
                                        .clicked()
                                    {
                                        to_remove = Some(index);
                                    }
                                    ui.add_space(11.0);
                                    let output = job
                                        .output_bytes
                                        .map(format_bytes)
                                        .unwrap_or_else(|| "—".to_owned());
                                    if let Some(output_path) = &job.output {
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new(output)
                                                        .color(PRIMARY)
                                                        .size(17.0)
                                                        .strong(),
                                                )
                                                .frame(false),
                                            )
                                            .on_hover_text("Open output folder")
                                            .clicked()
                                        {
                                            if let Some(parent) = output_path.parent() {
                                                let folder = parent.to_owned();
                                                std::thread::spawn(move || {
                                                    let _ = Command::new("xdg-open")
                                                        .arg(folder)
                                                        .status();
                                                });
                                            }
                                        }
                                    } else {
                                        ui.label(
                                            egui::RichText::new(output)
                                                .color(PRIMARY)
                                                .size(17.0)
                                                .strong(),
                                        );
                                    }
                                    ui.label(egui::RichText::new("→").color(MUTED).size(20.0));
                                },
                            );
                        });
                    });
                }
                if let Some(index) = to_remove {
                    self.jobs.remove(index);
                }
            });
    }

    fn mode_panel(&mut self, ui: &mut egui::Ui, width: f32, height: f32) {
        ui.allocate_ui_with_layout(
            Vec2::new(width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                panel().show(ui, |ui| {
                    ui.set_min_height(height - 34.0);
                    ui.label(egui::RichText::new("⚙  Mode").strong().size(17.0));
                    ui.add_space(4.0);
                    let specs = [
                        (Mode::Auto, "Smart"),
                        (Mode::Balanced, "Balanced CPU"),
                        (Mode::Fast, "Fast NVIDIA"),
                        (Mode::Manual, "Manual"),
                    ];
                    let tile_gap = 9.0;
                    let tile_width = ((ui.available_width() - 3.0 * tile_gap) / 4.0).max(68.0);
                    ui.spacing_mut().item_spacing.x = tile_gap;
                    ui.horizontal(|ui| {
                        for (mode, name) in specs {
                            let selected = self.settings.mode == mode;
                            let (rect, response) = ui.allocate_exact_size(
                                Vec2::new(tile_width, 87.0),
                                egui::Sense::click(),
                            );
                            let painter = ui.painter();
                            painter.rect(
                                rect,
                                11.0,
                                if selected {
                                    Color32::from_rgb(26, 61, 105)
                                } else {
                                    BG
                                },
                                Stroke::new(
                                    if selected { 1.7_f32 } else { 1.0_f32 },
                                    if selected { PRIMARY } else { BORDER },
                                ),
                                egui::StrokeKind::Inside,
                            );
                            let c = rect.center();
                            paint_mode_icon(
                                painter,
                                Pos2::new(c.x, rect.top() + 27.0),
                                mode,
                                if selected { PRIMARY } else { MUTED },
                            );
                            let label = if name == "Balanced CPU" {
                                "Balanced\nCPU"
                            } else if name == "Fast NVIDIA" {
                                "Fast\nNVIDIA"
                            } else {
                                name
                            };
                            painter.text(
                                Pos2::new(c.x, rect.top() + 58.0),
                                egui::Align2::CENTER_CENTER,
                                label,
                                FontId::proportional(12.5),
                                TEXT,
                            );
                            if response.clicked() && !self.running {
                                self.settings.mode = mode;
                            }
                            if response.hovered() && !self.running {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                        }
                    });
                    match self.settings.mode {
                        Mode::Manual => {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("CRF").color(MUTED).size(12.0));
                                ui.add_enabled(
                                    !self.running,
                                    egui::Slider::new(&mut self.settings.crf, 14..=35)
                                        .show_value(true),
                                );
                            });
                        }
                        Mode::Auto => {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new("Quality").color(MUTED).size(12.0));
                                ui.add_enabled(
                                    !self.running,
                                    egui::Slider::new(&mut self.settings.vmaf_target, 90.0..=98.0)
                                        .step_by(0.5),
                                );
                            });
                        }
                        _ => {
                            ui.add_space(30.0);
                        }
                    }
                });
            },
        );
    }

    fn output_panel(&mut self, ui: &mut egui::Ui, width: f32, height: f32, ctx: &egui::Context) {
        ui.allocate_ui_with_layout(
            Vec2::new(width, height),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                panel().show(ui, |ui| {
                    ui.set_min_height(height - 34.0);
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(Vec2::splat(19.0), egui::Sense::hover());
                        folder_symbol(ui.painter(), rect.center(), 0.43, MUTED);
                        ui.label(egui::RichText::new("Output").strong().size(17.0));
                    });
                    ui.add_space(15.0);
                    ui.horizontal(|ui| {
                        let text = self
                            .settings
                            .output_dir
                            .as_ref()
                            .map(|path| path.to_string_lossy().to_string())
                            .unwrap_or_else(|| "Same as source folder".to_owned());
                        egui::Frame::new()
                            .fill(BG)
                            .stroke(Stroke::new(1.0_f32, BORDER))
                            .corner_radius(9)
                            .inner_margin(egui::Margin::symmetric(13, 11))
                            .show(ui, |ui| {
                                let label_width = (ui.available_width() - 95.0).max(120.0);
                                ui.set_min_width(label_width);
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::Label::new(egui::RichText::new(&text).color(MUTED))
                                            .truncate(),
                                    )
                                    .on_hover_text(&text);
                                });
                            });
                        if ui
                            .add_enabled(
                                !self.running,
                                egui::Button::new("Choose…").min_size(Vec2::new(80.0, 40.0)),
                            )
                            .clicked()
                        {
                            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                                self.settings.output_dir = Some(path);
                            }
                            ctx.request_repaint();
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(
                                self.settings.output_dir.is_some() && !self.running,
                                egui::Button::new("Use source folder").small(),
                            )
                            .clicked()
                        {
                            self.settings.output_dir = None;
                        }
                    });
                    ui.add_enabled_ui(!self.running, |ui| {
                        ui.checkbox(
                            &mut self.settings.discard_if_larger,
                            "Keep only smaller results",
                        );
                    });
                });
            },
        );
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        let footer = egui::Frame::new()
            .fill(PANEL)
            .inner_margin(egui::Margin::symmetric(16, 13));
        footer.show(ui, |ui| {
            ui.horizontal(|ui| {
                let count = self.jobs.len();
                ui.label(
                    egui::RichText::new(format!(
                        "{count} video{}",
                        if count == 1 { "" } else { "s" }
                    ))
                    .color(MUTED)
                    .size(14.0),
                );
                if !self.running
                    && !self.jobs.is_empty()
                    && ui.add(egui::Button::new("Clear").frame(false)).clicked()
                {
                    self.jobs.clear();
                    self.hint.clear();
                    self.progress = 0.0;
                }
                if self.running {
                    ui.add_space(15.0);
                    ui.label(
                        egui::RichText::new(format!("{} / {}", self.completed, count)).color(MUTED),
                    );
                    let bar_width = (ui.available_width() - 320.0).max(100.0);
                    ui.add(
                        egui::ProgressBar::new(self.progress)
                            .desired_width(bar_width)
                            .show_percentage(),
                    );
                } else if !self.hint.is_empty() {
                    ui.label(egui::RichText::new(&self.hint).color(MUTED).size(12.5));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.running {
                        if ui
                            .add(egui::Button::new("■  Cancel").min_size(Vec2::new(170.0, 46.0)))
                            .clicked()
                        {
                            self.cancel.store(true, Ordering::Relaxed);
                            self.hint = "Cancelling…".to_owned();
                        }
                    } else {
                        let button = egui::Button::new(
                            egui::RichText::new("Compress")
                                .size(17.0)
                                .strong()
                                .color(Color32::WHITE),
                        )
                        .fill(PRIMARY)
                        .stroke(Stroke::NONE)
                        .corner_radius(11)
                        .min_size(Vec2::new(212.0, 48.0));
                        let response = ui.add_enabled(!self.jobs.is_empty(), button);
                        draw_play(
                            ui.painter(),
                            Pos2::new(response.rect.left() + 37.0, response.rect.center().y),
                            10.0,
                            Color32::WHITE,
                        );
                        if response.clicked() {
                            self.start();
                        }
                    }
                });
            });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_preview(ctx);
        self.poll();
        if self.running {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if !self.running {
            let drops: Vec<PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|d| d.path.clone())
                    .collect()
            });
            if !drops.is_empty() {
                self.add_paths(drops, ctx);
            }
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG))
            .show(ctx, |ui| {
                let width = ui.available_width();
                ui.spacing_mut().item_spacing.y = 13.0;
                ui.add_space(13.0);
                ui.add_space(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    self.header(ui);
                });
                ui.add_space(3.0);
                let content_width = (width - 36.0).max(650.0);
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(content_width, 164.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            self.drop_panel(ui, ctx, hovering);
                        },
                    );
                });
                let queue_height = (ui.available_height() - 315.0).max(100.0);
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    ui.allocate_ui_with_layout(
                        Vec2::new(content_width, queue_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            self.queue(ui, queue_height);
                        },
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_space(18.0);
                    let gap = 15.0;
                    let mode_width = (content_width * 0.53 - gap / 2.0).max(350.0);
                    let output_width = (content_width - mode_width - gap).max(280.0);
                    self.mode_panel(ui, mode_width, 181.0);
                    ui.add_space(gap);
                    self.output_panel(ui, output_width, 181.0, ctx);
                });
                ui.add_space(2.0);
                self.footer(ui);
            });
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn dotted_frame(p: &egui::Painter, rect: Rect, stroke: Stroke) {
    let rect = rect.shrink(1.0);
    let dash = 9.0;
    let gap = 6.0;
    for (from, to) in [
        (rect.left_top(), rect.right_top()),
        (rect.right_top(), rect.right_bottom()),
        (rect.right_bottom(), rect.left_bottom()),
        (rect.left_bottom(), rect.left_top()),
    ] {
        let delta = to - from;
        let length = delta.length();
        if length <= 0.0 {
            continue;
        }
        let dir = delta / length;
        let mut distance = 0.0;
        while distance < length {
            p.line_segment(
                [
                    from + dir * distance,
                    from + dir * (distance + dash).min(length),
                ],
                stroke,
            );
            distance += dash + gap;
        }
    }
}

fn draw_play(p: &egui::Painter, center: Pos2, radius: f32, color: Color32) {
    p.add(egui::Shape::convex_polygon(
        vec![
            Pos2::new(center.x - radius * 0.55, center.y - radius),
            Pos2::new(center.x + radius, center.y),
            Pos2::new(center.x - radius * 0.55, center.y + radius),
        ],
        color,
        Stroke::NONE,
    ));
}

fn paint_mode_icon(p: &egui::Painter, center: Pos2, mode: Mode, color: Color32) {
    let line = Stroke::new(1.8_f32, color);
    match mode {
        Mode::Auto => {
            p.circle_stroke(center, 11.0, line);
            p.circle_stroke(center, 6.0, line);
            p.circle_filled(center, 2.5, color);
        }
        Mode::Balanced => {
            p.rect_stroke(
                Rect::from_center_size(center, Vec2::splat(19.0)),
                2.0,
                line,
                egui::StrokeKind::Inside,
            );
            for offset in [-7.0, 0.0, 7.0] {
                p.line_segment(
                    [
                        center + Vec2::new(offset, -14.0),
                        center + Vec2::new(offset, -10.0),
                    ],
                    line,
                );
                p.line_segment(
                    [
                        center + Vec2::new(offset, 10.0),
                        center + Vec2::new(offset, 14.0),
                    ],
                    line,
                );
                p.line_segment(
                    [
                        center + Vec2::new(-14.0, offset),
                        center + Vec2::new(-10.0, offset),
                    ],
                    line,
                );
                p.line_segment(
                    [
                        center + Vec2::new(10.0, offset),
                        center + Vec2::new(14.0, offset),
                    ],
                    line,
                );
            }
        }
        Mode::Fast => {
            let points = [
                center + Vec2::new(-15.0, 0.0),
                center + Vec2::new(-8.0, -6.0),
                center + Vec2::new(0.0, -9.0),
                center + Vec2::new(8.0, -6.0),
                center + Vec2::new(15.0, 0.0),
                center + Vec2::new(8.0, 6.0),
                center + Vec2::new(0.0, 9.0),
                center + Vec2::new(-8.0, 6.0),
                center + Vec2::new(-15.0, 0.0),
            ];
            for pair in points.windows(2) {
                p.line_segment([pair[0], pair[1]], line);
            }
            p.circle_stroke(center, 4.7, line);
        }
        Mode::Manual => {
            for (y, knob) in [(-9.0, -5.0), (0.0, 7.0), (9.0, -2.0)] {
                p.line_segment(
                    [center + Vec2::new(-14.0, y), center + Vec2::new(14.0, y)],
                    line,
                );
                p.circle_filled(center + Vec2::new(knob, y), 3.5, PANEL);
                p.circle_stroke(center + Vec2::new(knob, y), 3.5, line);
            }
        }
    }
}

fn folder_symbol(p: &egui::Painter, center: Pos2, scale: f32, color: Color32) {
    let w = 40.0 * scale;
    let h = 27.0 * scale;
    let pos = center - Vec2::new(w / 2.0, h / 2.0);
    let rect = Rect::from_min_size(pos, Vec2::new(w, h));
    p.rect_stroke(
        rect,
        5.0,
        Stroke::new(1.8_f32, color),
        egui::StrokeKind::Inside,
    );
    p.line_segment(
        [
            Pos2::new(pos.x + 1.0, pos.y + 7.0),
            Pos2::new(pos.x + 14.0, pos.y + 7.0),
        ],
        Stroke::new(1.8_f32, color),
    );
}

fn install_korean_font(ctx: &egui::Context) {
    let candidates = [
        "/usr/share/fonts/TTF/NanumGothic.ttf",
        "/usr/share/fonts/truetype/nanum/NanumGothic.ttf",
        "/usr/share/fonts/noto/NotoSansKR-Regular.ttf",
    ];
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "local-hangul".to_owned(),
            egui::FontData::from_owned(bytes).into(),
        );
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(names) = fonts.families.get_mut(&family) {
                names.push("local-hangul".to_owned());
            }
        }
        ctx.set_fonts(fonts);
        break;
    }
}
