use eframe::egui;
use std::{
    io::{self, BufRead, Read, Write},
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Preferences {
    height: u32,
    fps: u32,
    bitrate: u32,
    seconds: u32,
    gpu: bool,
    microphone: bool,
    folder: String,
    display: u32,
}
impl Default for Preferences {
    fn default() -> Self {
        let folder = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Videos")
            .join("Instant Replay");
        Self {
            height: 0,
            fps: 60,
            bitrate: 0,
            seconds: 30,
            gpu: true,
            microphone: false,
            folder: folder.to_string_lossy().into_owned(),
            display: 0,
        }
    }
}
struct App {
    preferences: Preferences,
    child: Option<Child>,
    viewer: Option<Child>,
    viewer_error: String,
    logs: Option<mpsc::Receiver<String>>,
    status: String,
    stopping: bool,
    closing: bool,
    refresh: Option<u32>,
    checked_display: u32,
    refresh_checked: std::time::Instant,
}
impl App {
    fn close_viewer(&mut self) {
        if let Some(mut child) = self.viewer.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn open_clip(&mut self) -> io::Result<()> {
        let Some(file) = rfd::FileDialog::new()
            .set_title("Open clip")
            .set_directory(&self.preferences.folder)
            .add_filter("QuickTime movies", &["mov"])
            .pick_file()
        else {
            return Ok(());
        };
        let child = Command::new(std::env::current_exe()?)
            .arg("view")
            .arg(file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // CREATE_NO_WINDOW | BELOW_NORMAL_PRIORITY_CLASS
            .creation_flags(0x08000000 | 0x4000)
            .spawn()?;
        self.close_viewer();
        self.viewer = Some(child);
        self.viewer_error.clear();
        Ok(())
    }

    fn poll_viewer(&mut self) {
        let Some(child) = &mut self.viewer else {
            return;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut detail = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        let _ = stderr.read_to_string(&mut detail);
                    }
                    self.viewer_error = format!("Could not play clip. {}", detail.trim());
                }
                self.viewer = None;
            }
            Ok(None) => {}
            Err(err) => {
                self.viewer_error = err.to_string();
                self.close_viewer();
            }
        }
    }

    fn send(&mut self, command: &str) -> io::Result<()> {
        if let Some(input) = self.child.as_mut().and_then(|child| child.stdin.as_mut()) {
            writeln!(input, "{command}")?;
        }
        Ok(())
    }
    fn start(&mut self) -> io::Result<()> {
        let p = &self.preferences;
        if p.folder.trim().is_empty() {
            return Err(io::Error::other("Choose a clip folder."));
        }
        std::fs::create_dir_all(&p.folder)?;
        // Verify write access before starting the capture worker.
        let _probe = tempfile::NamedTempFile::new_in(&p.folder)?;
        let mut child = Command::new(std::env::current_exe()?)
            .args([
                "prores-replay",
                "--display",
                &p.display.to_string(),
                "--height",
                &p.height.to_string(),
                "--fps",
                &p.fps.to_string(),
                "--bitrate",
                &p.bitrate.to_string(),
                "--seconds",
                &p.seconds.to_string(),
                "--dir",
                &p.folder,
            ])
            .args(p.microphone.then_some("--microphone"))
            .args((!p.gpu).then_some("--cpu-only"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .creation_flags(0x08000000)
            .spawn()?;
        let error = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in io::BufReader::new(error).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        self.child = Some(child);
        self.logs = Some(rx);
        self.stopping = false;
        self.status = "Starting capture...".into();
        Ok(())
    }
    fn stop(&mut self) {
        match self.send("quit") {
            Ok(()) => {
                self.stopping = true;
                self.status = "Stopping and finishing saves...".into();
            }
            Err(e) => self.status = e.to_string(),
        }
    }
}
impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "settings", &self.preferences);
    }
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_viewer();
        if self.child.is_none()
            && (self.checked_display != self.preferences.display
                || self.refresh_checked.elapsed() > Duration::from_secs(2))
        {
            self.checked_display = self.preferences.display;
            self.refresh = crate::prores::desktop::refresh_rate(self.preferences.display).ok();
            self.refresh_checked = std::time::Instant::now();
            if let Some(cap) = self.refresh {
                self.preferences.fps = self.preferences.fps.min(cap);
            }
        }
        if let Some(logs) = &self.logs {
            for line in logs.try_iter() {
                self.status = line;
            }
        }
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(status)) => {
                    self.child = None;
                    self.logs = None;
                    self.stopping = false;
                    if status.success() {
                        self.status = "Stopped".into();
                    }
                    if self.closing {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Ok(None) => {}
                Err(e) => self.status = e.to_string(),
            }
        }
        if ctx.input(|i| i.viewport().close_requested()) && self.child.is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            if !self.stopping {
                self.stop();
            }
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.heading("Instant Replay");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Open clip").clicked() && let Err(error) = self.open_clip() {
                        self.viewer_error = error.to_string();
                    }
                });
            });
            if !self.viewer_error.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, &self.viewer_error);
            }
            ui.label("ProRes 422 HQ");
            ui.add_space(20.0);
            let running = self.child.is_some();
            ui.add_enabled_ui(!running, |ui| {
                egui::Grid::new("settings")
                    .spacing([24.0, 18.0])
                    .show(ui, |ui| {
                        ui.label("Display");
                        ui.add(egui::DragValue::new(&mut self.preferences.display).range(0..=15));
                        ui.end_row();
                        ui.label("Resolution");
                        egui::ComboBox::from_id_salt("resolution")
                            .width(200.0)
                            .selected_text(if self.preferences.height == 0 {
                                "Native".into()
                            } else {
                                format!("{}p", self.preferences.height)
                            })
                            .show_ui(ui, |ui| {
                                for (height, label) in [
                                    (0, "Native"),
                                    (2160, "2160p"),
                                    (1440, "1440p"),
                                    (1080, "1080p"),
                                    (720, "720p"),
                                    (480, "480p"),
                                ] {
                                    ui.selectable_value(
                                        &mut self.preferences.height,
                                        height,
                                        label,
                                    );
                                }
                            });
                        ui.end_row();
                        ui.label("FPS");
                        egui::ComboBox::from_id_salt("fps")
                            .width(200.0)
                            .selected_text(if self.preferences.fps == 0 {
                                "Monitor refresh".into()
                            } else {
                                self.preferences.fps.to_string()
                            })
                            .show_ui(ui, |ui| {
                                let cap = self.refresh.unwrap_or(60);
                                let mut rates = vec![
                                    0,
                                    24,
                                    30,
                                    60,
                                    90,
                                    120,
                                    144,
                                    165,
                                    180,
                                    240,
                                    360,
                                    480,
                                    cap,
                                    self.preferences.fps,
                                ];
                                rates.retain(|&fps| fps <= cap);
                                rates.sort_unstable();
                                rates.dedup();
                                for fps in rates {
                                    ui.selectable_value(
                                        &mut self.preferences.fps,
                                        fps,
                                        if fps == 0 {
                                            "Monitor refresh".into()
                                        } else {
                                            fps.to_string()
                                        },
                                    );
                                }
                            });
                        ui.end_row();
                        ui.label("Replay length");
                        ui.add(
                            egui::DragValue::new(&mut self.preferences.seconds)
                                .range(1..=300)
                                .suffix(" s"),
                        );
                        ui.end_row();
                        ui.label("Audio");
                        ui.checkbox(&mut self.preferences.microphone, "Record default microphone")
                            .on_hover_text("Desktop audio is always recorded. Enable this to mix in your Windows default microphone.");
                        ui.end_row();
                        ui.label("Encoder");
                        ui.checkbox(&mut self.preferences.gpu, "GPU acceleration");
                        ui.end_row();
                        ui.label("Target bitrate");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::DragValue::new(&mut self.preferences.bitrate)
                                    .range(0..=2000)
                                    .suffix(" Mbps"),
                            );
                            ui.small("0 = Auto HQ");
                        });
                        ui.end_row();
                    });
                ui.add_space(12.0);
                ui.small(match self.refresh {
                    Some(hz) => format!("Monitor refresh: {hz} Hz"),
                    None => "Refresh rate unavailable for this display.".into(),
                });
                ui.small("Resolution fits the display aspect ratio, up to native size.");
                ui.small("ProRes uses a variable bitrate. Lower targets reduce quality.");
                ui.add_space(18.0);
                ui.label("Clip folder");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.preferences.folder)
                            .desired_width(350.0),
                    );
                    if ui.button("Browse...").clicked()
                        && let Some(folder) = rfd::FileDialog::new()
                            .set_directory(&self.preferences.folder)
                            .pick_folder()
                    {
                        self.preferences.folder = folder.to_string_lossy().into_owned();
                    }
                });
            });
            ui.add_space(24.0);
            ui.horizontal(|ui| {
                if !running {
                    if ui
                        .add_sized([150.0, 36.0], egui::Button::new("Start replay"))
                        .clicked()
                        && let Err(e) = self.start()
                    {
                        self.status = e.to_string();
                    }
                } else {
                    if ui
                        .add_enabled(
                            !self.stopping,
                            egui::Button::new("Stop replay").min_size(egui::vec2(150.0, 36.0)),
                        )
                        .clicked()
                    {
                        self.stop();
                    }
                    if ui
                        .add_enabled(
                            !self.stopping,
                            egui::Button::new("Save clip (F5)").min_size(egui::vec2(150.0, 36.0)),
                        )
                        .clicked()
                        && let Err(e) = self.send("save")
                    {
                        self.status = e.to_string();
                    }
                }
            });
            ui.add_space(14.0);
            ui.label(&self.status);
            ui.add_space(10.0);
            ui.small("Minimize to keep recording. Closing this window stops replay.");
        });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.close_viewer();
        let _ = self.send("quit");
        if let Some(child) = &mut self.child {
            let _ = child.wait();
        }
    }
}
pub fn run() -> io::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 620.0])
            .with_min_inner_size([520.0, 620.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Instant Replay",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let mut preferences: Preferences = cc
                .storage
                .and_then(|s| eframe::get_value(s, "settings"))
                .unwrap_or_default();
            if ![0, 480, 720, 1080, 1440, 2160].contains(&preferences.height) {
                preferences.height = 0;
            }
            if preferences.fps > 1000 {
                preferences.fps = 60;
            }
            preferences.seconds = preferences.seconds.clamp(1, 300);
            preferences.bitrate = preferences.bitrate.min(2000);
            Ok(Box::new(App {
                preferences,
                child: None,
                viewer: None,
                viewer_error: String::new(),
                logs: None,
                status: "Ready".into(),
                stopping: false,
                closing: false,
                refresh: None,
                checked_display: u32::MAX,
                refresh_checked: std::time::Instant::now(),
            }))
        }),
    )
    .map_err(|e| io::Error::other(e.to_string()))
}
