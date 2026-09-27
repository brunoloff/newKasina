use crate::dsp::Resampler;
use anyhow::{Context, Result, bail};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::Instant,
};

#[derive(Debug)]
pub(crate) enum Frame {
    Render([f32; 160]),
    Capture([f32; 160], Instant),
    Error(String),
}
#[derive(Debug, Default)]
struct Playback {
    samples: VecDeque<f32>,
}
#[derive(Clone)]
struct FrameSender {
    sender: SyncSender<Frame>,
    overflow: Arc<AtomicBool>,
}
impl FrameSender {
    fn try_send(&self, frame: Frame) {
        if matches!(
            self.sender.try_send(frame),
            Err(mpsc::TrySendError::Full(_))
        ) {
            self.overflow.store(true, Ordering::Relaxed);
        }
    }
}
pub(crate) struct Audio {
    _output: Stream,
    input: Option<Stream>,
    playback: Arc<Mutex<Playback>>,
    pub frames: Receiver<Frame>,
    sender: FrameSender,
    rate: u32,
    pub microphone: String,
    pub output: String,
}
impl Audio {
    pub fn open() -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .context("No audio output device. Choose speakers in your system sound settings.")?;
        let supported = device.default_output_config()?;
        let config = supported.config();
        let (sender, frames) = mpsc::sync_channel(128);
        let sender = FrameSender {
            sender,
            overflow: Arc::new(AtomicBool::new(false)),
        };
        let playback = Arc::new(Mutex::new(Playback::default()));
        let output = match supported.sample_format() {
            SampleFormat::F32 => {
                output_stream::<f32>(&device, &config, playback.clone(), sender.clone())?
            }
            SampleFormat::I16 => {
                output_stream::<i16>(&device, &config, playback.clone(), sender.clone())?
            }
            SampleFormat::U16 => {
                output_stream::<u16>(&device, &config, playback.clone(), sender.clone())?
            }
            format => bail!(
                "Unsupported output format {format}; select a different default output in system sound settings."
            ),
        };
        output.play()?;
        let mut audio = Self {
            _output: output,
            input: None,
            playback,
            frames,
            sender,
            rate: config.sample_rate.0,
            microphone: String::new(),
            output: device.name().unwrap_or_else(|_| "Default output".into()),
        };
        audio.open_microphone()?;
        while audio.frames.try_recv().is_ok() {}
        audio.sender.overflow.store(false, Ordering::Relaxed);
        Ok(audio)
    }
    pub fn open_microphone(&mut self) -> Result<()> {
        if self.input.is_some() {
            return Ok(());
        }
        let device = cpal::default_host().default_input_device().context(
            "No microphone. Select one in system sound settings and allow microphone access.",
        )?;
        let supported = device.default_input_config()?;
        let config = supported.config();
        let input = match supported.sample_format() {
            SampleFormat::F32 => input_stream::<f32>(&device, &config, self.sender.clone())?,
            SampleFormat::I16 => input_stream::<i16>(&device, &config, self.sender.clone())?,
            SampleFormat::U16 => input_stream::<u16>(&device, &config, self.sender.clone())?,
            format => bail!(
                "Unsupported microphone format {format}; select a different default microphone in system sound settings."
            ),
        };
        input
            .play()
            .context("Start microphone; check system microphone permission")?;
        self.microphone = device
            .name()
            .unwrap_or_else(|_| "Default microphone".into());
        self.input = Some(input);
        Ok(())
    }
    pub fn take_overflow(&self) -> bool {
        self.sender.overflow.swap(false, Ordering::Relaxed)
    }
    pub fn close_microphone(&mut self) {
        self.input.take();
    }
    pub fn busy(&self) -> bool {
        !self
            .playback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .samples
            .is_empty()
    }
    pub fn clear(&self) {
        self.playback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .samples
            .clear();
    }
    pub fn play(&self, samples: &[f32], volume: f32) {
        let mut converted = VecDeque::new();
        let mut converter = Resampler::new(16000, self.rate);
        for &sample in samples {
            converter.push(sample * volume, |value| converted.push_back(value));
        }
        self.playback
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .samples = converted;
    }
}
fn output_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &StreamConfig,
    playback: Arc<Mutex<Playback>>,
    sender: FrameSender,
) -> Result<Stream> {
    let channels = config.channels as usize;
    let errors = sender.clone();
    let mut resampler = Resampler::new(config.sample_rate.0, 16000);
    let mut frame = [0.0; 160];
    let mut length = 0;
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let mut queue = playback.try_lock().ok();
            for channels in data.chunks_mut(channels) {
                let sample = queue
                    .as_mut()
                    .and_then(|queue| queue.samples.pop_front())
                    .unwrap_or(0.0);
                channels.fill(T::from_sample(sample));
                resampler.push(sample, |sample| {
                    frame[length] = sample;
                    length += 1;
                    if length == 160 {
                        sender.try_send(Frame::Render(frame));
                        length = 0;
                    }
                });
            }
        },
        move |error| {
            errors.try_send(Frame::Error(format!("Speaker output: {error}")));
        },
        None,
    )?)
}
fn input_stream<T: SizedSample + Sample>(
    device: &cpal::Device,
    config: &StreamConfig,
    sender: FrameSender,
) -> Result<Stream>
where
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let errors = sender.clone();
    let mut resampler = Resampler::new(config.sample_rate.0, 16000);
    let mut frame = [0.0; 160];
    let mut length = 0;
    Ok(device.build_input_stream(
        config,
        move |data: &[T], _| {
            for chunk in data.chunks(channels) {
                let sample = chunk
                    .iter()
                    .map(|sample| f32::from_sample(*sample))
                    .sum::<f32>()
                    / channels as f32;
                resampler.push(sample, |sample| {
                    frame[length] = sample;
                    length += 1;
                    if length == 160 {
                        sender.try_send(Frame::Capture(frame, Instant::now()));
                        length = 0;
                    }
                });
            }
        },
        move |error| {
            errors.try_send(Frame::Error(format!("Microphone: {error}")));
        },
        None,
    )?)
}
