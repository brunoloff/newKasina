use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use anyhow::{Context as _, Result, bail};
use kasina_render::{AuroraVortex, KasinaVisual, LuminousMandala, OrganicKaleidoscope};
use serde::{Deserialize, Serialize};

const SETTINGS_SCHEMA_VERSION: u32 = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct TabVisibility {
    pub dashboard: bool,
    pub raw_signals: bool,
    pub breath_kasina: bool,
    pub gpu_stress_test: bool,
    pub diagnostics: bool,
}

impl Default for TabVisibility {
    fn default() -> Self {
        Self {
            dashboard: false,
            raw_signals: false,
            breath_kasina: true,
            gpu_stress_test: false,
            diagnostics: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "implementation", content = "options", rename_all = "kebab-case")]
pub(crate) enum KasinaVisualPreset {
    LuminousMandala(LuminousMandala),
    AuroraVortex(AuroraVortex),
    OrganicKaleidoscope(OrganicKaleidoscope),
}

impl KasinaVisualPreset {
    pub fn implementation_name(&self) -> &'static str {
        match self {
            Self::LuminousMandala(visual) => visual.display_name(),
            Self::AuroraVortex(visual) => visual.display_name(),
            Self::OrganicKaleidoscope(visual) => visual.display_name(),
        }
    }

    pub fn as_visual(&self) -> &dyn KasinaVisual {
        match self {
            Self::LuminousMandala(visual) => visual,
            Self::AuroraVortex(visual) => visual,
            Self::OrganicKaleidoscope(visual) => visual,
        }
    }

    pub fn sanitize(&mut self) {
        match self {
            Self::LuminousMandala(options) => *options = options.sanitized(),
            Self::AuroraVortex(options) => *options = options.sanitized(),
            Self::OrganicKaleidoscope(options) => *options = options.sanitized(),
        }
    }
}

impl Default for KasinaVisualPreset {
    fn default() -> Self {
        Self::LuminousMandala(LuminousMandala::default())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KasinaPreset {
    pub id: u64,
    pub name: String,
    pub visual: KasinaVisualPreset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct AppSettings {
    pub schema_version: u32,
    pub simulation_mode: bool,
    pub visible_tabs: TabVisibility,
    pub active_preset_id: u64,
    pub next_preset_id: u64,
    pub presets: Vec<KasinaPreset>,
}

impl AppSettings {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        let settings: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse settings at {}", path.display()))?;
        if settings.schema_version > SETTINGS_SCHEMA_VERSION {
            bail!(
                "settings schema {} is newer than supported schema {}",
                settings.schema_version,
                SETTINGS_SCHEMA_VERSION
            );
        }
        Ok(settings.sanitized())
    }

    #[must_use]
    pub fn sanitized(mut self) -> Self {
        let source_schema = self.schema_version;
        if source_schema < 4 {
            let value_was_radians_per_second = source_schema < 3;
            for preset in &mut self.presets {
                if let KasinaVisualPreset::LuminousMandala(options) = &mut preset.visual {
                    options.migrate_shared_rotation_speed(value_was_radians_per_second);
                }
            }
        }
        self.schema_version = SETTINGS_SCHEMA_VERSION;
        let mut used_ids = BTreeSet::new();
        self.presets.retain_mut(|preset| {
            preset.name = preset.name.trim().chars().take(80).collect();
            preset.visual.sanitize();
            !preset.name.is_empty() && preset.id > 0 && used_ids.insert(preset.id)
        });
        if self.presets.is_empty() {
            return Self::default();
        }
        if source_schema < 5
            && self
                .presets
                .iter()
                .all(|preset| !matches!(&preset.visual, KasinaVisualPreset::AuroraVortex(_)))
        {
            let id = (self.next_preset_id.max(1)..=u64::MAX)
                .find(|candidate| !used_ids.contains(candidate))
                .unwrap_or_else(|| {
                    (1..self.next_preset_id)
                        .find(|candidate| !used_ids.contains(candidate))
                        .expect("a finite preset list must leave an unused identifier")
                });
            self.presets.push(default_aurora_preset(id));
            used_ids.insert(id);
        }
        if source_schema < 6
            && self
                .presets
                .iter()
                .all(|preset| !matches!(&preset.visual, KasinaVisualPreset::OrganicKaleidoscope(_)))
        {
            let id = (self.next_preset_id.max(1)..=u64::MAX)
                .find(|candidate| !used_ids.contains(candidate))
                .unwrap_or_else(|| {
                    (1..self.next_preset_id)
                        .find(|candidate| !used_ids.contains(candidate))
                        .expect("a finite preset list must leave an unused identifier")
                });
            self.presets.push(default_kaleidoscope_preset(id));
            used_ids.insert(id);
        }
        if !used_ids.contains(&self.active_preset_id) {
            self.active_preset_id = self.presets[0].id;
        }
        self.next_preset_id = self.next_preset_id.max(
            self.presets
                .iter()
                .map(|preset| preset.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        );
        self
    }

    pub fn active_preset(&self) -> &KasinaPreset {
        self.preset(self.active_preset_id)
            .unwrap_or_else(|| &self.presets[0])
    }

    pub fn preset(&self, id: u64) -> Option<&KasinaPreset> {
        self.presets.iter().find(|preset| preset.id == id)
    }

    pub fn preset_mut(&mut self, id: u64) -> Option<&mut KasinaPreset> {
        self.presets.iter_mut().find(|preset| preset.id == id)
    }

    pub fn add_preset(&mut self, source_id: u64) -> u64 {
        let mut preset = self
            .preset(source_id)
            .cloned()
            .unwrap_or_else(|| self.active_preset().clone());
        let id = (self.next_preset_id..=u64::MAX)
            .find(|candidate| self.preset(*candidate).is_none())
            .unwrap_or_else(|| {
                (1..self.next_preset_id)
                    .find(|candidate| self.preset(*candidate).is_none())
                    .expect("a finite preset list must leave an unused identifier")
            });
        self.next_preset_id = id.saturating_add(1);
        preset.id = id;
        preset.name = unique_custom_name(&self.presets);
        self.presets.push(preset);
        self.active_preset_id = id;
        id
    }

    pub fn remove_preset(&mut self, id: u64) -> bool {
        if self.presets.len() <= 1 {
            return false;
        }
        let original_len = self.presets.len();
        self.presets.retain(|preset| preset.id != id);
        if self.presets.len() == original_len {
            return false;
        }
        if self.active_preset_id == id {
            self.active_preset_id = self.presets[0].id;
        }
        true
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: SETTINGS_SCHEMA_VERSION,
            simulation_mode: false,
            visible_tabs: TabVisibility::default(),
            active_preset_id: 1,
            next_preset_id: 6,
            presets: vec![
                KasinaPreset {
                    id: 1,
                    name: "Luminous flow".to_owned(),
                    visual: KasinaVisualPreset::LuminousMandala(LuminousMandala::default()),
                },
                KasinaPreset {
                    id: 2,
                    name: "Quiet focus".to_owned(),
                    visual: KasinaVisualPreset::LuminousMandala(LuminousMandala {
                        minimum_radius: 0.32,
                        maximum_radius: 0.68,
                        rotation_enabled: false,
                        ..LuminousMandala::default()
                    }),
                },
                KasinaPreset {
                    id: 3,
                    name: "Deep orbit".to_owned(),
                    visual: KasinaVisualPreset::LuminousMandala(LuminousMandala {
                        minimum_radius: 0.22,
                        maximum_radius: 0.95,
                        rotation_enabled: true,
                        inner_rotations_per_second: 0.02,
                        middle_rotations_per_second: 0.02,
                        third_rotations_per_second: 0.02,
                        gold_rotations_per_second: 0.02,
                        ..LuminousMandala::default()
                    }),
                },
                default_aurora_preset(4),
                default_kaleidoscope_preset(5),
            ],
        }
    }
}

fn default_aurora_preset(id: u64) -> KasinaPreset {
    KasinaPreset {
        id,
        name: "Aurora tide".to_owned(),
        visual: KasinaVisualPreset::AuroraVortex(AuroraVortex::default()),
    }
}

fn default_kaleidoscope_preset(id: u64) -> KasinaPreset {
    KasinaPreset {
        id,
        name: "Kaleidoscopic bloom".to_owned(),
        visual: KasinaVisualPreset::OrganicKaleidoscope(OrganicKaleidoscope::default()),
    }
}

fn unique_custom_name(presets: &[KasinaPreset]) -> String {
    for suffix in 1_u64.. {
        let candidate = format!("Custom {suffix}");
        if presets.iter().all(|preset| preset.name != candidate) {
            return candidate;
        }
    }
    unreachable!("the finite preset list cannot contain every integer suffix")
}

#[derive(Debug)]
pub(crate) struct SettingsWriter {
    sender: Option<SyncSender<AppSettings>>,
    thread: Option<JoinHandle<()>>,
}

impl SettingsWriter {
    pub fn spawn(path: PathBuf) -> Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<AppSettings>(1);
        let thread = thread::Builder::new()
            .name("kasina-settings-writer".to_owned())
            .spawn(move || {
                while let Ok(mut settings) = receiver.recv() {
                    while let Ok(newer) = receiver.try_recv() {
                        settings = newer;
                    }
                    if let Err(error) = write_atomically(&path, &settings) {
                        tracing::error!(%error, path = %path.display(), "write app settings");
                    }
                }
            })
            .context("spawn settings writer")?;
        Ok(Self {
            sender: Some(sender),
            thread: Some(thread),
        })
    }

    pub fn try_queue(&self, settings: AppSettings) -> bool {
        let Some(sender) = &self.sender else {
            return false;
        };
        match sender.try_send(settings) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                tracing::error!("settings writer stopped unexpectedly");
                false
            }
        }
    }

    pub fn finish(&mut self, final_settings: Option<AppSettings>) {
        if let Some(sender) = self.sender.take() {
            if let Some(settings) = final_settings {
                let _ignored = sender.send(settings);
            }
            drop(sender);
        }
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::error!("settings writer panicked");
        }
    }
}

impl Drop for SettingsWriter {
    fn drop(&mut self) {
        self.finish(None);
    }
}

fn write_atomically(path: &Path, settings: &AppSettings) -> Result<()> {
    let parent = path
        .parent()
        .context("settings path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let temporary = path.with_extension("json.tmp");
    let encoded = serde_json::to_vec_pretty(settings).context("serialize app settings")?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("open {}", temporary.display()))?;
    file.write_all(&encoded)
        .with_context(|| format!("write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", temporary.display()))?;
    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("remove old settings at {}", path.display()))?;
    }
    fs::rename(&temporary, path)
        .with_context(|| format!("replace settings at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn defaults_show_only_breath_and_include_all_visual_styles() {
        let settings = AppSettings::default();
        assert!(settings.visible_tabs.breath_kasina);
        assert!(!settings.simulation_mode);
        assert!(!settings.visible_tabs.dashboard);
        assert!(!settings.visible_tabs.raw_signals);
        assert_eq!(settings.presets.len(), 5);
        assert_eq!(settings.active_preset().name, "Luminous flow");
        assert!(
            settings
                .presets
                .iter()
                .any(|preset| matches!(&preset.visual, KasinaVisualPreset::AuroraVortex(_)))
        );
        assert!(
            settings
                .presets
                .iter()
                .any(|preset| matches!(&preset.visual, KasinaVisualPreset::OrganicKaleidoscope(_)))
        );
    }

    #[test]
    fn presets_can_be_added_removed_and_round_trip() {
        let mut settings = AppSettings::default();
        let custom_id = settings.add_preset(2);
        assert_eq!(settings.active_preset_id, custom_id);
        assert_eq!(settings.presets.len(), 6);
        assert!(settings.remove_preset(custom_id));
        assert_eq!(settings.presets.len(), 5);

        settings.presets.truncate(1);
        assert!(!settings.remove_preset(settings.presets[0].id));
        assert_eq!(settings.presets.len(), 1);

        let encoded = serde_json::to_vec(&settings).unwrap();
        let decoded: AppSettings = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.presets.len(), 1);
        assert_eq!(decoded.active_preset().name, "Luminous flow");
    }

    #[test]
    fn sanitization_repairs_invalid_active_preset_and_options() {
        let mut settings = AppSettings {
            active_preset_id: 999,
            ..AppSettings::default()
        };
        let KasinaVisualPreset::LuminousMandala(options) = &mut settings.presets[0].visual else {
            panic!("first default preset should be luminous")
        };
        options.minimum_radius = 4.0;
        options.maximum_radius = -2.0;
        let settings = settings.sanitized();

        assert_eq!(settings.active_preset_id, 1);
        let KasinaVisualPreset::LuminousMandala(options) = &settings.presets[0].visual else {
            panic!("first default preset should be luminous")
        };
        assert!(options.minimum_radius < options.maximum_radius);
    }

    #[test]
    fn settings_file_can_be_created_replaced_and_loaded() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "newkasina-settings-test-{}-{unique}",
            std::process::id()
        ));
        let path = directory.join("app-settings.json");
        let mut settings = AppSettings::default();
        write_atomically(&path, &settings).unwrap();
        settings.visible_tabs.diagnostics = true;
        write_atomically(&path, &settings).unwrap();

        let loaded = AppSettings::load(&path).unwrap();
        assert!(loaded.visible_tabs.diagnostics);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn schema_one_settings_migrate_with_simulation_disabled() {
        let encoded = serde_json::to_value(AppSettings::default()).unwrap();
        let mut object = encoded.as_object().unwrap().clone();
        object.insert("schema_version".to_owned(), serde_json::json!(1));
        object.remove("simulation_mode");
        let old_settings: AppSettings = serde_json::from_value(object.into()).unwrap();
        let migrated = old_settings.sanitized();

        assert_eq!(migrated.schema_version, SETTINGS_SCHEMA_VERSION);
        assert!(!migrated.simulation_mode);
    }

    #[test]
    fn schema_two_rotation_speed_migrates_from_radians_to_rotations() {
        let mut encoded = serde_json::to_value(AppSettings::default()).unwrap();
        encoded["schema_version"] = serde_json::json!(2);
        let options = encoded["presets"][0]["visual"]["options"]
            .as_object_mut()
            .unwrap();
        options.remove("rotations_per_second");
        options.insert("rotation_speed".to_owned(), serde_json::json!(0.3));

        let old_settings: AppSettings = serde_json::from_value(encoded).unwrap();
        let migrated = old_settings.sanitized();
        let KasinaVisualPreset::LuminousMandala(options) = &migrated.presets[0].visual else {
            panic!("first migrated preset should be luminous")
        };
        let expected = 0.3 / std::f32::consts::TAU;

        assert_eq!(migrated.schema_version, SETTINGS_SCHEMA_VERSION);
        assert!((options.inner_rotations_per_second - expected).abs() < 1.0e-6);
        assert!((options.middle_rotations_per_second - expected).abs() < 1.0e-6);
        assert!((options.third_rotations_per_second - expected).abs() < 1.0e-6);
        assert!((options.gold_rotations_per_second - expected).abs() < 1.0e-6);
        let rewritten = serde_json::to_string(&migrated).unwrap();
        assert!(rewritten.contains("inner_rotations_per_second"));
        assert!(!rewritten.contains("\"rotations_per_second\""));
        assert!(!rewritten.contains("rotation_speed"));
    }

    #[test]
    fn schema_three_shared_rotations_migrate_to_each_layer() {
        let mut encoded = serde_json::to_value(AppSettings::default()).unwrap();
        encoded["schema_version"] = serde_json::json!(3);
        let options = encoded["presets"][0]["visual"]["options"]
            .as_object_mut()
            .unwrap();
        options.insert("rotations_per_second".to_owned(), serde_json::json!(0.25));

        let old_settings: AppSettings = serde_json::from_value(encoded).unwrap();
        let migrated = old_settings.sanitized();
        let KasinaVisualPreset::LuminousMandala(options) = &migrated.presets[0].visual else {
            panic!("first migrated preset should be luminous")
        };

        assert_eq!(options.layer_speeds(0.0), [0.25; 4]);
    }

    #[test]
    fn schema_four_settings_gain_one_aurora_preset() {
        let mut settings = AppSettings {
            schema_version: 4,
            ..AppSettings::default()
        };
        settings
            .presets
            .retain(|preset| matches!(&preset.visual, KasinaVisualPreset::LuminousMandala(_)));
        settings.next_preset_id = 4;

        let migrated = settings.sanitized();
        let aurora_presets = migrated
            .presets
            .iter()
            .filter(|preset| matches!(&preset.visual, KasinaVisualPreset::AuroraVortex(_)))
            .collect::<Vec<_>>();

        assert_eq!(migrated.schema_version, SETTINGS_SCHEMA_VERSION);
        assert_eq!(aurora_presets.len(), 1);
        assert_eq!(aurora_presets[0].name, "Aurora tide");
        assert_eq!(migrated.next_preset_id, 6);
    }

    #[test]
    fn schema_five_settings_gain_one_kaleidoscope_preset() {
        let mut settings = AppSettings {
            schema_version: 5,
            ..AppSettings::default()
        };
        settings
            .presets
            .retain(|preset| !matches!(&preset.visual, KasinaVisualPreset::OrganicKaleidoscope(_)));
        settings.next_preset_id = 5;

        let migrated = settings.sanitized();
        let kaleidoscope_presets = migrated
            .presets
            .iter()
            .filter(|preset| matches!(&preset.visual, KasinaVisualPreset::OrganicKaleidoscope(_)))
            .collect::<Vec<_>>();

        assert_eq!(migrated.schema_version, SETTINGS_SCHEMA_VERSION);
        assert_eq!(kaleidoscope_presets.len(), 1);
        assert_eq!(kaleidoscope_presets[0].name, "Kaleidoscopic bloom");
        assert_eq!(migrated.next_preset_id, 6);
    }
}
