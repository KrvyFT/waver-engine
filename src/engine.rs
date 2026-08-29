//! Audio-thread graph walker. The skeleton ignores schedule order and renders silence.

use std::sync::Arc;

use waver_core::{RtCommand, Schedule};
use waver_dsp::{Process, ProcessCtx, Silence};

/// Internal block size in frames. Independent of the device callback length.
pub const BLOCK: usize = 64;

/// State owned by the cpal callback. Must not be wrapped in a `Mutex`.
pub struct Engine {
    sample_rate: f32,
    channels: usize,
    schedule: Arc<Schedule>,
    silence: Silence,
}

impl Engine {
    /// Build an engine for a negotiated stream format.
    pub fn new(sample_rate: f32, channels: usize) -> Self {
        Self {
            sample_rate,
            channels: channels.max(1),
            schedule: Arc::new(Schedule::empty()),
            silence: Silence,
        }
    }

    /// Apply a command popped from the SPSC queue. Called at block boundaries.
    pub fn apply_rt(&mut self, cmd: RtCommand) {
        match cmd {
            RtCommand::SwapSchedule(schedule) => self.schedule = schedule,
            RtCommand::AllNotesOff => {}
        }
    }

    /// Fill an interleaved `f32` slice with silence.
    ///
    /// `interleaved.len()` may be a partial last block. Schedule execution is
    /// not implemented; [`Silence`] is the only processor.
    ///
    /// Dropping the previous `Arc<Schedule>` on this thread is accepted in the
    /// skeleton (empty vec). Later builds should hand it to basedrop.
    pub fn process_block(&mut self, interleaved: &mut [f32]) {
        let _ = self.schedule.order();
        let frames = interleaved.len() / self.channels;
        let mut outputs: [&mut [f32]; 1] = [interleaved];
        let mut ctx = ProcessCtx {
            sample_rate: self.sample_rate,
            block: frames,
            inputs: &[],
            outputs: &mut outputs,
        };
        self.silence.process(&mut ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::Engine;

    #[test]
    fn process_block_writes_silence() {
        let mut engine = Engine::new(48_000.0, 2);
        let mut buf = [1.0f32; 128];
        engine.process_block(&mut buf);
        assert!(buf.iter().all(|sample| *sample == 0.0));
    }
}
