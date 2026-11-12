//! Микрофон, Opus и воспроизведение. Сервер звук не декодирует:
//! сюда приходит чужой Opus, отсюда уходит свой.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SupportedStreamConfig};
use opus1::{Application, Channels, OpusDecoder, OpusEncoder, SampleRate};

pub const FRAME: usize = 960;

type Queues = Mutex<Vec<Arc<Mutex<VecDeque<i16>>>>>;
type Mix = Arc<Queues>;

/// Подчищает один кадр микрофона перед кодированием Opus.
/// Сейчас возвращает кадр как есть. Позже сюда встанет шумодав,
/// не трогая сервер и протокол.
pub fn level(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let energy: f32 = frame
        .iter()
        .map(|sample| {
            let unit = *sample as f32 / 32768.0;
            unit * unit
        })
        .sum();
    (energy / frame.len() as f32).sqrt()
}

pub fn clean_mic(frame: &mut [i16]) {
    let _ = frame;
}

pub struct Codec {
    encoder: OpusEncoder,
    decoder: OpusDecoder,
}

impl Codec {
    pub fn new() -> Result<Self, String> {
        let encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|err| err.to_string())?;
        // Стереодекодер принимает и моно, и стерео от браузера.
        let decoder = OpusDecoder::new(SampleRate::Hz48000, Channels::Stereo)
            .map_err(|err| err.to_string())?;
        Ok(Self { encoder, decoder })
    }

    pub fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>, String> {
        let mut packet = [0u8; 1500];
        let size = self
            .encoder
            .encode_i16(frame, FRAME, &mut packet)
            .map_err(|err| err.to_string())?;
        Ok(packet[..size].to_vec())
    }

    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, String> {
        let mut pcm = [0i16; 5760 * 2];
        let samples = self
            .decoder
            .decode_i16(packet, 5760, &mut pcm)
            .map_err(|err| err.to_string())?;
        let mono = (0..samples)
            .map(|index| {
                let left = pcm[index * 2] as i32;
                let right = pcm[index * 2 + 1] as i32;
                ((left + right) / 2) as i16
            })
            .collect();
        Ok(mono)
    }
}

pub enum Capture {
    Device {
        _stream: cpal::Stream,
        samples: Arc<Mutex<VecDeque<f32>>>,
        rate: u32,
        cursor: f64,
    },
    Synth {
        tone: bool,
        next: Instant,
        phase: u64,
    },
}

impl Capture {
    pub fn device() -> Result<Self, String> {
        crate::permit::ensure_microphone()?;
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "нет доступа к микрофону".to_string())?;
        let config = choose_config(device.supported_input_configs().map_err(mic_error)?)?;
        let rate = config.sample_rate();
        let channels = config.channels() as usize;
        let samples = Arc::new(Mutex::new(VecDeque::new()));
        let incoming = Arc::clone(&samples);
        let stream = build_input(&device, &config, channels, incoming)?;
        stream.play().map_err(mic_error)?;
        Ok(Self::Device {
            _stream: stream,
            samples,
            rate,
            cursor: 0.0,
        })
    }

    pub fn synth(tone: bool) -> Self {
        Self::Synth {
            tone,
            next: Instant::now(),
            phase: 0,
        }
    }

    pub fn is_device(&self) -> bool {
        matches!(self, Self::Device { .. })
    }

    pub fn next_frame(&mut self) -> Option<[i16; FRAME]> {
        match self {
            Self::Device {
                samples,
                rate,
                cursor,
                ..
            } => pull_device(samples, *rate, cursor),
            Self::Synth { tone, next, phase } => {
                if Instant::now() < *next {
                    return None;
                }
                *next += Duration::from_millis(20);
                if next.elapsed() > Duration::from_millis(100) {
                    *next = Instant::now() + Duration::from_millis(20);
                }
                let frame = synth_frame(*tone, *phase);
                *phase += 1;
                Some(frame)
            }
        }
    }
}

pub struct Playback {
    _stream: cpal::Stream,
    queues: Mix,
    deaf: Arc<std::sync::atomic::AtomicBool>,
}

impl Playback {
    pub fn open() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "нет устройства воспроизведения".to_string())?;
        // Не переключать устройство на 48 кГц: на Mac это часто заканчивается
        // «Sample rate update timed out», хотя свои 24 кГц открываются сразу.
        let config = device
            .default_output_config()
            .map_err(|err| format!("нет устройства воспроизведения: {err}"))?;
        let channels = config.channels() as usize;
        let rate = config.sample_rate();
        let queues = Arc::new(Mutex::new(Vec::new()));
        let deaf = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shared = Arc::clone(&queues);
        let silent = Arc::clone(&deaf);
        let stream = build_output(&device, &config, channels, rate, shared, silent)?;
        stream
            .play()
            .map_err(|_| "нет устройства воспроизведения".to_string())?;
        Ok(Self {
            _stream: stream,
            queues,
            deaf,
        })
    }

    pub fn set_deaf(&self, deaf: bool) {
        self.deaf.store(deaf, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn add_queue(&self) -> Arc<Mutex<VecDeque<i16>>> {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        self.queues
            .lock()
            .expect("playback")
            .push(Arc::clone(&queue));
        queue
    }
}

pub fn push_playback(queue: &Mutex<VecDeque<i16>>, samples: Vec<i16>) {
    let mut queue = queue.lock().expect("playback");
    queue.extend(samples);
    let max = 48_000 / 5;
    if queue.len() > max {
        let keep = 48_000 / 1000 * 60;
        let drop_count = queue.len() - keep;
        queue.drain(..drop_count);
    }
}

fn synth_frame(tone: bool, index: u64) -> [i16; FRAME] {
    let mut frame = [0i16; FRAME];
    if tone {
        for (offset, sample) in frame.iter_mut().enumerate() {
            let t = (index * FRAME as u64 + offset as u64) as f32 / 48_000.0;
            *sample = ((t * 440.0 * std::f32::consts::TAU).sin() * 8000.0) as i16;
        }
    }
    frame
}

fn pull_device(
    samples: &Mutex<VecDeque<f32>>,
    rate: u32,
    cursor: &mut f64,
) -> Option<[i16; FRAME]> {
    let mut src = samples.lock().expect("mic");
    if rate == 48_000 {
        if src.len() < FRAME {
            return None;
        }
        let mut frame = [0i16; FRAME];
        for sample in &mut frame {
            *sample = to_i16(src.pop_front().unwrap_or(0.0));
        }
        return Some(frame);
    }
    let step = f64::from(rate) / 48_000.0;
    let last = *cursor + step * (FRAME as f64 - 1.0);
    let needed = last.ceil() as usize + 1;
    if src.len() < needed {
        return None;
    }
    let snapshot: Vec<f32> = src.iter().take(needed).copied().collect();
    let mut frame = [0i16; FRAME];
    for (index, sample) in frame.iter_mut().enumerate() {
        let at = *cursor + step * index as f64;
        let idx = at.floor() as usize;
        let frac = (at - idx as f64) as f32;
        let left = snapshot[idx];
        let right = snapshot.get(idx + 1).copied().unwrap_or(left);
        *sample = to_i16(left + (right - left) * frac);
    }
    let consume = *cursor + step * FRAME as f64;
    let count = (consume.floor() as usize).min(src.len());
    src.drain(..count);
    *cursor = consume - count as f64;
    Some(frame)
}

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

fn choose_config(
    configs: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
) -> Result<SupportedStreamConfig, String> {
    let range = configs
        .min_by_key(|config| {
            let covers = config.min_sample_rate() <= 48_000 && config.max_sample_rate() >= 48_000;
            let penalty = if covers { 0 } else { 1000 };
            penalty + u32::from(config.channels())
        })
        .ok_or_else(|| "нет доступа к микрофону".to_string())?;
    let rate = if range.min_sample_rate() <= 48_000 && range.max_sample_rate() >= 48_000 {
        48_000
    } else {
        range.max_sample_rate()
    };
    Ok(range.with_sample_rate(rate))
}

fn build_input(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    channels: usize,
    incoming: Arc<Mutex<VecDeque<f32>>>,
) -> Result<cpal::Stream, String> {
    let err = |error| tracing::warn!(%error, "микрофон");
    match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            (*config).into(),
            move |data: &[f32], _| push_mono(&incoming, data, channels),
            err,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            (*config).into(),
            move |data: &[i16], _| {
                let floats: Vec<f32> = data
                    .iter()
                    .map(|sample| *sample as f32 / i16::MAX as f32)
                    .collect();
                push_mono(&incoming, &floats, channels);
            },
            err,
            None,
        ),
        _ => return Err("нет доступа к микрофону".into()),
    }
    .map_err(mic_error)
}

struct Speaker {
    samples: VecDeque<i16>,
    cursor: f64,
}

fn build_output(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    channels: usize,
    rate: u32,
    queues: Mix,
    deaf: Arc<std::sync::atomic::AtomicBool>,
) -> Result<cpal::Stream, String> {
    let err = |error| tracing::warn!(%error, "воспроизведение");
    let step = if rate == 0 {
        1.0
    } else {
        48_000.0 / f64::from(rate)
    };
    let speaker = Arc::new(Mutex::new(Speaker {
        samples: VecDeque::new(),
        cursor: 0.0,
    }));
    let fail = |err: cpal::Error| format!("нет устройства воспроизведения: {err}");
    match config.sample_format() {
        SampleFormat::F32 => {
            let queues = Arc::clone(&queues);
            let deaf = Arc::clone(&deaf);
            let speaker = Arc::clone(&speaker);
            device
                .build_output_stream(
                    (*config).into(),
                    move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let silent = deaf.load(std::sync::atomic::Ordering::Relaxed);
                        let mut speaker = speaker.lock().expect("playback");
                        for frame in output.chunks_mut(channels.max(1)) {
                            let sample = speaker.next_sample(&queues, step);
                            let sample = if silent { 0.0 } else { sample };
                            for slot in frame {
                                *slot = sample;
                            }
                        }
                    },
                    err,
                    None,
                )
                .map_err(fail)
        }
        SampleFormat::I16 => {
            let queues = Arc::clone(&queues);
            let deaf = Arc::clone(&deaf);
            device
                .build_output_stream(
                    (*config).into(),
                    move |output: &mut [i16], _| {
                        let silent = deaf.load(std::sync::atomic::Ordering::Relaxed);
                        let mut speaker = speaker.lock().expect("playback");
                        for frame in output.chunks_mut(channels.max(1)) {
                            let sample = if silent {
                                0
                            } else {
                                (speaker.next_sample(&queues, step) * i16::MAX as f32) as i16
                            };
                            for slot in frame {
                                *slot = sample;
                            }
                        }
                    },
                    err,
                    None,
                )
                .map_err(fail)
        }
        other => Err(format!("нет устройства воспроизведения: формат {other:?}")),
    }
}

impl Speaker {
    fn next_sample(&mut self, queues: &Queues, step: f64) -> f32 {
        loop {
            let need = self.cursor.floor() as usize + 2;
            if self.samples.len() >= need {
                break;
            }
            let Some(sample) = mix_one(queues) else {
                if self.samples.len() < 2 {
                    self.samples.clear();
                    self.cursor = 0.0;
                    return 0.0;
                }
                break;
            };
            self.samples.push_back(sample);
        }
        let index = self.cursor.floor() as usize;
        if index + 1 >= self.samples.len() {
            return 0.0;
        }
        let frac = (self.cursor - index as f64) as f32;
        let left = f32::from(self.samples[index]);
        let right = f32::from(self.samples[index + 1]);
        self.cursor += step;
        let drop_count = self.cursor.floor() as usize;
        if drop_count > 0 {
            let drop_count = drop_count.min(self.samples.len());
            self.samples.drain(..drop_count);
            self.cursor -= drop_count as f64;
        }
        (left + (right - left) * frac) / i16::MAX as f32
    }
}

fn mix_one(queues: &Queues) -> Option<i16> {
    let queues = queues.lock().expect("playback");
    let mut heard = false;
    let mut sum = 0i32;
    for queue in queues.iter() {
        if let Some(sample) = queue.lock().expect("pcm").pop_front() {
            heard = true;
            sum += i32::from(sample);
        }
    }
    heard.then_some(sum.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16)
}

fn push_mono(buf: &Mutex<VecDeque<f32>>, data: &[f32], channels: usize) {
    if channels == 0 {
        return;
    }
    let mut buf = buf.lock().expect("mic");
    for frame in data.chunks(channels) {
        let sum: f32 = frame.iter().sum();
        buf.push_back(sum / channels as f32);
    }
    let max = 48_000 * 2;
    if buf.len() > max {
        let extra = buf.len() - max;
        buf.drain(..extra);
    }
}

fn mic_error(err: impl ToString) -> String {
    let _ = err;
    "нет доступа к микрофону".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_roundtrip_keeps_a_tone() {
        let mut codec = Codec::new().unwrap();
        let mut loud = 0i16;
        for index in 0..8 {
            let frame = synth_frame(true, index);
            let packet = codec.encode(&frame).unwrap();
            let decoded = codec.decode(&packet).unwrap();
            loud = loud.max(decoded.iter().map(|sample| sample.abs()).max().unwrap_or(0));
        }
        assert!(loud > 1000, "тон потерялся, пик {loud}");
    }

    #[test]
    fn clean_mic_leaves_samples() {
        let mut frame = [1i16, -2, 3];
        clean_mic(&mut frame);
        assert_eq!(frame, [1, -2, 3]);
    }
}
