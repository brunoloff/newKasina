use serde::{Deserialize, Serialize};

/// Breath-cycle lengths at the beginning, midpoint and end of the settling curve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub companions: usize,
    pub duration_minutes: f64,
    pub extension_minutes: f64,
    pub settling_minutes: f64,
    pub cycle_seconds: [f64; 3],
    pub pace: [f64; 4],
    pub volume: f32,
    pub microphone_threshold: f32,
    pub speakers: bool,
    pub shared_counts: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            companions: 2,
            duration_minutes: 10.0,
            extension_minutes: 3.0,
            settling_minutes: 15.0,
            cycle_seconds: [5.0, 7.0, 9.0],
            pace: [0.85, 1.05, 1.22, 0.96],
            volume: 0.55,
            microphone_threshold: 0.008,
            speakers: true,
            shared_counts: false,
        }
    }
}

impl Settings {
    pub fn sanitize(&mut self) {
        fn bounded(value: f64, fallback: f64, low: f64, high: f64) -> f64 {
            if value.is_finite() {
                value.clamp(low, high)
            } else {
                fallback
            }
        }
        let default = Self::default();
        self.companions = self.companions.clamp(1, 4);
        self.duration_minutes =
            bounded(self.duration_minutes, default.duration_minutes, 0.1, 180.0);
        self.extension_minutes =
            bounded(self.extension_minutes, default.extension_minutes, 0.1, 30.0);
        self.settling_minutes =
            bounded(self.settling_minutes, default.settling_minutes, 0.5, 120.0);
        for (value, fallback) in self.cycle_seconds.iter_mut().zip(default.cycle_seconds) {
            *value = bounded(*value, fallback, 3.0, 30.0);
        }
        for (value, fallback) in self.pace.iter_mut().zip(default.pace) {
            *value = bounded(*value, fallback, 0.65, 1.5);
        }
        self.volume = bounded(self.volume as f64, default.volume as f64, 0.0, 1.0) as f32;
        self.microphone_threshold = bounded(
            self.microphone_threshold as f64,
            default.microphone_threshold as f64,
            0.001,
            0.08,
        ) as f32;
    }

    pub fn cycle_at(&self, elapsed_seconds: f64, companion: usize) -> f64 {
        let progress = (elapsed_seconds / (self.settling_minutes * 60.0)).clamp(0.0, 1.0);
        let (a, b, t) = if progress < 0.5 {
            (self.cycle_seconds[0], self.cycle_seconds[1], progress * 2.0)
        } else {
            (
                self.cycle_seconds[1],
                self.cycle_seconds[2],
                (progress - 0.5) * 2.0,
            )
        };
        let smooth = t * t * (3.0 - 2.0 * t);
        (a + (b - a) * smooth) * self.pace[companion.min(3)]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Ready,
    Counting,
    Quiet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    You,
    Companion(usize),
}

/// One slot per participant, so repeated recognition never adds duplicate slices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Speakers(u8);
impl Speakers {
    pub fn insert(&mut self, speaker: Speaker) -> bool {
        let bit = match speaker {
            Speaker::You => 1,
            Speaker::Companion(index) if index < 4 => 1 << (index + 1),
            Speaker::Companion(_) => return false,
        };
        let fresh = self.0 & bit == 0;
        self.0 |= bit;
        fresh
    }
    pub fn iter(self) -> impl Iterator<Item = Speaker> {
        (0..5)
            .filter(move |index| self.0 & (1 << index) != 0)
            .map(|index| {
                if index == 0 {
                    Speaker::You
                } else {
                    Speaker::Companion(index - 1)
                }
            })
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn len(self) -> usize {
        self.0.count_ones() as usize
    }
}
impl From<Speaker> for Speakers {
    fn from(speaker: Speaker) -> Self {
        let mut speakers = Self::default();
        speakers.insert(speaker);
        speakers
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Count { number: u8, speakers: Speakers },
    Joined { number: u8 },
    Bell,
}

const COMPANION_GROUP_SECONDS: f64 = 0.25;
const HUMAN_OVERLAP_SECONDS: f64 = 0.45;

/// The current round followed by the session's completed rounds, newest first.
/// Skipped positions are explicit markers; filled cells are never overwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountHistory {
    pub rows: Vec<[Speakers; 10]>,
    pub completed_rows: usize,
    pub missed: Vec<[bool; 10]>,
    current_number: u8,
    round: u64,
}
impl Default for CountHistory {
    fn default() -> Self {
        Self {
            rows: vec![[Speakers::default(); 10]],
            missed: vec![[false; 10]],
            completed_rows: 0,
            current_number: 0,
            round: 0,
        }
    }
}
impl CountHistory {
    fn advance(&mut self) {
        self.rows.insert(0, [Speakers::default(); 10]);
        self.missed.insert(0, [false; 10]);
        self.current_number = 0;
        self.completed_rows += 1;
        self.round += 1;
    }
    fn record(&mut self, number: u8, speakers: Speakers) -> u64 {
        if number < self.current_number {
            self.missed[0][usize::from(self.current_number)..].fill(true);
            self.advance();
        }
        let round = self.round;
        for column in usize::from(self.current_number)..usize::from(number - 1) {
            self.missed[0][column] = true;
        }
        // A repeated current count merges participants, never replaces them.
        for speaker in speakers.iter() {
            self.rows[0][usize::from(number - 1)].insert(speaker);
        }
        self.current_number = number;
        if number == 10 {
            self.advance();
        }
        round
    }
    fn join(&mut self, round: u64, number: u8, speaker: Speaker) -> bool {
        let Some(row) = self
            .round
            .checked_sub(round)
            .filter(|row| (*row as usize) < self.rows.len())
        else {
            return false;
        };
        self.rows[row as usize][usize::from(number - 1)].insert(speaker)
    }
}
#[derive(Debug)]
struct RecentCount {
    started: f64,
    round: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub phase: Phase,
    pub last_number: u8,
    pub last_speakers: Speakers,
    pub history: CountHistory,
    pub elapsed_seconds: f64,
    pub remaining_seconds: f64,
    pub breath_phase: [f64; 4],
    pub cycle_seconds: [f64; 4],
    pub turns: u64,
}

#[derive(Debug)]
pub struct Engine {
    settings: Settings,
    phase: Phase,
    started: f64,
    deadline: f64,
    last_tick: f64,
    last_number: u8,
    recent_counts: Vec<RecentCount>,
    history: CountHistory,
    phases: [f64; 4],
    pending: [Option<f64>; 4],
    variation: [f64; 4],
    random: u64,
    next_voice_at: f64,
    recent_virtual: Vec<(u8, f64)>,
    turns: u64,
}

impl Engine {
    pub fn new(mut settings: Settings) -> Self {
        settings.sanitize();
        Self {
            settings,
            phase: Phase::Ready,
            started: 0.0,
            deadline: 0.0,
            last_tick: 0.0,
            last_number: 0,
            recent_counts: Vec::new(),
            history: CountHistory::default(),
            phases: [0.72, 0.30, 0.51, 0.06],
            pending: [None; 4],
            variation: [1.0; 4],
            random: 0x9327_185d_67b0_891f,
            next_voice_at: 0.0,
            recent_virtual: Vec::new(),
            turns: 0,
        }
    }

    pub fn start(&mut self, now: f64) {
        *self = Self::new(self.settings.clone());
        self.phase = Phase::Counting;
        self.started = now;
        self.last_tick = now;
        self.deadline = now + self.settings.duration_minutes * 60.0;
        self.next_voice_at = now + 1.0;
    }

    pub fn stop(&mut self) {
        self.phase = Phase::Ready;
        self.pending = [None; 4];
    }

    pub fn extend(&mut self, now: f64) {
        if self.phase == Phase::Ready {
            return;
        }
        if self.phase == Phase::Quiet || now >= self.deadline {
            self.phase = Phase::Counting;
            self.last_tick = now;
            self.pending = [None; 4];
            self.phases = [0.72, 0.30, 0.51, 0.06];
            self.next_voice_at = now + 1.0;
        }
        self.deadline = self.deadline.max(now) + self.settings.extension_minutes * 60.0;
    }

    /// The caller processes the bell before audio or recognition events at the deadline.
    pub fn tick(&mut self, now: f64, voice_busy: bool) -> Option<Event> {
        if self.phase != Phase::Counting {
            return None;
        }
        if now >= self.deadline {
            self.phase = Phase::Quiet;
            self.pending = [None; 4];
            return Some(Event::Bell);
        }
        let dt = (now - self.last_tick).max(0.0);
        self.last_tick = now;
        // After suspension, resume the current rhythms without a burst of old counts.
        if dt > 1.0 {
            self.pending = [None; 4];
            self.next_voice_at = now + 0.8;
        }
        for index in 0..self.settings.companions {
            let cycle = self.settings.cycle_at(now - self.started, index) * self.variation[index];
            self.phases[index] += dt.min(1.0) / cycle;
            if self.phases[index] >= 1.0 {
                self.phases[index] %= 1.0;
                self.pending[index].get_or_insert(now - self.phases[index] * cycle);
                self.random ^= self.random << 13;
                self.random ^= self.random >> 7;
                self.random ^= self.random << 17;
                self.variation[index] = 0.97 + (self.random % 1000) as f64 * 0.00006;
            }
        }
        self.recent_virtual.retain(|(_, time)| now - time < 2.5);
        if voice_busy || now < self.next_voice_at {
            return None;
        }
        let index = (0..self.settings.companions)
            .filter(|index| self.pending[*index].is_some())
            .min_by(|a, b| {
                self.pending[*a]
                    .unwrap()
                    .total_cmp(&self.pending[*b].unwrap())
            })?;
        let first_index = index;
        let first_due = self.pending[index].unwrap();
        if now - first_due < COMPANION_GROUP_SECONDS {
            return None;
        }
        let mut speakers = Speakers::default();
        for index in 0..self.settings.companions {
            if self.pending[index].is_some_and(|due| due - first_due <= COMPANION_GROUP_SECONDS)
                && (self.settings.shared_counts || index == first_index)
            {
                self.pending[index] = None;
                speakers.insert(Speaker::Companion(index));
            }
        }
        let number = self.last_number % 10 + 1;
        self.commit(number, speakers, now);
        self.recent_virtual.push((number, now));
        Some(Event::Count { number, speakers })
    }

    /// A recognized human number takes precedence and can gently repair a lost count.
    pub fn heard(
        &mut self,
        number: u8,
        utterance_started: f64,
        now: f64,
        independent_voice: bool,
    ) -> Option<Event> {
        if self.phase != Phase::Counting
            || now >= self.deadline
            || !(1..=10).contains(&number)
            || utterance_started < self.started
            || utterance_started > now
            || now - utterance_started > 4.0
        {
            return None;
        }
        // Only the latest accepted count can gain participants. A delayed
        // recognition result must not repaint an older cell or rewind a newer turn.
        if let Some(recent) = self.recent_counts.last() {
            if number == self.last_number {
                return (self.settings.shared_counts
                    && independent_voice
                    && utterance_started >= recent.started - HUMAN_OVERLAP_SECONDS
                    && self.history.join(recent.round, number, Speaker::You))
                .then_some(Event::Joined { number });
            }
            if utterance_started < recent.started
                || (!self.settings.shared_counts
                    && utterance_started - recent.started <= HUMAN_OVERLAP_SECONDS)
            {
                return None;
            }
        }
        // Retain the playback guard across a manual reset as room echo decays.
        if self
            .recent_virtual
            .iter()
            .any(|(_, time)| (utterance_started - time).abs() <= HUMAN_OVERLAP_SECONDS)
        {
            return None;
        }
        self.commit(number, Speaker::You.into(), now);
        Some(Event::Count {
            number,
            speakers: Speaker::You.into(),
        })
    }

    pub fn manual_count(&mut self, now: f64) -> Option<Event> {
        if self.phase != Phase::Counting || now >= self.deadline {
            return None;
        }
        if !self.settings.shared_counts && self.last_number != 0 && now < self.next_voice_at {
            return None;
        }
        let number = self.last_number % 10 + 1;
        self.commit(number, Speaker::You.into(), now);
        Some(Event::Count {
            number,
            speakers: Speaker::You.into(),
        })
    }

    pub fn reset_count(&mut self) {
        self.last_number = 0;
        self.recent_counts.clear();
        self.history.rows[0] = [Speakers::default(); 10];
        self.history.missed[0] = [false; 10];
        self.history.current_number = 0;
        // Keep recent spoken numbers in the echo guard while their room echo decays.
    }

    fn commit(&mut self, number: u8, speakers: Speakers, now: f64) {
        self.last_number = number;
        let round = self.history.record(number, speakers);
        self.recent_counts
            .retain(|count| now - count.started <= 4.5);
        self.recent_counts.push(RecentCount {
            started: now,
            round,
        });
        self.turns += 1;
        self.next_voice_at = now + 0.75;
    }

    pub fn snapshot(&self, now: f64) -> Snapshot {
        Snapshot {
            phase: self.phase,
            last_number: self.last_number,
            last_speakers: if self.last_number == 0 {
                Speakers::default()
            } else {
                self.history.rows[usize::from(self.last_number == 10)]
                    [usize::from(self.last_number - 1)]
            },
            history: self.history.clone(),
            elapsed_seconds: if self.phase == Phase::Ready {
                0.0
            } else {
                (now - self.started).max(0.0)
            },
            remaining_seconds: if self.phase == Phase::Counting {
                (self.deadline - now).max(0.0)
            } else {
                0.0
            },
            breath_phase: self.phases,
            cycle_seconds: std::array::from_fn(|i| self.settings.cycle_at(now - self.started, i)),
            turns: self.turns,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn skips_backtracking_and_ten_preserve_every_previously_filled_cell() {
        let mut engine = Engine::new(Settings::default());
        engine.start(0.0);
        assert!(engine.heard(3, 1.0, 1.2, true).is_some());
        assert_eq!(
            engine.history.missed[0],
            [
                true, true, false, false, false, false, false, false, false, false
            ]
        );
        engine.commit(4, Speaker::Companion(1).into(), 3.0);
        let previous = engine.history.rows[0];
        assert!(engine.heard(2, 5.0, 5.2, true).is_some());
        assert_eq!(engine.history.completed_rows, 1);
        assert_eq!(engine.history.rows[1], previous);
        assert_eq!(
            engine.history.missed[1],
            [true, true, false, false, true, true, true, true, true, true]
        );
        assert!(engine.history.missed[0][0]);
        assert_eq!(engine.history.rows[0][1], Speaker::You.into());
        assert!(engine.heard(10, 7.0, 7.2, true).is_some());
        assert_eq!(engine.history.completed_rows, 2);
        assert_eq!(engine.history.rows[2], previous);
        assert_eq!(engine.history.rows[0], [Speakers::default(); 10]);
        assert_eq!(engine.history.missed[0], [false; 10]);
        assert!(engine.history.missed[1][2..9].iter().all(|missed| *missed));
        // A normal restart after ten must not archive a second empty row.
        assert!(engine.heard(1, 9.0, 9.2, true).is_some());
        assert_eq!(engine.history.completed_rows, 2);
    }

    #[test]
    fn queued_companions_count_every_next_number_in_single_count_mode() {
        let mut engine = Engine::new(Settings {
            companions: 4,
            ..Settings::default()
        });
        engine.start(0.0);
        engine.last_tick = 1.0;
        engine.phases = [0.0; 4];
        engine.pending = [Some(1.03), Some(1.02), Some(1.01), Some(1.0)];
        for number in 1..=4 {
            let Some(Event::Count {
                number: spoken,
                speakers,
            }) = engine.tick(number as f64 + 0.3, false)
            else {
                panic!("A waiting companion's turn was lost");
            };
            assert_eq!(spoken, number);
            assert_eq!(speakers, Speaker::Companion(4 - number as usize).into());
        }
    }

    #[test]
    fn varied_number_sequences_never_overwrite_history_or_mix_markers_and_speakers() {
        let mut engine = Engine::new(Settings {
            shared_counts: true,
            duration_minutes: 180.0,
            ..Settings::default()
        });
        engine.start(0.0);
        let mut random = 93847_u64;
        for turn in 1..=1000 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let number = (random >> 32) as u8 % 10 + 1;
            let before = engine.history.clone();
            let now = turn as f64 * 2.0;
            engine.heard(number, now, now + 0.2, true);
            let after = &engine.history;
            let shift = (after.round - before.round) as usize;
            for old_row in 0..before.rows.len() {
                for column in 0..10 {
                    if !before.rows[old_row][column].is_empty() {
                        assert_eq!(
                            before.rows[old_row][column],
                            after.rows[old_row + shift][column]
                        );
                    }
                }
            }
            for row in 0..=after.completed_rows {
                for column in 0..10 {
                    assert!(!after.missed[row][column] || after.rows[row][column].is_empty());
                    if row > 0 {
                        assert!(after.missed[row][column] || !after.rows[row][column].is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn shared_counts_are_opt_in_in_new_and_saved_settings() {
        assert!(!Settings::default().shared_counts);
        let legacy: Settings = serde_json::from_str(r#"{"companions":3}"#).unwrap();
        assert!(!legacy.shared_counts);
        let shared = Settings {
            shared_counts: true,
            ..Settings::default()
        };
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&shared).unwrap()).unwrap();
        assert!(restored.shared_counts);
    }

    #[test]
    fn single_count_mode_keeps_first_speaker_and_rejects_conflicting_numbers() {
        let mut engine = Engine::new(Settings::default());
        engine.start(0.0);
        engine.last_tick = 1.1;
        engine.phases = [0.0; 4];
        // Companion one finishes first, even though companion zero is first in the array.
        engine.pending = [Some(1.1), Some(1.0), None, None];
        assert_eq!(
            engine.tick(1.3, false),
            Some(Event::Count {
                number: 1,
                speakers: Speaker::Companion(1).into(),
            })
        );
        let first = engine.snapshot(1.3).history;
        for number in 1..=10 {
            assert_eq!(engine.heard(number, 1.4, 2.0, true), None);
        }
        assert_eq!(engine.manual_count(1.4), None);
        assert_eq!(engine.snapshot(2.0).history, first);
        assert_eq!(engine.pending[0], Some(1.1));
        assert_eq!(engine.pending[1], None);
        assert!(engine.heard(7, 2.5, 3.0, true).is_some());
        assert!(matches!(
            engine.heard(2, 3.6, 4.0, true),
            Some(Event::Count { number: 2, .. })
        ));
        assert_eq!(engine.snapshot(4.0).history.rows[1][0].len(), 1);
    }

    #[test]
    fn human_first_keeps_waiting_companions_for_later_turns() {
        let mut engine = Engine::new(Settings::default());
        engine.start(0.0);
        engine.pending = [Some(1.1), Some(2.0), None, None];
        assert!(engine.heard(1, 1.0, 1.5, true).is_some());
        assert_eq!(engine.pending[0], Some(1.1));
        assert_eq!(engine.pending[1], Some(2.0));
        assert_eq!(engine.snapshot(1.5).last_speakers, Speaker::You.into());
    }

    #[test]
    fn nearby_companion_breaths_share_one_number() {
        let mut engine = Engine::new(Settings {
            companions: 3,
            shared_counts: true,
            ..Settings::default()
        });
        engine.start(0.0);
        engine.last_tick = 1.1;
        engine.phases = [0.0; 4];
        engine.pending = [Some(1.0), Some(1.1), Some(1.8), None];
        assert_eq!(engine.tick(1.2, false), None);
        let Some(Event::Count {
            number: 1,
            speakers,
        }) = engine.tick(1.3, false)
        else {
            panic!("Two companions should count one together");
        };
        assert_eq!(
            speakers.iter().collect::<Vec<_>>(),
            vec![Speaker::Companion(0), Speaker::Companion(1)]
        );
        assert_eq!(engine.snapshot(1.3).turns, 1);
        let Some(Event::Count {
            number: 2,
            speakers,
        }) = engine.tick(2.2, false)
        else {
            panic!("A later breath should advance normally");
        };
        assert_eq!(speakers, Speaker::Companion(2).into());
    }

    #[test]
    fn human_overlap_joins_a_completed_ten_without_advancing_or_duplicating() {
        let mut engine = Engine::new(Settings {
            shared_counts: true,
            ..Settings::default()
        });
        engine.start(0.0);
        let mut companions = Speakers::from(Speaker::Companion(0));
        companions.insert(Speaker::Companion(1));
        engine.commit(10, companions, 5.0);
        engine.recent_virtual.push((10, 5.0));
        assert_eq!(
            engine.heard(10, 5.1, 6.0, false),
            None,
            "echo is not a second person"
        );
        assert_eq!(
            engine.heard(10, 5.1, 6.1, true),
            Some(Event::Joined { number: 10 })
        );
        engine.manual_count(6.2);
        let snapshot = engine.snapshot(6.2);
        assert_eq!(snapshot.last_number, 1);
        assert_eq!(snapshot.turns, 2);
        assert_eq!(snapshot.history.completed_rows, 1);
        assert_eq!(snapshot.history.rows[1][9].len(), 3);
        assert_eq!(engine.heard(10, 5.1, 6.2, true), None);
        assert_eq!(engine.snapshot(6.2).history, snapshot.history);
        engine.reset_count();
        assert_eq!(engine.heard(10, 5.1, 6.3, true), None);
    }

    #[test]
    fn repeating_the_latest_number_merges_once_and_shared_colors_roll_down() {
        let mut engine = Engine::new(Settings {
            shared_counts: true,
            ..Settings::default()
        });
        engine.start(0.0);
        engine.commit(1, Speaker::Companion(2).into(), 1.0);
        engine.recent_virtual.push((1, 1.0));
        assert_eq!(
            engine.heard(1, 1.8, 2.3, true),
            Some(Event::Joined { number: 1 })
        );
        assert_eq!(engine.heard(1, 1.1, 2.4, true), None);
        assert_eq!(engine.snapshot(2.4).last_speakers.len(), 2);
        let shared = engine.snapshot(2.4).history.rows[0][0];
        for number in 2..=10 {
            engine.manual_count(number as f64 + 2.0);
        }
        assert_eq!(engine.snapshot(12.0).history.rows[1][0], shared);
    }

    #[test]
    fn history_keeps_all_completed_rounds_beyond_the_ten_row_viewport() {
        let mut engine = Engine::new(Settings::default());
        engine.start(0.0);
        assert_eq!(engine.snapshot(0.0).history, CountHistory::default());
        let mut expected = Vec::new();
        for round in 0..12 {
            let mut row = [Speakers::default(); 10];
            for number in 1..=10 {
                let speaker = if (round + number) % 3 == 0 {
                    Speaker::You
                } else {
                    Speaker::Companion((round + number) % 4)
                };
                engine.commit(number as u8, speaker.into(), (round * 10 + number) as f64);
                row[number - 1] = speaker.into();
                if number < 10 {
                    assert_eq!(engine.snapshot(0.0).history.rows[0], row);
                }
            }
            expected.insert(0, row);

            let history = engine.snapshot(0.0).history;
            assert_eq!(history.rows[0], [Speakers::default(); 10]);
            assert_eq!(history.completed_rows, expected.len());
            assert_eq!(&history.rows[1..=expected.len()], expected.as_slice());
        }
    }

    #[test]
    fn history_handles_recognition_resync_reset_resume_and_new_sessions() {
        let mut engine = Engine::new(Settings {
            shared_counts: true,
            ..Settings::default()
        });
        engine.start(0.0);
        engine.manual_count(1.0);
        // A resynchronizing number must not assign invented speakers to skipped dots.
        engine.heard(10, 2.0, 2.5, false).unwrap();
        let completed = engine.snapshot(2.5).history;
        assert_eq!(completed.rows[1][0], Speakers::from(Speaker::You));
        assert_eq!(completed.rows[1][9], Speakers::from(Speaker::You));
        assert_eq!(&completed.rows[1][1..9], &[Speakers::default(); 8]);
        assert!(engine.heard(10, 2.6, 2.9, false).is_none());
        assert_eq!(engine.snapshot(2.9).history, completed);
        engine.manual_count(3.0);
        engine.reset_count();
        assert_eq!(engine.snapshot(3.0).history, completed);
        engine.manual_count(4.0);
        let before_bell = engine.snapshot(4.0).history;
        assert_eq!(engine.tick(600.0, true), Some(Event::Bell));
        engine.extend(610.0);
        assert_eq!(engine.snapshot(610.0).history, before_bell);
        engine.stop();
        engine.start(620.0);
        assert_eq!(engine.snapshot(620.0).history, CountHistory::default());
    }

    #[test]
    fn human_and_companions_share_counter_and_wrap() {
        let mut engine = Engine::new(Settings::default());
        engine.start(0.0);
        for time in 1..=10 {
            engine.tick(time as f64, true);
            engine.manual_count(time as f64);
        }
        assert_eq!(engine.snapshot(10.0).last_number, 10);
        let Some(Event::Count {
            number: 1,
            speakers,
        }) = engine.tick(11.0, false)
        else {
            panic!("Expected a companion to wrap to one");
        };
        assert_eq!(engine.snapshot(11.0).history.rows[0][0], speakers);
        assert_eq!(
            engine.snapshot(11.0).history.rows[1],
            [Speakers::from(Speaker::You); 10]
        );
    }
    #[test]
    fn deadline_wins_over_a_pending_voice_and_bell_is_once() {
        let settings = Settings {
            duration_minutes: 0.1,
            ..Settings::default()
        };
        let mut engine = Engine::new(settings);
        engine.start(0.0);
        for n in 1..60 {
            engine.tick(n as f64 / 10.0, true);
        }
        assert_eq!(engine.tick(6.0, true), Some(Event::Bell));
        assert_eq!(engine.tick(7.0, false), None);
        assert_eq!(engine.heard(3, 5.8, 6.1, false), None);
        assert_eq!(engine.manual_count(7.0), None);
    }
    #[test]
    fn extension_preserves_count_and_settling_progress() {
        let settings = Settings {
            duration_minutes: 0.1,
            ..Settings::default()
        };
        let mut engine = Engine::new(settings);
        engine.start(10.0);
        engine.manual_count(11.0);
        engine.tick(16.0, true);
        engine.extend(100.0);
        let view = engine.snapshot(100.0);
        assert_eq!(view.last_number, 1);
        assert_eq!(view.elapsed_seconds, 90.0);
        assert_eq!(view.remaining_seconds, 180.0);
        assert_eq!(engine.tick(100.0, false), None);
    }
    #[test]
    fn echo_and_stale_recognition_never_advance_counter() {
        let mut engine = Engine::new(Settings {
            shared_counts: true,
            ..Settings::default()
        });
        engine.start(0.0);
        let mut spoken = None;
        for tick in 1..100 {
            let now = tick as f64 * 0.05;
            if let Some(Event::Count { number, .. }) = engine.tick(now, false) {
                spoken = Some((number, now));
                break;
            }
        }
        let (number, time) = spoken.unwrap();
        assert_eq!(engine.heard(number, time + 0.1, time + 0.7, false), None);
        assert_eq!(engine.heard(7, time, time + 5.0, false), None);
        assert_eq!(
            engine.heard(7, time + 1.0, time + 1.5, false),
            Some(Event::Count {
                number: 7,
                speakers: Speaker::You.into()
            })
        );
    }
    #[test]
    fn independent_rhythms_are_not_round_robin() {
        let settings = Settings {
            companions: 2,
            pace: [0.65, 1.5, 1.0, 1.0],
            cycle_seconds: [4.0; 3],
            ..Settings::default()
        };
        let mut engine = Engine::new(settings);
        engine.start(0.0);
        let mut speakers = Vec::new();
        for tick in 1..3000 {
            if let Some(Event::Count {
                speakers: group, ..
            }) = engine.tick(tick as f64 * 0.02, false)
            {
                speakers.extend(group.iter());
            }
        }
        assert!(
            speakers
                .windows(2)
                .any(|pair| pair == [Speaker::Companion(0); 2])
        );
    }
    #[test]
    fn bad_saved_settings_are_safe_and_curve_has_exact_knots() {
        let mut settings = Settings {
            companions: 9,
            duration_minutes: f64::NAN,
            ..Settings::default()
        };
        settings.sanitize();
        assert_eq!(settings.companions, 4);
        assert_eq!(settings.duration_minutes, 10.0);
        assert_eq!(settings.cycle_at(0.0, 0), 5.0 * 0.85);
        assert_eq!(settings.cycle_at(450.0, 0), 7.0 * 0.85);
        assert_eq!(settings.cycle_at(900.0, 0), 9.0 * 0.85);
    }
}
