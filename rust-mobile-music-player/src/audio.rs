//! Background Audio Engine Thread with 5-Band Biquad Equalizer
//! Runs on a dedicated OS thread. Decodes audio files using rodio and symphonia,
//! applies real-time 5-band EQ filtering, manages playback state, volume, seeking,
//! and emits periodic 50ms progress updates.

use crossbeam_channel::{Receiver, Sender};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink, Source};
use std::f32::consts::PI;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// 5-Band EQ gains in dB (-12.0 to +12.0) at 60Hz, 230Hz, 910Hz, 3.6kHz, 14kHz
pub type EqGains = [f32; 5];

/// Commands sent from the UI Thread to the Audio Engine.
#[derive(Debug, Clone)]
pub enum AudioCommand {
    Play(PathBuf),
    Pause,
    Resume,
    Stop,
    Seek(Duration),
    SetVolume(f32),
    SetMute(bool),
    SetEq(EqGains),
}

/// Events emitted from the Audio Engine back to the UI Thread.
#[derive(Debug, Clone)]
pub enum AudioEvent {
    PlaybackStarted {
        path: PathBuf,
        total_duration: Option<Duration>,
    },
    Progress {
        position: Duration,
        total_duration: Option<Duration>,
    },
    PlaybackFinished,
    Error(String),
}

/// Lightweight 2nd-order Biquad peaking filter per channel
#[derive(Clone, Default)]
struct BiquadState {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

#[derive(Clone)]
struct BiquadCoeffs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoeffs {
    fn peaking(sample_rate: f32, freq: f32, q: f32, gain_db: f32) -> Self {
        let a = 10.0_f32.powf(gain_db / 40.0);
        let omega = 2.0 * PI * (freq / sample_rate).clamp(0.001, 0.49);
        let alpha = omega.sin() / (2.0 * q);
        let cos_w = omega.cos();

        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha / a;

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
        }
    }

    fn process(&self, sample: f32, state: &mut BiquadState) -> f32 {
        let y = self.b0 * sample + self.b1 * state.x1 + self.b2 * state.x2
            - self.a1 * state.y1
            - self.a2 * state.y2;
        state.x2 = state.x1;
        state.x1 = sample;
        state.y2 = state.y1;
        state.y1 = y;
        y
    }
}

/// Rodio Source wrapper applying real-time 5-band Equalizer DSP
struct EqualizerSource<S>
where
    S: Source<Item = f32>,
{
    inner: S,
    shared_gains: Arc<Mutex<EqGains>>,
    last_gains: EqGains,
    coeffs: Vec<BiquadCoeffs>,
    states: Vec<Vec<BiquadState>>, // [band][channel]
    channel_idx: usize,
}

impl<S> EqualizerSource<S>
where
    S: Source<Item = f32>,
{
    fn new(inner: S, shared_gains: Arc<Mutex<EqGains>>) -> Self {
        let channels = inner.channels().max(1) as usize;
        let sample_rate = inner.sample_rate().max(8000) as f32;
        let gains = *shared_gains.lock().unwrap();
        let coeffs = Self::build_coeffs(sample_rate, &gains);
        let states = vec![vec![BiquadState::default(); channels]; 5];

        Self {
            inner,
            shared_gains,
            last_gains: gains,
            coeffs,
            states,
            channel_idx: 0,
        }
    }

    fn build_coeffs(sample_rate: f32, gains: &EqGains) -> Vec<BiquadCoeffs> {
        const BANDS: [f32; 5] = [60.0, 230.0, 910.0, 3600.0, 14000.0];
        BANDS
            .iter()
            .zip(gains.iter())
            .map(|(&freq, &db)| BiquadCoeffs::peaking(sample_rate, freq, 1.0, db))
            .collect()
    }
}

impl<S> Iterator for EqualizerSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let mut sample = self.inner.next()?;
        let channels = self.inner.channels().max(1) as usize;

        if self.channel_idx == 0 {
            if let Ok(current) = self.shared_gains.try_lock() {
                if *current != self.last_gains {
                    self.last_gains = *current;
                    let sr = self.inner.sample_rate().max(8000) as f32;
                    self.coeffs = Self::build_coeffs(sr, &self.last_gains);
                }
            }
        }

        let ch = self.channel_idx % channels;
        for (band_idx, coeff) in self.coeffs.iter().enumerate() {
            if self.last_gains[band_idx].abs() > 0.05 {
                sample = coeff.process(sample, &mut self.states[band_idx][ch]);
            }
        }

        self.channel_idx = (self.channel_idx + 1) % channels;
        Some(sample.clamp(-1.0, 1.0))
    }
}

impl<S> Source for EqualizerSource<S>
where
    S: Source<Item = f32>,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }
    fn channels(&self) -> u16 {
        self.inner.channels()
    }
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }
    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        self.inner.try_seek(pos)
    }
}

struct AudioState {
    sink: Option<Sink>,
    current_path: Option<PathBuf>,
    is_paused: bool,
    volume: f32,
    is_muted: bool,
    eq_gains: Arc<Mutex<EqGains>>,
    total_duration: Option<Duration>,
    base_offset: Duration,
    resume_instant: Option<Instant>,
}

impl AudioState {
    fn new() -> Self {
        Self {
            sink: None,
            current_path: None,
            is_paused: false,
            volume: 0.85,
            is_muted: false,
            eq_gains: Arc::new(Mutex::new([0.0; 5])),
            total_duration: None,
            base_offset: Duration::ZERO,
            resume_instant: None,
        }
    }

    fn current_position(&self) -> Duration {
        if self.sink.is_none() {
            return Duration::ZERO;
        }
        if self.is_paused {
            self.base_offset
        } else if let Some(instant) = self.resume_instant {
            let total = self.base_offset + instant.elapsed();
            if let Some(dur) = self.total_duration {
                total.min(dur)
            } else {
                total
            }
        } else {
            self.base_offset
        }
    }

    fn apply_volume(&self) {
        if let Some(sink) = &self.sink {
            sink.set_volume(if self.is_muted { 0.0 } else { self.volume });
        }
    }
}

pub fn spawn_audio_thread(
    cmd_receiver: Receiver<AudioCommand>,
    event_sender: Sender<AudioEvent>,
) {
    thread::Builder::new()
        .name("mobile_audio_engine".into())
        .spawn(move || {
            let (_stream, stream_handle): (OutputStream, OutputStreamHandle) =
                match OutputStream::try_default() {
                    Ok(res) => res,
                    Err(err) => {
                        let _ = event_sender.send(AudioEvent::Error(format!(
                            "Audio output device initialization failed: {err}"
                        )));
                        return;
                    }
                };

            let mut state = AudioState::new();
            let tick_rate = Duration::from_millis(50);
            let mut last_progress_send = Instant::now();

            loop {
                match cmd_receiver.recv_timeout(tick_rate) {
                    Ok(command) => match command {
                        AudioCommand::Play(path) => {
                            match play_file(&stream_handle, &mut state, path.clone()) {
                                Ok(duration) => {
                                    state.total_duration = duration;
                                    state.base_offset = Duration::ZERO;
                                    state.resume_instant = Some(Instant::now());
                                    state.is_paused = false;
                                    state.current_path = Some(path.clone());

                                    let _ = event_sender.send(AudioEvent::PlaybackStarted {
                                        path,
                                        total_duration: duration,
                                    });
                                }
                                Err(err) => {
                                    let _ = event_sender.send(AudioEvent::Error(err));
                                }
                            }
                        }
                        AudioCommand::Pause => {
                            if let Some(sink) = &state.sink {
                                if !state.is_paused {
                                    sink.pause();
                                    state.base_offset = state.current_position();
                                    state.resume_instant = None;
                                    state.is_paused = true;
                                }
                            }
                        }
                        AudioCommand::Resume => {
                            if let Some(sink) = &state.sink {
                                if state.is_paused {
                                    sink.play();
                                    state.resume_instant = Some(Instant::now());
                                    state.is_paused = false;
                                }
                            }
                        }
                        AudioCommand::Stop => {
                            state.sink = None;
                            state.current_path = None;
                            state.is_paused = false;
                            state.base_offset = Duration::ZERO;
                            state.resume_instant = None;
                            state.total_duration = None;
                        }
                        AudioCommand::Seek(target) => {
                            if let Some(sink) = &mut state.sink {
                                if sink.try_seek(target).is_ok() {
                                    state.base_offset = target;
                                    if !state.is_paused {
                                        state.resume_instant = Some(Instant::now());
                                    }
                                }
                            }
                        }
                        AudioCommand::SetVolume(vol) => {
                            state.volume = vol.clamp(0.0, 1.0);
                            state.apply_volume();
                        }
                        AudioCommand::SetMute(muted) => {
                            state.is_muted = muted;
                            state.apply_volume();
                        }
                        AudioCommand::SetEq(gains) => {
                            if let Ok(mut lock) = state.eq_gains.lock() {
                                *lock = gains;
                            }
                        }
                    },
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                }

                if let Some(sink) = &state.sink {
                    if sink.empty() && !state.is_paused {
                        state.sink = None;
                        state.current_path = None;
                        state.resume_instant = None;
                        let _ = event_sender.send(AudioEvent::PlaybackFinished);
                        continue;
                    }

                    if last_progress_send.elapsed() >= Duration::from_millis(50) {
                        last_progress_send = Instant::now();
                        let pos = state.current_position();
                        let _ = event_sender.send(AudioEvent::Progress {
                            position: pos,
                            total_duration: state.total_duration,
                        });
                    }
                }
            }
        })
        .expect("Failed to spawn mobile audio worker thread");
}

fn play_file(
    handle: &OutputStreamHandle,
    state: &mut AudioState,
    path: PathBuf,
) -> Result<Option<Duration>, String> {
    let file = File::open(&path).map_err(|e| format!("Could not open file {:?}: {e}", path))?;
    let reader = BufReader::new(file);

    let decoder = Decoder::new(reader).map_err(|e| format!("Audio decode error: {e}"))?;
    let duration = decoder.total_duration();
    let eq_source = EqualizerSource::new(decoder.convert_samples::<f32>(), state.eq_gains.clone());

    let sink = Sink::try_new(handle).map_err(|e| format!("Sink creation error: {e}"))?;
    sink.append(eq_source);

    state.sink = Some(sink);
    state.apply_volume();

    Ok(duration)
}
