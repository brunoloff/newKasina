//! Presentation and user intentions for the measurement service.
//!
//! The panel never starts hardware or makes requests itself: all controls use the
//! same service connection as the other panels, including a separate tray service.

use egui::{Color32, RichText, Stroke, Vec2};
use kasina_protocol::v1::{
    ConnectionState, DeviceKind, DeviceStatus, RecordingState, StatusSnapshot, StreamKind,
};

const MINT: Color32 = Color32::from_rgb(99, 222, 193);
const AMBER: Color32 = Color32::from_rgb(255, 192, 117);
const CORAL: Color32 = Color32::from_rgb(249, 128, 143);
const VIOLET: Color32 = Color32::from_rgb(190, 151, 255);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServiceMode {
    Offline,
    Starting,
    Embedded,
    External,
    Stopping,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct DeviceControl {
    pub id: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct SerialChoice {
    pub path: String,
    pub label: String,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SerialSettings {
    pub automatic: bool,
    pub selected_path: Option<String>,
    pub ports: Vec<SerialChoice>,
    pub detail: String,
}

pub(crate) struct ServicePanelInput<'a> {
    pub mode: ServiceMode,
    /// False immediately when the live connection drops, even if a snapshot remains.
    pub connected: bool,
    pub status: Option<&'a StatusSnapshot>,
    pub notice: Option<&'a str>,
    pub controls_available: bool,
    pub device_controls: &'a [DeviceControl],
    pub serial: Option<&'a SerialSettings>,
    pub busy: bool,
    pub can_start: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServiceAction {
    StartService,
    StopService,
    SetDeviceEnabled { device_id: String, enabled: bool },
    ReconnectDevice { device_id: String },
    RefreshSerialPorts,
    SetThoughtStreamPort(Option<String>),
    StartRecording { label: String },
    StopRecording,
    OpenRecordings,
}

#[derive(Default)]
pub(crate) struct ServicePanel {
    recording_label: String,
}

impl ServicePanel {
    pub fn ui(&mut self, ui: &mut egui::Ui, input: ServicePanelInput<'_>) -> Vec<ServiceAction> {
        let mut actions = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("measurement-service")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.label(RichText::new("Measurement service").size(29.0).strong());
                ui.label(RichText::new("Your sensors, together in one place.").color(muted(ui)));
                ui.add_space(22.0);
                service_summary(ui, &input, &mut actions);
                ui.add_space(22.0);
                ui.label(RichText::new("SENSORS").size(11.0).strong().color(muted(ui)));
                ui.add_space(10.0);

                let kinds = [DeviceKind::Polar, DeviceKind::GoDirect, DeviceKind::ThoughtStream];
                if ui.available_width() >= 760.0 {
                    ui.columns(3, |columns| {
                        for (column, kind) in columns.iter_mut().zip(kinds) {
                            device_card(column, kind, false, &input, &mut actions);
                        }
                    });
                } else {
                    for kind in kinds {
                        device_card(ui, kind, true, &input, &mut actions);
                        ui.add_space(8.0);
                    }
                }
                if input.connected && !input.controls_available {
                    ui.add_space(8.0);
                    ui.label(RichText::new("This service supports live readings. Update the service to control sensors from this panel.").size(12.0).color(muted(ui)));
                }
                if input.connected && input.status.is_some_and(|status| status.devices.iter().any(|device| device.device.as_ref().is_some_and(|device| device.kind == DeviceKind::Simulated as i32))) {
                    ui.add_space(8.0);
                    ui.label(RichText::new("The service is providing simulated readings.").size(12.0).color(AMBER));
                }
                ui.add_space(18.0);
                self.recording_ui(ui, &input, &mut actions);
                ui.add_space(16.0);
                connection_options(ui, &input, &mut actions);
                ui.add_space(14.0);
            });
        actions
    }

    fn recording_ui(
        &mut self,
        ui: &mut egui::Ui,
        input: &ServicePanelInput<'_>,
        actions: &mut Vec<ServiceAction>,
    ) {
        let recording = input.status.and_then(|status| status.recording.as_ref());
        let recording_state =
            recording.and_then(|value| RecordingState::try_from(value.state).ok());
        let active = input.connected && recording_state == Some(RecordingState::Recording);
        let available = input.connected
            && recording_state.is_some_and(|state| {
                !matches!(
                    state,
                    RecordingState::Unavailable | RecordingState::Unspecified
                )
            });
        card_frame(ui).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                status_dot(
                    ui,
                    if active {
                        CORAL
                    } else {
                        ui.visuals().weak_text_color()
                    },
                );
                ui.label(
                    RichText::new(if active {
                        "Recording your session"
                    } else {
                        "Session recording"
                    })
                    .size(17.0),
                );
                if active {
                    ui.label(RichText::new("REC").size(12.0).strong().color(CORAL));
                }
            });
            ui.add_space(5.0);
            if active {
                if let Some(recording) = recording {
                    ui.label(
                        RichText::new(format!(
                            "{} · {} readings saved",
                            recording.label, recording.sample_count
                        ))
                        .color(muted(ui)),
                    );
                    if recording.dropped_samples > 0 {
                        ui.label(
                            RichText::new(format!(
                                "{} readings could not be saved. See Diagnostics for details.",
                                recording.dropped_samples
                            ))
                            .size(12.0)
                            .color(AMBER),
                        );
                    }
                }
            } else {
                ui.label(
                    RichText::new("Save the original sensor readings for later.").color(muted(ui)),
                );
            }
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if !active {
                    ui.add_enabled(
                        available && !input.busy,
                        egui::TextEdit::singleline(&mut self.recording_label)
                            .hint_text("Session name (optional)")
                            .desired_width(220.0),
                    );
                }
                if ui
                    .add_enabled(
                        available && !input.busy,
                        egui::Button::new(if active {
                            "Stop recording"
                        } else {
                            "Start recording"
                        }),
                    )
                    .clicked()
                {
                    actions.push(if active {
                        ServiceAction::StopRecording
                    } else {
                        ServiceAction::StartRecording {
                            label: self.recording_label.trim().to_owned(),
                        }
                    });
                }
                if ui
                    .add_enabled(
                        input.connected
                            && recording.is_some_and(|recording| !recording.directory.is_empty()),
                        egui::Button::new("Show recordings"),
                    )
                    .clicked()
                {
                    actions.push(ServiceAction::OpenRecordings);
                }
            });
            if input.connected
                && matches!(
                    recording_state,
                    Some(RecordingState::Error | RecordingState::Interrupted)
                )
                && let Some(recording) = recording
            {
                ui.add_space(8.0);
                ui.label(RichText::new(&recording.detail).size(12.0).color(AMBER));
            }
        });
    }
}

fn service_summary(
    ui: &mut egui::Ui,
    input: &ServicePanelInput<'_>,
    actions: &mut Vec<ServiceAction>,
) {
    let (title, description, color) = match (input.mode, input.connected) {
        (ServiceMode::Embedded, true) => (
            "Running in this app",
            "Sensors connect here automatically. Keep this app open during your session.",
            MINT,
        ),
        (ServiceMode::External, true) => (
            "Connected to your measurement service",
            "Your separate service keeps sensors connected, even when this window is closed.",
            MINT,
        ),
        (ServiceMode::Stopping, _) => (
            "Stopping measurement service…",
            "Finishing recordings and releasing your sensors.",
            AMBER,
        ),
        (ServiceMode::Starting, _) => (
            "Starting measurement service…",
            "Preparing your sensors. This can take a moment.",
            AMBER,
        ),
        (ServiceMode::Failed, false) => (
            "Measurement service needs attention",
            "Your sensors are not connected to this window.",
            AMBER,
        ),
        (_, true) => (
            "Measurement service connected",
            "Ready to receive sensor readings.",
            MINT,
        ),
        (ServiceMode::Embedded | ServiceMode::External, false) => (
            "Reconnecting to measurement service…",
            "Sensor status will update when the connection is restored.",
            AMBER,
        ),
        (ServiceMode::Offline, false) => (
            "Ready when you are",
            "Start the measurement service to connect your sensors.",
            ui.visuals().weak_text_color(),
        ),
    };
    card_frame(ui)
        .fill(if ui.visuals().dark_mode {
            Color32::from_rgb(24, 37, 40)
        } else {
            Color32::from_rgb(235, 247, 246)
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                status_dot(ui, color);
                ui.label(RichText::new(title).size(19.0).strong());
                if matches!(input.mode, ServiceMode::Starting | ServiceMode::Stopping) {
                    ui.spinner();
                }
            });
            ui.add_space(6.0);
            ui.label(RichText::new(description).color(muted(ui)));
            if let Some(notice) = input.notice.filter(|notice| !notice.trim().is_empty()) {
                ui.add_space(7.0);
                ui.label(RichText::new(notice).size(12.0).color(
                    if input.mode == ServiceMode::Failed {
                        AMBER
                    } else {
                        ui.visuals().text_color()
                    },
                ));
            }
            ui.add_space(12.0);
            ui.horizontal_wrapped(|ui| {
                if !input.connected
                    && matches!(input.mode, ServiceMode::Offline | ServiceMode::Failed)
                {
                    if ui
                        .add_enabled(
                            input.can_start && !input.busy,
                            egui::Button::new("Start measurement service")
                                .min_size(Vec2::new(0.0, 34.0)),
                        )
                        .clicked()
                    {
                        actions.push(ServiceAction::StartService);
                    }
                } else if input.mode == ServiceMode::Embedded
                    && ui
                        .add_enabled(!input.busy, egui::Button::new("Stop service"))
                        .on_hover_text(
                            "Stops this app’s measurements and finishes any active recording.",
                        )
                        .clicked()
                {
                    actions.push(ServiceAction::StopService);
                }
                if input.connected
                    && let Some(status) = input.status
                {
                    ui.label(
                        RichText::new(format!("Running for {}", elapsed(status.uptime_millis)))
                            .size(12.0)
                            .color(muted(ui)),
                    );
                }
            });
        });
}

fn device_card(
    ui: &mut egui::Ui,
    kind: DeviceKind,
    compact: bool,
    input: &ServicePanelInput<'_>,
    actions: &mut Vec<ServiceAction>,
) {
    let status = input.status.and_then(|status| {
        status.devices.iter().find(|status| {
            status
                .device
                .as_ref()
                .is_some_and(|device| device.kind == kind as i32)
        })
    });
    let control = status
        .and_then(|status| status.device.as_ref())
        .and_then(|device| {
            input
                .device_controls
                .iter()
                .find(|control| control.id == device.id)
        });
    let view = device_view(kind, status, input);
    let (title, subtitle) = match kind {
        DeviceKind::Polar => ("Heart", "Polar H10 · Bluetooth"),
        DeviceKind::GoDirect => ("Breathing", "Go Direct belt · Bluetooth"),
        _ => ("ThoughtStream", "Skin resistance · USB"),
    };
    let identity = |ui: &mut egui::Ui| {
        ui.horizontal(|ui| {
            device_icon(ui, kind, view.live);
            ui.vertical(|ui| {
                ui.label(RichText::new(title).size(20.0).strong());
                ui.label(RichText::new(subtitle).size(12.0).color(muted(ui)));
            });
        });
    };
    let feedback = |ui: &mut egui::Ui| {
        ui.horizontal(|ui| {
            status_dot(ui, view.color);
            ui.label(RichText::new(view.label).color(view.color).strong());
        });
        let secondary = if !input.connected {
            "Waiting for the measurement service".to_owned()
        } else if view.live {
            format!(
                "Last reading {} ago",
                elapsed(status.map_or(0, |status| status.last_sample_age_millis))
            )
        } else if control.is_some_and(|control| !control.enabled) {
            "Enable this sensor when you want to use it".to_owned()
        } else {
            match kind {
                DeviceKind::Polar => "Wear the chest strap and moisten its contacts".to_owned(),
                DeviceKind::GoDirect => "Turn on your belt and keep it nearby".to_owned(),
                _ => "Connect the USB cable and turn on the device".to_owned(),
            }
        };
        ui.add(egui::Label::new(RichText::new(secondary).size(12.0).color(muted(ui))).wrap());
    };
    let mut buttons = |ui: &mut egui::Ui| {
        ui.horizontal_wrapped(|ui| {
            if let Some(control) = control {
                let mut enabled = control.enabled;
                if ui
                    .add_enabled(
                        input.connected && input.controls_available && !input.busy,
                        egui::Checkbox::new(&mut enabled, "Enabled"),
                    )
                    .changed()
                {
                    actions.push(ServiceAction::SetDeviceEnabled {
                        device_id: control.id.clone(),
                        enabled,
                    });
                }
            }
            if let Some(device) = status.and_then(|status| status.device.as_ref()) {
                let enabled = control.is_some_and(|control| control.enabled);
                if ui
                    .add_enabled(
                        input.connected && input.controls_available && enabled && !input.busy,
                        egui::Button::new("Reconnect"),
                    )
                    .clicked()
                {
                    actions.push(ServiceAction::ReconnectDevice {
                        device_id: device.id.clone(),
                    });
                }
            }
        });
    };
    let details = |ui: &mut egui::Ui| {
        if input.connected
            && let Some(status) = status.filter(|status| !status.detail.trim().is_empty())
        {
            ui.add_space(4.0);
            egui::CollapsingHeader::new("Connection details")
                .id_salt(("sensor-detail", kind as i32))
                .show(ui, |ui| {
                    ui.label(RichText::new(&status.detail).size(12.0));
                    if status.reconnect_attempts > 0 {
                        ui.label(
                            RichText::new(format!(
                                "{} reconnection attempts",
                                status.reconnect_attempts
                            ))
                            .size(12.0)
                            .color(muted(ui)),
                        );
                    }
                });
            if cfg!(target_os = "macos")
                && matches!(kind, DeviceKind::Polar | DeviceKind::GoDirect)
                && permission_error(&status.detail)
            {
                ui.label(RichText::new("Allow newKasina in System Settings → Privacy & Security → Bluetooth, then reconnect.").size(12.0).color(AMBER));
            }
        }
    };
    card_frame(ui).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if compact {
            ui.columns(2, |columns| {
                identity(&mut columns[0]);
                columns[0].add_space(10.0);
                buttons(&mut columns[0]);
                feedback(&mut columns[1]);
                columns[1].add_space(8.0);
                details(&mut columns[1]);
            });
        } else {
            identity(ui);
            ui.add_space(13.0);
            feedback(ui);
            ui.add_space(12.0);
            buttons(ui);
            details(ui);
        }
    });
}

struct DeviceView {
    label: &'static str,
    color: Color32,
    live: bool,
}

fn device_view(
    kind: DeviceKind,
    status: Option<&DeviceStatus>,
    input: &ServicePanelInput<'_>,
) -> DeviceView {
    let off = Color32::from_gray(145);
    if !input.connected {
        return DeviceView {
            label: "Service offline",
            color: off,
            live: false,
        };
    }
    let Some(status) = status else {
        return DeviceView {
            label: "Not available",
            color: off,
            live: false,
        };
    };
    if status.device.as_ref().is_some_and(|device| {
        input
            .device_controls
            .iter()
            .any(|control| control.id == device.id && !control.enabled)
    }) {
        return DeviceView {
            label: "Turned off",
            color: off,
            live: false,
        };
    }
    let (label, color, live) = match ConnectionState::try_from(status.state).ok() {
        Some(ConnectionState::Connected) => {
            let streams = match kind {
                DeviceKind::Polar => &[StreamKind::HeartRate, StreamKind::RrInterval][..],
                DeviceKind::GoDirect => &[StreamKind::RespirationForce][..],
                _ => &[StreamKind::SkinResistance, StreamKind::ThoughtStreamAdc][..],
            };
            let has_readings = input.status.is_some_and(|status| {
                status.streams.iter().any(|stream| {
                    stream.newest_sequence > 0
                        && streams.iter().any(|kind| *kind as i32 == stream.stream)
                })
            });
            if has_readings && status.last_sample_age_millis <= 5_000 {
                ("Live signal", MINT, true)
            } else {
                ("Waiting for readings", AMBER, false)
            }
        }
        Some(ConnectionState::Discovering) => ("Looking for sensor", AMBER, false),
        Some(ConnectionState::Connecting) => ("Connecting…", AMBER, false),
        Some(ConnectionState::Reconnecting) => ("Reconnecting…", AMBER, false),
        Some(ConnectionState::Error) => ("Needs attention", AMBER, false),
        _ => ("Not connected", off, false),
    };
    DeviceView { label, color, live }
}

fn connection_options(
    ui: &mut egui::Ui,
    input: &ServicePanelInput<'_>,
    actions: &mut Vec<ServiceAction>,
) {
    egui::CollapsingHeader::new("ThoughtStream connection")
        .id_salt("measurement-thoughtstream-connection")
        .show(ui, |ui| {
            ui.label(RichText::new("Usually, your device is found automatically. Choose its USB connection here if it is not.").color(muted(ui)));
            ui.add_space(8.0);
            let available = input.connected && input.controls_available && !input.busy;
            ui.add_enabled_ui(available, |ui| {
                let automatic = input.serial.is_none_or(|serial| serial.automatic);
                if ui.radio(automatic, "Find automatically").clicked() && !automatic {
                    actions.push(ServiceAction::SetThoughtStreamPort(None));
                }
                ui.horizontal_wrapped(|ui| {
                    let selected = input.serial.and_then(|serial| serial.selected_path.as_ref());
                    egui::ComboBox::from_id_salt("measurement-usb-choice")
                        .selected_text(if automatic { "Choose a USB connection…" } else { selected.map_or("Choose a USB connection…", String::as_str) })
                        .width(270.0_f32.min(ui.available_width().max(100.0)))
                        .show_ui(ui, |ui| {
                            if let Some(serial) = input.serial {
                                if serial.ports.is_empty() {
                                    ui.label("No USB connections found");
                                }
                                if let Some(selected) = selected.filter(|selected| !automatic && !serial.ports.iter().any(|port| &port.path == *selected)) {
                                    ui.label(RichText::new(format!("Saved: {selected} (not connected)")).color(muted(ui)));
                                }
                                for port in &serial.ports {
                                    if ui.selectable_label(!automatic && selected == Some(&port.path), &port.label).on_hover_text(&port.path).clicked() {
                                        actions.push(ServiceAction::SetThoughtStreamPort(Some(port.path.clone())));
                                    }
                                }
                            }
                        });
                    if ui.button("Refresh list").clicked() {
                        actions.push(ServiceAction::RefreshSerialPorts);
                    }
                });
            });
            if let Some(serial) = input.serial.filter(|serial| !serial.detail.is_empty()) {
                ui.add_space(6.0);
                ui.label(RichText::new(&serial.detail).size(12.0).color(muted(ui)));
            }
            if input.connected && !input.controls_available {
                ui.label(RichText::new("Update the measurement service to choose a USB connection here.").size(12.0).color(muted(ui)));
            }
        });
}

fn muted(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(167, 172, 184)
    } else {
        Color32::from_rgb(85, 88, 98)
    }
}

fn card_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::new()
        .fill(if ui.visuals().dark_mode {
            Color32::from_rgb(30, 31, 39)
        } else {
            Color32::from_rgb(245, 246, 250)
        })
        .corner_radius(14)
        .inner_margin(18.0)
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, color);
}

fn elapsed(millis: u64) -> String {
    match millis {
        0..1_000 => "less than a second".to_owned(),
        1_000..60_000 => format!("{} s", millis / 1_000),
        60_000..3_600_000 => format!("{} min", millis / 60_000),
        _ => format!("{} h {} min", millis / 3_600_000, (millis / 60_000) % 60),
    }
}

fn permission_error(detail: &str) -> bool {
    let detail = detail.to_lowercase();
    ["permission", "unauthorized", "not authorized", "denied"]
        .iter()
        .any(|word| detail.contains(word))
}

fn device_icon(ui: &mut egui::Ui, kind: DeviceKind, live: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(46.0), egui::Sense::hover());
    let base = match kind {
        DeviceKind::Polar => CORAL,
        DeviceKind::GoDirect => MINT,
        _ => VIOLET,
    };
    let color = if live { base } else { Color32::from_gray(135) };
    let painter = ui.painter();
    painter.circle_filled(
        rect.center(),
        22.0,
        color.gamma_multiply(if live { 0.12 } else { 0.06 }),
    );
    let at = |x: f32, y: f32| rect.min + Vec2::new(x * rect.width(), y * rect.height());
    let stroke = Stroke::new(2.2, color);
    match kind {
        DeviceKind::Polar => {
            // Sample the classic heart curve for a smooth outline without font glyphs.
            let points = (0..=64)
                .map(|index| {
                    let t = index as f32 / 64.0 * std::f32::consts::TAU;
                    let x = 16.0 * t.sin().powi(3);
                    let y = 13.0 * t.cos()
                        - 5.0 * (2.0 * t).cos()
                        - 2.0 * (3.0 * t).cos()
                        - (4.0 * t).cos();
                    at(0.5 + x * 0.020, 0.47 - y * 0.020)
                })
                .collect();
            painter.add(egui::Shape::closed_line(points, stroke));
        }
        DeviceKind::GoDirect => {
            painter.line_segment([at(0.5, 0.23), at(0.5, 0.47)], stroke);
            for direction in [-1.0, 1.0] {
                let points = [
                    (0.50, 0.47),
                    (0.43, 0.39),
                    (0.37, 0.30),
                    (0.30, 0.37),
                    (0.23, 0.49),
                    (0.20, 0.66),
                    (0.23, 0.75),
                    (0.32, 0.77),
                    (0.42, 0.71),
                    (0.45, 0.60),
                    (0.45, 0.52),
                ]
                .into_iter()
                .map(|(x, y)| at(0.5 + (x - 0.5) * direction, y))
                .collect();
                painter.add(egui::Shape::line(points, stroke));
            }
        }
        _ => {
            // A softly scalloped cloud and two smaller circles make a thought bubble.
            let points = (0..=72)
                .map(|index| {
                    let angle = index as f32 / 72.0 * std::f32::consts::TAU;
                    let radius = 0.28 + 0.022 * (angle * 7.0).cos();
                    at(
                        0.52 + angle.cos() * radius,
                        0.44 + angle.sin() * radius * 0.82,
                    )
                })
                .collect();
            painter.add(egui::Shape::closed_line(points, stroke));
            painter.circle_stroke(at(0.32, 0.74), 3.0, stroke);
            painter.circle_filled(at(0.23, 0.84), 1.8, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kasina_protocol::v1::{DeviceInfo, StreamDiagnostics};

    #[test]
    fn retained_snapshot_never_looks_live_after_service_disconnects() {
        let device = DeviceStatus {
            device: Some(DeviceInfo {
                id: "heart".into(),
                kind: DeviceKind::Polar as i32,
                ..Default::default()
            }),
            state: ConnectionState::Connected as i32,
            last_sample_age_millis: 5,
            ..Default::default()
        };
        let snapshot = StatusSnapshot {
            devices: vec![device.clone()],
            streams: vec![StreamDiagnostics {
                stream: StreamKind::HeartRate as i32,
                newest_sequence: 12,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut input = ServicePanelInput {
            mode: ServiceMode::External,
            connected: true,
            status: Some(&snapshot),
            notice: None,
            controls_available: true,
            device_controls: &[],
            serial: None,
            busy: false,
            can_start: true,
        };
        assert!(device_view(DeviceKind::Polar, Some(&device), &input).live);
        input.connected = false;
        let view = device_view(DeviceKind::Polar, Some(&device), &input);
        assert!(!view.live);
        assert_eq!(view.label, "Service offline");
    }

    #[test]
    fn connected_device_without_readings_or_with_stale_readings_is_not_live() {
        let mut device = DeviceStatus {
            state: ConnectionState::Connected as i32,
            ..Default::default()
        };
        let snapshot = StatusSnapshot::default();
        let mut input = ServicePanelInput {
            mode: ServiceMode::Embedded,
            connected: true,
            status: Some(&snapshot),
            notice: None,
            controls_available: true,
            device_controls: &[],
            serial: None,
            busy: false,
            can_start: true,
        };
        assert!(!device_view(DeviceKind::Polar, Some(&device), &input).live);
        let with_readings = StatusSnapshot {
            streams: vec![StreamDiagnostics {
                stream: StreamKind::HeartRate as i32,
                newest_sequence: 12,
                ..Default::default()
            }],
            ..Default::default()
        };
        input.status = Some(&with_readings);
        assert!(device_view(DeviceKind::Polar, Some(&device), &input).live);
        device.last_sample_age_millis = 8_000;
        assert!(!device_view(DeviceKind::Polar, Some(&device), &input).live);
    }
}
