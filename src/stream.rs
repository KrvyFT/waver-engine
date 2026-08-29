//! Open the default output device and run [`crate::Engine`] in the cpal callback.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SAMPLE_RATE_48K, SampleFormat, Stream, StreamConfig, SupportedStreamConfigRange};
use rtrb::{Consumer, Producer, RingBuffer};
use waver_core::{EngineStatus, RtCommand};

use crate::{BLOCK, Engine};

const COMMAND_CAPACITY: usize = 256;

/// GUI-side handle: command producer, status atomics, and the live stream.
///
/// Dropping this stops the callback.
pub struct AudioRuntime {
    commands: Option<Producer<RtCommand>>,
    /// Cross-thread meters. Never lock this from the GUI.
    pub status: Arc<EngineStatus>,
    /// Negotiated device label, or empty if open failed.
    pub device_name: String,
    /// Human-readable open/play failure. Window still starts.
    pub error: Option<String>,
    _stream: Option<Stream>,
}

impl AudioRuntime {
    /// Move the SPSC producer to the GUI. Call once at startup.
    pub fn take_commands(&mut self) -> Option<Producer<RtCommand>> {
        self.commands.take()
    }
}

/// Failures while opening or starting the output stream.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// Host has no default output.
    #[error("no default output device")]
    NoOutputDevice,
    /// Device cannot run f32 output (skeleton does not convert).
    #[error("output device '{device}' has no f32 configuration")]
    NoF32Config {
        /// Device name from the host.
        device: String,
    },
    /// Wrapped cpal / host error.
    #[error("{context}: {message}")]
    Cpal {
        /// Which step failed.
        context: &'static str,
        /// Display of the host error. Named `message` so thiserror does not treat it as `Error::source`.
        message: String,
    },
    /// `Stream::play` failed.
    #[error("failed to start output stream: {0}")]
    Play(String),
}

/// Create a command queue and try to start a silent f32 output stream.
///
/// Never panics on missing hardware: [`AudioRuntime::error`] is set instead.
pub fn spawn_output() -> AudioRuntime {
    let status = Arc::new(EngineStatus::new());
    status.set_format(0, BLOCK as u32, 0);
    let (producer, consumer) = RingBuffer::<RtCommand>::new(COMMAND_CAPACITY);

    match try_open(consumer, Arc::clone(&status)) {
        Ok((stream, device_name)) => match stream.play() {
            Ok(()) => {
                status.set_running(true);
                AudioRuntime {
                    commands: Some(producer),
                    status,
                    device_name,
                    error: None,
                    _stream: Some(stream),
                }
            }
            Err(err) => AudioRuntime {
                commands: Some(producer),
                status,
                device_name,
                error: Some(EngineError::Play(err.to_string()).to_string()),
                _stream: None,
            },
        },
        Err(err) => AudioRuntime {
            commands: Some(producer),
            status,
            device_name: String::new(),
            error: Some(err.to_string()),
            _stream: None,
        },
    }
}

fn try_open(
    mut consumer: Consumer<RtCommand>,
    status: Arc<EngineStatus>,
) -> Result<(Stream, String), EngineError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(EngineError::NoOutputDevice)?;
    let device_name = device
        .description()
        .map(|desc| desc.name().to_owned())
        .unwrap_or_else(|_| device.to_string());
    let supported = pick_f32_config(&device, &device_name)?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config: StreamConfig = supported.into();

    status.set_format(sample_rate, BLOCK as u32, channels as u32);

    let mut engine = Engine::new(sample_rate as f32, channels);
    let err_status = Arc::clone(&status);
    let block_samples = BLOCK * channels.max(1);

    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _info| {
                while let Ok(cmd) = consumer.pop() {
                    engine.apply_rt(cmd);
                }
                let mut written = 0;
                while written < data.len() {
                    let n = (data.len() - written).min(block_samples);
                    engine.process_block(&mut data[written..written + n]);
                    written += n;
                }
            },
            move |_err| {
                err_status.bump_xrun();
            },
            None,
        )
        .map_err(|err| EngineError::Cpal {
            context: "build_output_stream",
            message: err.to_string(),
        })?;

    Ok((stream, device_name))
}

fn pick_f32_config(
    device: &cpal::Device,
    device_name: &str,
) -> Result<cpal::SupportedStreamConfig, EngineError> {
    let ranges: Vec<SupportedStreamConfigRange> = device
        .supported_output_configs()
        .map_err(|err| EngineError::Cpal {
            context: "supported_output_configs",
            message: err.to_string(),
        })?
        .filter(|range| range.sample_format() == SampleFormat::F32)
        .collect();

    if ranges.is_empty() {
        let default = device
            .default_output_config()
            .map_err(|err| EngineError::Cpal {
                context: "default_output_config",
                message: err.to_string(),
            })?;
        if default.sample_format() == SampleFormat::F32 {
            return Ok(default);
        }
        return Err(EngineError::NoF32Config {
            device: device_name.to_owned(),
        });
    }

    for range in ranges.iter().copied() {
        if let Some(cfg) = range.try_with_sample_rate(SAMPLE_RATE_48K) {
            return Ok(cfg);
        }
    }

    if let Ok(default) = device.default_output_config() {
        if default.sample_format() == SampleFormat::F32 {
            return Ok(default);
        }
    }

    Ok(ranges[0].with_max_sample_rate())
}
