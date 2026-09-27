use egui::{Color32, RichText, Stroke, pos2, vec2};
use kasina_counting::{
    engine::{Phase, Settings, Snapshot, Speaker},
    model,
    runtime::{Command, Controller, Status},
};
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    time::Duration,
};

const MINT: Color32 = Color32::from_rgb(113, 220, 191);
const YOU: Color32 = Color32::from_rgb(242, 153, 184);
const INK: Color32 = Color32::from_rgb(23, 35, 39);
const COLORS: [Color32; 4] = [
    MINT,
    Color32::from_rgb(173, 163, 239),
    Color32::from_rgb(239, 185, 130),
    Color32::from_rgb(133, 194, 231),
];
const NAMES: [&str; 4] = ["Willow", "Iris", "Amber", "River"];

#[derive(Debug)]
pub(crate) struct CountingPanel {
    runtime: Controller,
    cache: PathBuf,
    model: Option<PathBuf>,
    download: Option<Receiver<Result<PathBuf, String>>>,
    download_error: Option<String>,
}
impl CountingPanel {
    pub fn new() -> Self {
        let cache = directories::ProjectDirs::from("org", "newkasina", "newKasina").map_or_else(
            || std::env::temp_dir().join("newKasina-speech"),
            |dirs| dirs.cache_dir().join("speech"),
        );
        Self {
            runtime: Controller::new(),
            model: model::find(&cache),
            cache,
            download: None,
            download_error: None,
        }
    }
    pub fn stop(&self) {
        self.runtime.send(Command::Stop);
    }
    pub fn has_model(&self) -> bool {
        self.model.is_some()
    }
    pub fn active(&self) -> bool {
        let state = self.runtime.status();
        state.loading || state.snapshot.phase == Phase::Counting
    }
    pub fn shortcut(&self, context: &egui::Context) {
        if self.runtime.status().snapshot.phase == Phase::Counting
            && context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Space))
        {
            self.runtime.send(Command::ManualCount);
        }
    }
    pub fn indicator(&self, ui: &mut egui::Ui) {
        let state = self.runtime.status();
        if state.loading || state.snapshot.phase == Phase::Counting {
            ui.separator();
            ui.colored_label(
                MINT,
                format!("Counting · {}", clock(state.snapshot.remaining_seconds)),
            );
            if ui.small_button("Stop counting").clicked() {
                self.runtime.send(Command::Stop);
            }
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
    pub fn ui(&mut self, ui: &mut egui::Ui, settings: &mut Settings) -> bool {
        if let Some(receiver) = &self.download
            && let Ok(result) = receiver.try_recv()
        {
            match result {
                Ok(path) => {
                    self.model = Some(path);
                    self.download_error = None;
                }
                Err(error) => self.download_error = Some(error),
            }
            self.download = None;
        }
        let state = self.runtime.status();
        let running = state.snapshot.phase == Phase::Counting;
        let editable = !running && !state.loading && state.snapshot.phase == Phase::Ready;
        let before = settings.clone();
        ui.ctx().request_repaint_after(Duration::from_millis(50));
        egui::ScrollArea::vertical().show(ui,|ui| {
            ui.add_space(12.0);
            ui.label(RichText::new("Aided breath counting").size(29.0));
            ui.label(RichText::new("Your own breath. A little company. One shared count.").color(Color32::from_gray(160)));
            ui.add_space(20.0);
            egui::Frame::new().fill(INK).corner_radius(22).inner_margin(24).show(ui,|ui| {
                ui.set_min_width((ui.available_width()-1.0).max(0.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(match state.snapshot.phase { Phase::Ready=>"COME TOGETHER", Phase::Counting=>"COUNTING TOGETHER", Phase::Quiet=>"ROOM FOR SILENCE" }).size(11.0).color(MINT));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center),|ui| {
                        ui.label(RichText::new(if running {format!("{} remaining",clock(state.snapshot.remaining_seconds))} else if state.snapshot.phase==Phase::Quiet {"Guidance complete".into()} else {format!("{} minutes",settings.duration_minutes)}).color(Color32::from_gray(180)));
                    });
                });
                ui.add_space(12.0);
                ui.vertical_centered(|ui| {
                    let number=if state.snapshot.last_number==0 {"—".into()} else {state.snapshot.last_number.to_string()};
                    ui.label(RichText::new(number).size(104.0).color(Color32::from_rgb(230,242,237)));
                    let speaker=match state.snapshot.last_speaker { Some(Speaker::You)=>"You".into(), Some(Speaker::Companion(index))=>NAMES[index].into(), None=>"Breathe naturally; join whenever you wish".to_owned() };
                    ui.label(RichText::new(speaker).size(16.0).color(state.snapshot.last_speaker.map_or(MINT, speaker_color)));
                    ui.add_space(10.0);
                    count_history(ui, &state.snapshot);
                });
                ui.add_space(24.0);
                ui.columns(settings.companions,|columns| {
                    for (index,column) in columns.iter_mut().enumerate() {
                        column.vertical_centered(|ui| {
                            let (rect,_)=ui.allocate_exact_size(vec2(92.0,92.0),egui::Sense::hover());
                            let phase=if running {state.snapshot.breath_phase[index] as f32}else{0.0};
                            let swell=0.5-(phase*std::f32::consts::TAU).cos()*0.5;
                            let color=COLORS[index]; let radius=24.0+swell*9.0;
                            ui.painter().circle_stroke(rect.center(),41.0,Stroke::new(1.0,color.gamma_multiply(0.25)));
                            ui.painter().circle_filled(rect.center(),radius+5.0,color.gamma_multiply(0.07));
                            ui.painter().circle_filled(rect.center(),radius,color.gamma_multiply(0.16));
                            ui.painter().circle_stroke(rect.center(),radius,Stroke::new(1.6,color.gamma_multiply(0.8)));
                            ui.painter().circle_filled(rect.center()+vec2(-6.0,-2.0),1.6,color);
                            ui.painter().circle_filled(rect.center()+vec2(6.0,-2.0),1.6,color);
                            ui.label(RichText::new(NAMES[index]).color(color));
                            let cycle=if running {state.snapshot.cycle_seconds[index]}else{settings.cycle_at(0.0,index)};
                            ui.label(RichText::new(format!("{cycle:.1} s / breath")).size(11.0).color(Color32::from_gray(150)));
                        });
                    }
                });
                ui.add_space(18.0);
                if running {
                    let fraction=(state.snapshot.remaining_seconds/(settings.duration_minutes*60.0)).clamp(0.0,1.0) as f32;
                    ui.add(egui::ProgressBar::new(fraction).desired_height(3.0).fill(MINT.gamma_multiply(0.6)));
                }
            });
            ui.add_space(16.0);
            ui.horizontal_wrapped(|ui| {
                if running || state.loading {
                    if ui.add_sized([138.0,40.0],egui::Button::new("Stop session")).clicked() {self.runtime.send(Command::Stop);}
                } else if state.snapshot.phase==Phase::Ready {
                    if ui.add_enabled(self.model.is_some() && self.download.is_none(),egui::Button::new(RichText::new("Start together").color(INK)).fill(MINT).min_size(vec2(150.0,40.0))).clicked() {
                        self.runtime.send(Command::Start(settings.clone(),self.model.clone().unwrap()));
                    }
                } else if ui.add_sized([138.0,40.0],egui::Button::new("End session")).clicked() {self.runtime.send(Command::Stop);}
                if state.snapshot.phase!=Phase::Ready {
                    if ui.add_sized([150.0,40.0],egui::Button::new(format!("+ {} min together",settings.extension_minutes))).clicked() {self.runtime.send(Command::Extend);}
                    if ui.add_enabled(running,egui::Button::new("I counted · Space").min_size(vec2(145.0,40.0))).clicked() {self.runtime.send(Command::ManualCount);}
                    if ui.add_enabled(running,egui::Button::new("Reset to 1")).on_hover_text("The next person will say one.").clicked() {self.runtime.send(Command::ResetCount);}
                }
            });
            ui.add_space(10.0);
            self.listening(ui,&state);
            if let Some(error)=&state.error {ui.colored_label(Color32::from_rgb(244,168,140),error);}
            if self.model.is_none() || self.download.is_some() || self.download_error.is_some() || state.error.as_ref().is_some_and(|error|error.contains("speech model") || error.contains("Load local speech recognition")) {
                ui.add_space(10.0);
                ui.label("English speech recognition runs entirely on your computer. Its 32 MB model is needed once.");
                if self.download.is_some() {ui.horizontal(|ui|{ui.spinner();ui.label("Downloading the English model…");});}
                else if ui.button("Download speech recognition").clicked() {
                    let (sender,receiver)=mpsc::channel(); let cache=self.cache.clone();
                    std::thread::spawn(move || {let _=sender.send(model::download(&cache).map_err(|e|format!("{e:#}")));});
                    self.download=Some(receiver);self.download_error=None;
                }
                if let Some(error)=&self.download_error {ui.colored_label(Color32::LIGHT_RED,error);}
            }
            ui.add_space(22.0);
            ui.add_enabled_ui(editable,|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label("Companions");ui.add(egui::Slider::new(&mut settings.companions,1..=4));
                    ui.separator();ui.label("Together for");ui.add(egui::DragValue::new(&mut settings.duration_minutes).range(0.1..=180.0).speed(0.5).suffix(" min"));
                    ui.separator();ui.label("Extra time");ui.add(egui::DragValue::new(&mut settings.extension_minutes).range(0.1..=30.0).speed(0.5).suffix(" min"));
                });
                ui.add_space(14.0);
                egui::CollapsingHeader::new("Breathing pattern").default_open(true).show(ui,|ui| {
                    ui.label(RichText::new("Companions settle at different speeds. Follow your own breath; these are their rhythms.").color(Color32::from_gray(165)));
                    curve(ui,settings);
                    ui.horizontal_wrapped(|ui| {
                        for (label,value) in ["At the start","Halfway","Once settled"].into_iter().zip(&mut settings.cycle_seconds) {
                            ui.label(label);ui.add(egui::DragValue::new(value).range(3.0..=30.0).speed(0.1).suffix(" s"));ui.add_space(8.0);
                        }
                        ui.label("Settle over");ui.add(egui::DragValue::new(&mut settings.settling_minutes).range(0.5..=120.0).speed(0.5).suffix(" min"));
                    });
                    ui.add_space(8.0);
                    for (index,name) in NAMES.iter().enumerate().take(settings.companions) {
                        ui.horizontal(|ui|{ui.colored_label(COLORS[index],*name);ui.add(egui::Slider::new(&mut settings.pace[index],0.65..=1.5).text("cycle length ×"));});
                    }
                });
                ui.add_space(12.0);
                egui::CollapsingHeader::new("Sound & microphone").show(ui,|ui| {
                    ui.add(egui::Slider::new(&mut settings.volume,0.0..=1.0).text("Voice & bell volume"));
                    ui.checkbox(&mut settings.speakers,"Using speakers · cancel companion echo");
                    ui.add(egui::Slider::new(&mut settings.microphone_threshold,0.001..=0.08).logarithmic(true).text("Speech threshold"));
                    ui.small("Lower the threshold for a quiet voice; raise it if room noise keeps the companions waiting.");
                    ui.small("Uses your system’s default microphone and speakers. Change devices in system sound settings before starting.");
                });
            });
            if !editable {ui.small("End the session to change its breathing pattern or audio settings.");}
            ui.add_space(14.0);
            ui.small("Speak one English number at the end of your outbreath. Everyone shares 1–10, then begins again at 1.");
            ui.small("The bell ends the guidance and closes the microphone. Add time to invite your companions back.");
        });
        settings.sanitize();
        *settings != before
    }
    fn listening(&self, ui: &mut egui::Ui, state: &Status) {
        ui.horizontal_wrapped(|ui| {
            let on = state.snapshot.phase == Phase::Counting;
            let (rect, _) = ui.allocate_exact_size(vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 3.0, if on { MINT } else { Color32::GRAY });
            ui.label(if state.loading {
                "Preparing local speech recognition…"
            } else if state.recognizing {
                "Listening · recognizing your number…"
            } else {
                &state.notice
            });
            if on {
                ui.add(
                    egui::ProgressBar::new((state.level * 20.0).clamp(0.0, 1.0))
                        .desired_width(75.0)
                        .desired_height(5.0)
                        .fill(MINT),
                )
                .on_hover_text(format!(
                    "Microphone: {}\nOutput: {}",
                    state.microphone, state.output
                ));
            }
            if state.loading {
                ui.spinner();
            }
        });
    }
}
fn speaker_color(speaker: Speaker) -> Color32 {
    match speaker {
        Speaker::You => YOU,
        Speaker::Companion(index) => COLORS[index],
    }
}

fn count_history(ui: &mut egui::Ui, snapshot: &Snapshot) {
    let spacing = 22.0_f32.min(ui.available_width() / 10.0);
    let visible_rows = snapshot.history.completed_rows + 1;
    let (rect, _) = ui.allocate_exact_size(
        vec2(spacing * 10.0, spacing * visible_rows as f32),
        egui::Sense::hover(),
    );
    let radius = spacing * 0.20;
    for (row_index, row) in snapshot.history.rows.iter().take(visible_rows).enumerate() {
        for (column, speaker) in row.iter().enumerate() {
            let center = rect.min
                + vec2(
                    (column as f32 + 0.5) * spacing,
                    (row_index as f32 + 0.5) * spacing,
                );
            let color = speaker.map_or(Color32::from_rgb(61, 85, 83), speaker_color);
            let latest_row = usize::from(snapshot.last_number == 10);
            let latest = snapshot.last_number != 0
                && row_index == latest_row
                && column + 1 == usize::from(snapshot.last_number);
            if latest {
                ui.painter().circle_stroke(
                    center,
                    radius + 3.0,
                    Stroke::new(1.0, color.gamma_multiply(0.45)),
                );
            }
            ui.painter().circle_filled(
                center,
                if speaker.is_some() {
                    radius
                } else {
                    radius * 0.6
                },
                color,
            );
            let name = match speaker {
                Some(Speaker::You) => "You",
                Some(Speaker::Companion(index)) => NAMES[*index],
                None => "Not counted",
            };
            let round = if row_index == 0 {
                "Current round".to_owned()
            } else if row_index == 1 {
                "Previous round".to_owned()
            } else {
                format!("{row_index} rounds ago")
            };
            ui.interact(
                egui::Rect::from_center_size(center, vec2(spacing, spacing)),
                ui.id().with(("count-history", row_index, column)),
                egui::Sense::hover(),
            )
            .on_hover_text(format!("{round} · {} · {name}", column + 1));
        }
    }
}

fn clock(seconds: f64) -> String {
    let total = seconds.ceil().max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}
fn curve(ui: &mut egui::Ui, settings: &Settings) {
    let (rect, _) = ui.allocate_exact_size(
        vec2(ui.available_width().min(950.0), 115.0),
        egui::Sense::hover(),
    );
    let plot = rect.shrink2(vec2(8.0, 15.0));
    let max = settings.cycle_seconds.into_iter().fold(0.0_f64, f64::max) * 1.5;
    for (index, color) in COLORS.iter().enumerate().take(settings.companions) {
        let points = (0..=80)
            .map(|step| {
                let fraction = step as f32 / 80.0;
                pos2(
                    plot.left() + fraction * plot.width(),
                    plot.bottom()
                        - (settings
                            .cycle_at(fraction as f64 * settings.settling_minutes * 60.0, index)
                            / max) as f32
                            * plot.height(),
                )
            })
            .collect();
        ui.painter()
            .add(egui::Shape::line(points, Stroke::new(1.8, *color)));
    }
    ui.painter().text(
        pos2(plot.left(), rect.bottom()),
        egui::Align2::LEFT_BOTTOM,
        "Start",
        egui::FontId::proportional(10.0),
        Color32::GRAY,
    );
    ui.painter().text(
        pos2(plot.right(), rect.bottom()),
        egui::Align2::RIGHT_BOTTOM,
        format!("{} min · longer cycles ↑", settings.settling_minutes),
        egui::FontId::proportional(10.0),
        Color32::GRAY,
    );
}
