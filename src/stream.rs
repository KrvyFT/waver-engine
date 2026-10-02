//! Open the default output device and run [`crate::Engine`] in the cpal callback.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};
use waver_core::{AudioSelection, EngineStatus, RtCommand};

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
    /// The device's default format is not PCM audio supported by the renderer.
    #[error("output device '{device}' uses unsupported sample format '{format}'")]
    UnsupportedSampleFormat {
        /// Device name from the host.
        device: String,
        /// Format reported by the device.
        format: SampleFormat,
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

/// Create a command queue and start output in the device's default PCM format.
/// On Windows, CPAL uses WASAPI and the system mix format; no ASIO SDK is needed.
///
/// Never panics on missing hardware: [`AudioRuntime::error`] is set instead.
pub fn spawn_output() -> AudioRuntime {
    spawn_output_for(&crate::default_selection())
}

pub fn spawn_output_for(selection: &AudioSelection) -> AudioRuntime {
    let status = Arc::new(EngineStatus::new());
    status.set_format(0, BLOCK as u32, 0);
    let (producer, consumer) = RingBuffer::<RtCommand>::new(COMMAND_CAPACITY);

    match try_open(consumer, Arc::clone(&status), selection) {
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
    consumer: Consumer<RtCommand>,
    status: Arc<EngineStatus>,
    selection: &AudioSelection,
) -> Result<(Stream, String), EngineError> {
    let host = crate::devices::selected_host(selection).map_err(|message| EngineError::Cpal {
        context: "select_host",
        message,
    })?;
    let device = if let Some(id) = &selection.device {
        let id = id.parse().map_err(|e| EngineError::Cpal {
            context: "device_id",
            message: format!("{e}"),
        })?;
        host.device_by_id(&id)
    } else {
        host.default_output_device()
    }
    .ok_or(EngineError::NoOutputDevice)?;
    let device_name = device
        .description()
        .map(|desc| desc.name().to_owned())
        .unwrap_or_else(|_| device.to_string());
    // WASAPI's default config is the Windows mix format. Use its rate and
    // channel count instead of forcing 48 kHz or requiring a float device.
    let supported = device
        .default_output_config()
        .map_err(|err| EngineError::Cpal {
            context: "default_output_config",
            message: err.to_string(),
        })?;
    let format = supported.sample_format();
    let sample_rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    let config: StreamConfig = supported.into();

    status.set_format(sample_rate, BLOCK as u32, channels as u32);

    let stream = match format {
        SampleFormat::I8 => build_stream::<i8>(&device, config, consumer, status),
        SampleFormat::I16 => build_stream::<i16>(&device, config, consumer, status),
        SampleFormat::I24 => build_stream::<cpal::I24>(&device, config, consumer, status),
        SampleFormat::I32 => build_stream::<i32>(&device, config, consumer, status),
        SampleFormat::I64 => build_stream::<i64>(&device, config, consumer, status),
        SampleFormat::U8 => build_stream::<u8>(&device, config, consumer, status),
        SampleFormat::U16 => build_stream::<u16>(&device, config, consumer, status),
        SampleFormat::U24 => build_stream::<cpal::U24>(&device, config, consumer, status),
        SampleFormat::U32 => build_stream::<u32>(&device, config, consumer, status),
        SampleFormat::U64 => build_stream::<u64>(&device, config, consumer, status),
        SampleFormat::F32 => build_stream::<f32>(&device, config, consumer, status),
        SampleFormat::F64 => build_stream::<f64>(&device, config, consumer, status),
        _ => {
            return Err(EngineError::UnsupportedSampleFormat {
                device: device_name,
                format,
            });
        }
    }?;
    Ok((stream, device_name))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut consumer: Consumer<RtCommand>,
    status: Arc<EngineStatus>,
) -> Result<Stream, EngineError>
where
    T: SizedSample + FromSample<f32>,
{
    let mut renderer = OutputRenderer::new(config.sample_rate as f32, config.channels as usize);
    let err_status = Arc::clone(&status);

    device
        .build_output_stream(
            config,
            move |data: &mut [T], _info| {
                while let Ok(cmd) = consumer.pop() {
                    renderer.engine.apply_rt(cmd);
                }
                renderer.render(data);
            },
            move |_err| {
                err_status.bump_xrun();
            },
            None,
        )
        .map_err(|err| EngineError::Cpal {
            context: "build_output_stream",
            message: err.to_string(),
        })
}

/// The DSP stays in f32. Only the device boundary converts samples, using a
/// buffer allocated before the stream starts, independent of callback length.
struct OutputRenderer {
    engine: Engine,
    channels: usize,
    scratch: Vec<f32>,
}

impl OutputRenderer {
    fn new(sample_rate: f32, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            engine: Engine::new(sample_rate, channels),
            channels,
            scratch: vec![0.0; BLOCK * channels],
        }
    }

    fn render<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T]) {
        let complete_samples = data.len() / self.channels * self.channels;
        for chunk in data[..complete_samples].chunks_mut(self.scratch.len()) {
            let samples = &mut self.scratch[..chunk.len()];
            self.engine.process_block(samples);
            for (destination, &sample) in chunk.iter_mut().zip(samples.iter()) {
                *destination = device_sample(sample);
            }
        }
        // CPAL supplies complete frames; keep malformed trailing samples silent.
        data[complete_samples..].fill(T::EQUILIBRIUM);
    }
}

fn device_sample<T: SizedSample + FromSample<f32>>(sample: f32) -> T {
    // Fan-in can exceed full scale. Saturate before integer conversion rather
    // than wrapping, and prevent invalid DSP values from reaching the device.
    // dasp's 24-bit wrappers do not saturate +1.0 and would produce an
    // out-of-range value. Their largest positive PCM sample is below +1.0.
    let max = match T::FORMAT {
        SampleFormat::I24 | SampleFormat::U24 => 1.0 - 1.0 / 8_388_608.0,
        _ => 1.0,
    };
    T::from_sample(if sample.is_finite() {
        sample.clamp(-1.0, max)
    } else {
        0.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::Sample;
    use waver_core::{Graph, NodeKind, ParamId, PortId, PortRef};

    fn tone(channels: usize) -> OutputRenderer {
        let mut graph = Graph::new();
        let vco = graph.insert(NodeKind::Vco);
        let output = graph.insert(NodeKind::Output);
        graph.connect(
            PortRef {
                node: vco,
                port: PortId::new(0),
            },
            PortRef {
                node: output,
                port: PortId::new(0),
            },
        );
        let patch = graph.compile_patch(None).unwrap();
        patch.params.get(vco, ParamId::new(1)).unwrap().set(0.25);
        let mut renderer = OutputRenderer::new(44_100.0, channels);
        renderer
            .engine
            .apply_rt(RtCommand::SwapSchedule(Arc::new(patch)));
        renderer
    }

    #[test]
    fn pcm_conversion_handles_silence_full_scale_and_invalid_values() {
        assert_eq!(device_sample::<i16>(0.0), 0);
        assert_eq!(device_sample::<u16>(0.0), 32_768);
        assert_eq!(device_sample::<u8>(0.0), 128);
        assert_eq!(device_sample::<i16>(-2.0), i16::MIN);
        assert_eq!(device_sample::<i16>(2.0), i16::MAX);
        assert_eq!(device_sample::<i32>(-2.0), i32::MIN);
        assert_eq!(device_sample::<i32>(2.0), i32::MAX);
        assert_eq!(device_sample::<u16>(-2.0), 0);
        assert_eq!(device_sample::<u16>(2.0), u16::MAX);
        assert_eq!(device_sample::<cpal::I24>(2.0).inner(), 8_388_607);
        assert_eq!(device_sample::<cpal::I24>(-2.0).inner(), -8_388_608);
        assert_eq!(device_sample::<cpal::U24>(2.0).inner(), 16_777_215);
        assert_eq!(device_sample::<cpal::U24>(-2.0).inner(), 0);
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(device_sample::<i16>(sample), 0);
            assert_eq!(device_sample::<u16>(sample), 32_768);
            assert_eq!(device_sample::<f32>(sample), 0.0);
        }
        assert_eq!(device_sample::<f64>(0.25), 0.25);
    }

    #[test]
    fn integer_callbacks_match_float_audio_across_blocks_and_short_tail() {
        let mut float = tone(2);
        let mut integer = tone(2);
        // Exercise several full blocks, a short final block and phase continuity.
        for frames in [0, 1, 64, 145, 17] {
            let mut expected = vec![0.0f32; frames * 2];
            let mut actual = vec![0i16; frames * 2];
            float.render(&mut expected);
            integer.render(&mut actual);
            for (frame, expected_frame) in actual.chunks_exact(2).zip(expected.chunks_exact(2)) {
                assert_eq!(frame[0], frame[1]);
                assert_eq!(frame[0], i16::from_sample(expected_frame[0]));
            }
            if frames >= 64 {
                assert!(actual.iter().any(|&sample| sample != 0));
            }
        }
    }

    #[test]
    fn unsigned_empty_patch_and_partial_frame_are_silent() {
        let mut renderer = OutputRenderer::new(48_000.0, 2);
        let mut data = [0u16; 259];
        renderer.render(&mut data);
        assert!(data.iter().all(|&sample| sample == 32_768));
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires an available Windows audio output device"]
    fn windows_explicit_device_opens_while_previous_stream_is_running() {
        let previous = spawn_output();
        assert!(previous.error.is_none(), "{:?}", previous.error);
        let catalog = crate::audio_catalog();
        let device = catalog
            .devices
            .iter()
            .find(|d| d.backend == crate::default_selection().backend)
            .expect("output device");
        let next = spawn_output_for(&AudioSelection {
            backend: device.backend.clone(),
            device: Some(device.id.clone()),
        });
        assert!(next.error.is_none(), "{:?}", next.error);
        assert!(previous.status.running() && next.status.running());
        eprintln!("Explicit output: {}", next.device_name);
    }

    #[test]
    fn unknown_backend_fails_without_starting_a_stream() {
        let runtime = spawn_output_for(&AudioSelection {
            backend: "missing-backend".into(),
            device: None,
        });
        assert!(runtime.error.is_some());
        assert!(!runtime.status.running());
    }

    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires an available Windows audio output device"]
    fn windows_default_device_opens_and_runs_silently() {
        assert_eq!(cpal::default_host().id(), cpal::HostId::Wasapi);
        let runtime = spawn_output();
        assert!(runtime.error.is_none(), "{:?}", runtime.error);
        assert!(runtime.status.running());
        assert!(runtime.status.sample_rate() > 0);
        assert!(runtime.status.channels() > 0);
        eprintln!(
            "Windows output: {} ({} Hz, {} channels)",
            runtime.device_name,
            runtime.status.sample_rate(),
            runtime.status.channels()
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
        assert_eq!(runtime.status.xruns(), 0);
    }
}
