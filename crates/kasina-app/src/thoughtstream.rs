//! ThoughtStream session display and the original pyThoughtstream `tts2` cue rules.
mod delta;

use egui::{Color32, RichText, Vec2};
use kasina_audio::{FeedbackCue, FeedbackPlayer};
use kasina_domain::quality;
use kasina_protocol::v1::Sample;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const VIOLET: Color32 = Color32::from_rgb(190, 151, 255);
const MINT: Color32 = Color32::from_rgb(99, 222, 193);
const AMBER: Color32 = Color32::from_rgb(255, 187, 111);
const INVALID: u32 = quality::SOURCE_INVALID | quality::STALE | quality::PROBE_ERROR;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct ThoughtStreamSettings {
    pub average_seconds: f64,
    pub cue_seconds: f64,
    pub slowdown_kohm: f64,
    pub max_pause_seconds: f64,
    pub warning_drop_kohm: f64,
    pub volume: f32,
}
impl Default for ThoughtStreamSettings {
    fn default() -> Self {
        Self {
            average_seconds: 1.0,
            cue_seconds: 1.0,
            slowdown_kohm: 100.0,
            max_pause_seconds: 7.0,
            warning_drop_kohm: 4.0,
            volume: 0.5,
        }
    }
}
impl ThoughtStreamSettings {
    pub fn sanitize(&mut self) {
        fn bounded(value: f64, default: f64, min: f64, max: f64) -> f64 {
            if value.is_finite() {
                value.clamp(min, max)
            } else {
                default
            }
        }
        self.average_seconds = bounded(self.average_seconds, 1.0, 0.25, 5.0);
        self.cue_seconds = bounded(self.cue_seconds, 1.0, 0.25, 10.0);
        self.slowdown_kohm = bounded(self.slowdown_kohm, 100.0, 10.0, 1000.0);
        self.max_pause_seconds = bounded(self.max_pause_seconds, 7.0, self.cue_seconds, 30.0);
        self.warning_drop_kohm = bounded(self.warning_drop_kohm, 4.0, 0.1, 100.0);
        self.volume = bounded(f64::from(self.volume), 0.5, 0.0, 1.0) as f32;
    }
    fn interval(&self, kohm: f64) -> f64 {
        if kohm < self.slowdown_kohm {
            self.cue_seconds
        } else {
            (self.cue_seconds * (1.0 + kohm / self.slowdown_kohm)).min(self.max_pause_seconds)
        }
    }
}

#[derive(Debug, Default)]
struct CueRules {
    reference: Option<f64>,
    last_regular: Option<Instant>,
}
impl CueRules {
    fn observe(
        &mut self,
        kohm: f64,
        now: Instant,
        settings: &ThoughtStreamSettings,
    ) -> Option<FeedbackCue> {
        let Some(reference) = self.reference else {
            self.reference = Some(kohm);
            self.last_regular = Some(now);
            return None;
        };
        let cue = if kohm < reference - settings.warning_drop_kohm {
            // Warnings bypass the timer but do not postpone the next regular cue.
            FeedbackCue::Warning
        } else if now.duration_since(self.last_regular.unwrap()).as_secs_f64()
            >= settings.interval(kohm)
        {
            self.last_regular = Some(now);
            if kohm > reference {
                FeedbackCue::Up
            } else {
                FeedbackCue::Down
            }
        } else {
            return None;
        };
        self.reference = Some(kohm);
        Some(cue)
    }
}

#[derive(Debug)]
pub(crate) struct ThoughtStreamPanel {
    player: FeedbackPlayer,
    enabled: bool,
    active: bool,
    identity: String,
    sequence: u64,
    last_update: Instant,
    sum: f64,
    count: usize,
    rules: CueRules,
    value: Option<f64>,
    delta: Option<f64>,
    history: VecDeque<(Instant, Option<f64>)>,
    status: &'static str,
    low_battery: bool,
}
impl ThoughtStreamPanel {
    pub fn new(now: Instant) -> Self {
        Self {
            player: FeedbackPlayer::default(),
            enabled: false,
            active: false,
            identity: String::new(),
            sequence: 0,
            last_update: now,
            sum: 0.0,
            count: 0,
            rules: CueRules::default(),
            value: None,
            delta: None,
            history: VecDeque::new(),
            status: "Waiting for ThoughtStream",
            low_battery: false,
        }
    }
    fn reset_window(&mut self, now: Instant) {
        self.sum = 0.0;
        self.count = 0;
        self.last_update = now;
        self.rules = CueRules::default();
        self.value = None;
        self.delta = None;
        self.player.stop();
        if self
            .history
            .back()
            .is_some_and(|(_, value)| value.is_some())
        {
            self.history.push_back((now, None));
        }
    }
    pub fn deactivate(&mut self, now: Instant) {
        if self.active || self.enabled {
            self.enabled = false;
            self.active = false;
            self.reset_window(now);
        }
    }
    pub fn update(
        &mut self,
        samples: Option<&VecDeque<Sample>>,
        adc: Option<&Sample>,
        session: &str,
        now: Instant,
        wall_ns: u64,
        settings: &ThoughtStreamSettings,
    ) {
        let Some(samples) = samples.filter(|samples| !samples.is_empty()) else {
            self.reset_window(now);
            self.status = "Waiting for ThoughtStream";
            return;
        };
        let latest = samples.back().unwrap();
        let identity = format!("{session}/{}", latest.source_id);
        if !self.active || identity != self.identity || latest.sequence < self.sequence {
            self.reset_window(now);
            self.history.clear();
            self.active = true;
            self.identity = identity;
            // Never turn retained history into audible feedback when entering the panel.
            self.sequence = latest.sequence;
        }
        let latest_flags = adc
            .filter(|adc| adc.wall_time_unix_ns >= latest.wall_time_unix_ns)
            .map_or(latest.quality_flags, |adc| adc.quality_flags);
        self.low_battery = latest_flags & quality::LOW_BATTERY != 0;
        let stale = wall_ns.abs_diff(latest.wall_time_unix_ns) > 3_000_000_000;
        if stale || latest_flags & INVALID != 0 {
            self.sequence = latest.sequence;
            self.reset_window(now);
            self.status = if latest_flags & quality::PROBE_ERROR != 0 {
                "Check the finger sensors"
            } else if stale || latest_flags & quality::STALE != 0 {
                "Waiting for fresh readings"
            } else {
                "Sensor reading unavailable"
            };
            return;
        }
        self.status = if latest_flags & quality::SIMULATED != 0 {
            "Simulated signal"
        } else {
            "Live signal"
        };
        if now.duration_since(self.last_update)
            > Duration::from_secs_f64(settings.average_seconds + 3.0)
        {
            self.reset_window(now);
            self.sequence = latest.sequence;
        }
        let previous_sequence = self.sequence;
        let mut expected_sequence = previous_sequence.saturating_add(1);
        for sample in samples
            .iter()
            .filter(|sample| sample.sequence > previous_sequence)
        {
            if sample.sequence != expected_sequence {
                self.reset_window(now);
            }
            expected_sequence = sample.sequence.saturating_add(1);
            if sample.quality_flags & (INVALID | quality::AFTER_GAP | quality::RECALIBRATED) != 0 {
                self.reset_window(now);
            }
            if sample.quality_flags & INVALID == 0
                && sample.value.is_finite()
                && sample.value > 0.0
                && wall_ns.abs_diff(sample.wall_time_unix_ns) <= 3_000_000_000
            {
                self.sum += sample.value / 1000.0;
                self.count += 1;
            }
        }
        self.sequence = latest.sequence;
        if now.duration_since(self.last_update).as_secs_f64() >= settings.average_seconds {
            self.last_update = now;
            if self.count > 0 {
                let value = self.sum / self.count as f64;
                self.delta = self.value.map(|previous| value - previous);
                self.value = Some(value);
                self.history.push_back((now, Some(value)));
                if self.enabled
                    && let Some(cue) = self.rules.observe(value, now, settings)
                {
                    self.player.play(cue, settings.volume);
                }
                self.sum = 0.0;
                self.count = 0;
            }
        }
        while self
            .history
            .front()
            .is_some_and(|(time, _)| now.duration_since(*time).as_secs() > 180)
        {
            self.history.pop_front();
        }
        if self.player.error().is_some() {
            self.enabled = false;
            self.rules = CueRules::default();
        }
    }
    fn toggle(&mut self) {
        self.enabled = !self.enabled;
        self.rules = CueRules::default();
        self.player.stop();
        self.player.clear_error();
    }
    pub fn ui(&mut self, ui: &mut egui::Ui, settings: &mut ThoughtStreamSettings) -> bool {
        if !ui.ctx().text_edit_focused() {
            let pressed = ui.input_mut(|input| {
                let first_press = input.events.iter().any(|event| matches!(event,
                    egui::Event::Key { key: egui::Key::Space, pressed: true, repeat: false, modifiers, .. } if modifiers.is_none()));
                let consumed = input.consume_key(egui::Modifiers::NONE, egui::Key::Space);
                first_press && consumed
            });
            if pressed {
                self.toggle();
            }
        }
        let mut changed = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.heading(RichText::new("ThoughtStream").size(27.0).color(Color32::from_rgb(235, 230, 244)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.colored_label(if self.value.is_some() { MINT } else { ui.visuals().weak_text_color() }, self.status);
                });
            });
            ui.label(RichText::new("A quiet space for skin-resistance feedback").color(Color32::from_gray(150)));
            ui.add_space(18.0);
            egui::Frame::new().fill(Color32::from_rgb(29, 26, 40)).corner_radius(18).inner_margin(24.0).show(ui, |ui| {
                ui.set_min_width((ui.available_width()).max(0.0));
                let delta_text = self.delta.map_or_else(|| "—".into(), |v| format!("{v:+.2}"));
                let absolute_text = self.value.map_or_else(|| "—".into(), |v| format!("{v:.1}"));
                let column_width = (ui.available_width() - 28.0) * 0.5;
                let characters = delta_text.len().max(absolute_text.len()) as f32;
                let font_size = (column_width / (characters * 0.65)).clamp(24.0, 92.0);
                ui.spacing_mut().item_spacing.x = 28.0;
                ui.columns(2, |columns| {
                    delta::readout(&mut columns[0], self.delta, true, font_size, settings.average_seconds);
                    delta::readout(&mut columns[1], self.value, false, font_size, settings.average_seconds);
                });
                ui.add_space(22.0);
                let now = Instant::now();
                ui.columns(2, |columns| {
                    delta::show(&mut columns[0], &self.history, true, self.delta, now);
                    delta::show(&mut columns[1], &self.history, false, self.value, now);
                });
                ui.add_space(8.0);
                ui.label(RichText::new("Scale: middle 60% of values + every reading from the last 10 s").size(10.0).weak())
                    .on_hover_text("Each graph uses its 20th–80th percentile range, expanded to include all readings from the latest 10 seconds. Delta shading continues to the plot edges for out-of-range readings; the absolute trace omits older outliers. Recordings and sound feedback are unchanged.");
            });
            ui.add_space(14.0);
            ui.horizontal_wrapped(|ui| {
                let label = if self.enabled { "Sound on · click to mute" } else { "Sound off · click to enable" };
                if ui.add(egui::Button::new(RichText::new(label).size(15.0)).min_size(Vec2::new(220.0, 36.0)).fill(if self.enabled { Color32::from_rgb(71, 49, 99) } else { Color32::from_rgb(48, 48, 54) })).clicked() { self.toggle(); }
                ui.label(RichText::new("Space to toggle").weak());
                changed |= ui.add(egui::Slider::new(&mut settings.volume, 0.0..=1.0).text("Volume")).changed();
            });
            if let Some(error) = self.player.error() { ui.colored_label(AMBER, error); }
            if self.low_battery { ui.colored_label(AMBER, "ThoughtStream battery is low."); }
            ui.label(RichText::new("Feedback is silent when this panel is closed or readings are unavailable.").small().weak());
            ui.add_space(14.0);
            ui.collapsing("Timing & feedback settings", |ui| {
                egui::Grid::new("thoughtstream_timing").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
                    for (label, value, range, suffix) in [
                        ("Average / display interval", &mut settings.average_seconds, 0.25..=5.0, " s"),
                        ("Base cue interval", &mut settings.cue_seconds, 0.25..=10.0, " s"),
                        ("Slowdown starts at", &mut settings.slowdown_kohm, 10.0..=1000.0, " kΩ"),
                        ("Maximum cue interval", &mut settings.max_pause_seconds, 0.25..=30.0, " s"),
                        ("Warn when resistance drops by more than", &mut settings.warning_drop_kohm, 0.1..=100.0, " kΩ"),
                    ] { ui.label(label); changed |= ui.add(egui::DragValue::new(value).range(range).speed(0.1).suffix(suffix)).changed(); ui.end_row(); }
                });
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label("Preview tones:");
                    for (label, cue) in [("Rise", FeedbackCue::Up), ("Fall", FeedbackCue::Down), ("Drop warning", FeedbackCue::Warning)] {
                        if ui.add_enabled(self.enabled, egui::Button::new(label)).on_hover_text("Enable sound above to preview this tone").clicked() {
                            self.player.play(cue, settings.volume);
                        }
                    }
                });
                ui.label("Above the slowdown threshold, the interval is base × (1 + resistance / threshold), capped at the maximum. Cues are evaluated after each average; warnings bypass the cue timer.");
                ui.label("Rise, fall and drop warnings compare with the last feedback reference. The first fresh average establishes that reference silently.");
                if ui.button("Restore Python timing defaults").clicked() {
                    let volume = settings.volume;
                    *settings = ThoughtStreamSettings { volume, ..Default::default() }; changed = true;
                }
            });
        });
        if changed {
            settings.sanitize();
            self.reset_window(Instant::now());
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_interval_and_warning_reference_are_preserved() {
        let config = ThoughtStreamSettings::default();
        for (value, seconds) in [(99.9, 1.0), (100.0, 2.0), (300.0, 4.0), (900.0, 7.0)] {
            assert_eq!(config.interval(value), seconds);
        }
        let mut rules = CueRules::default();
        let t = Instant::now();
        assert_eq!(rules.observe(300.0, t, &config), None);
        assert_eq!(
            rules.observe(299.0, t + Duration::from_secs(1), &config),
            None
        );
        assert_eq!(
            rules.observe(296.0, t + Duration::from_secs(2), &config),
            None
        ); // strict > 4 kΩ
        assert_eq!(
            rules.observe(295.0, t + Duration::from_secs(3), &config),
            Some(FeedbackCue::Warning)
        );
        assert_eq!(
            rules.observe(296.0, t + Duration::from_secs(4), &config),
            Some(FeedbackCue::Up)
        ); // warning didn't reset the clock
        assert_eq!(
            rules.observe(296.0, t + Duration::from_secs(8), &config),
            Some(FeedbackCue::Down)
        );
    }
    #[test]
    fn customized_cadence_cap_and_warning_threshold() {
        let config = ThoughtStreamSettings {
            cue_seconds: 2.0,
            slowdown_kohm: 200.0,
            max_pause_seconds: 5.0,
            warning_drop_kohm: 10.0,
            ..Default::default()
        };
        assert_eq!(config.interval(100.0), 2.0);
        assert_eq!(config.interval(200.0), 4.0);
        assert_eq!(config.interval(900.0), 5.0);
        let t = Instant::now();
        let mut rules = CueRules::default();
        rules.observe(100.0, t, &config);
        assert_eq!(
            rules.observe(89.0, t + Duration::from_millis(250), &config),
            Some(FeedbackCue::Warning)
        );
    }
    fn sample(seq: u64, ns: u64, kohm: f64, flags: u32) -> Sample {
        Sample {
            sequence: seq,
            source_id: "test".into(),
            wall_time_unix_ns: ns,
            value: kohm * 1000.0,
            quality_flags: flags,
            ..Default::default()
        }
    }
    #[test]
    fn averages_new_samples_and_rejects_history_staleness_and_bad_probes() {
        let t = Instant::now();
        let mut panel = ThoughtStreamPanel::new(t);
        let config = ThoughtStreamSettings::default();
        let mut samples = VecDeque::from([sample(1, 0, 500.0, 0)]);
        panel.update(Some(&samples), None, "session", t, 0, &config);
        samples.push_back(sample(2, 500_000_000, 100.0, 0));
        panel.update(
            Some(&samples),
            None,
            "session",
            t + Duration::from_millis(500),
            500_000_000,
            &config,
        );
        samples.push_back(sample(3, 1_000_000_000, 200.0, 0));
        panel.update(
            Some(&samples),
            None,
            "session",
            t + Duration::from_secs(1),
            1_000_000_000,
            &config,
        );
        assert_eq!(panel.value, Some(150.0)); // retained 500 excluded, samples aren't counted twice
        panel.update(
            Some(&samples),
            None,
            "session",
            t + Duration::from_secs(5),
            5_000_000_000,
            &config,
        );
        assert_eq!(panel.value, None);
        assert_eq!(panel.status, "Waiting for fresh readings");
        samples.push_back(sample(4, 6_000_000_000, 100.0, 0));
        let adc = sample(4, 6_000_000_000, 0.0, quality::PROBE_ERROR);
        panel.update(
            Some(&samples),
            Some(&adc),
            "session",
            t + Duration::from_secs(6),
            6_000_000_000,
            &config,
        );
        assert_eq!(panel.value, None);
        assert_eq!(panel.status, "Check the finger sensors");
    }
    #[test]
    fn gaps_and_new_sessions_reset_the_reference_and_leaving_mutes() {
        let t = Instant::now();
        let mut panel = ThoughtStreamPanel::new(t);
        let config = ThoughtStreamSettings::default();
        let mut samples = VecDeque::from([sample(1, 0, 500.0, 0)]);
        panel.update(Some(&samples), None, "first", t, 0, &config);
        panel.rules.reference = Some(900.0);
        samples.push_back(sample(2, 1_000_000_000, 100.0, quality::AFTER_GAP));
        panel.update(
            Some(&samples),
            None,
            "first",
            t + Duration::from_secs(1),
            1_000_000_000,
            &config,
        );
        assert_eq!(panel.rules.reference, None);
        panel.rules.reference = Some(800.0);
        panel.update(
            Some(&samples),
            None,
            "second",
            t + Duration::from_secs(1),
            1_000_000_000,
            &config,
        );
        assert_eq!(panel.rules.reference, None);
        panel.toggle();
        assert!(panel.enabled);
        panel.deactivate(t);
        assert!(!panel.enabled);
        panel.toggle();
        assert!(panel.enabled); // also works while no data has arrived
        panel.deactivate(t);
        assert!(!panel.enabled);
    }
    #[test]
    fn space_toggles_once_and_respects_text_editing() {
        let context = egui::Context::default();
        let mut panel = ThoughtStreamPanel::new(Instant::now());
        let mut settings = ThoughtStreamSettings::default();
        let key = |repeat| egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Space,
                physical_key: None,
                pressed: true,
                repeat,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut output = context.run_ui(Default::default(), |ui| {
            panel.ui(ui, &mut settings);
        });
        output.textures_delta.clear();
        let mut output = context.run_ui(key(false), |ui| {
            panel.ui(ui, &mut settings);
        });
        output.textures_delta.clear();
        assert!(panel.enabled);
        let mut output = context.run_ui(key(true), |ui| {
            panel.ui(ui, &mut settings);
        });
        output.textures_delta.clear();
        assert!(panel.enabled);
        let mut release = key(false);
        if let egui::Event::Key { pressed, .. } = &mut release.events[0] {
            *pressed = false;
        }
        let mut output = context.run_ui(release, |ui| {
            panel.ui(ui, &mut settings);
        });
        output.textures_delta.clear();
        let mut output = context.run_ui(key(false), |ui| {
            panel.ui(ui, &mut settings);
        });
        output.textures_delta.clear();
        assert!(!panel.enabled);
        let mut text = String::from("editing");
        let mut output = context.run_ui(Default::default(), |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).id(egui::Id::new("test-edit")))
                .request_focus();
        });
        output.textures_delta.clear();
        let mut output = context.run_ui(key(false), |ui| {
            panel.ui(ui, &mut settings);
            ui.add(egui::TextEdit::singleline(&mut text).id(egui::Id::new("test-edit")));
        });
        output.textures_delta.clear();
        assert!(!panel.enabled);
    }
    #[test]
    fn configuration_survives_serialization_and_invalid_numbers_are_bounded() {
        let mut config = ThoughtStreamSettings {
            average_seconds: f64::NAN,
            cue_seconds: 10.0,
            max_pause_seconds: -1.0,
            volume: f32::INFINITY,
            ..Default::default()
        };
        config.sanitize();
        assert_eq!(config.average_seconds, 1.0);
        assert_eq!(config.max_pause_seconds, 10.0);
        assert_eq!(config.volume, 0.5);
        let encoded = serde_json::to_vec(&config).unwrap();
        let decoded: ThoughtStreamSettings = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.cue_seconds, 10.0);
        let legacy: crate::settings::AppSettings =
            serde_json::from_str(r#"{"schema_version":8,"visible_tabs":{"breath_kasina":true}}"#)
                .unwrap();
        assert!(legacy.visible_tabs.thoughtstream);
        assert_eq!(legacy.thoughtstream.average_seconds, 1.0);
    }
}
