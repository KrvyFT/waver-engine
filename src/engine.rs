//! Audio-thread graph walker executing a compiled patch schedule.

use std::collections::HashMap;
use std::sync::Arc;

use waver_core::{
    CompiledPatch, NodeId, NodeKind, ParamId, PortId, PortRef, RtCommand,
};
use waver_dsp::{Delay, Output, Process, ProcessCtx, Silence, Vco};

/// Internal block size in frames. Independent of the device callback length.
pub const BLOCK: usize = 64;

const MAX_INPUTS: usize = 4;

/// Live processor instances keyed by node id.
enum ProcessorSlot {
    Vco(Vco),
    Output(Output),
    Delay(Delay),
    Silence(Silence),
}

impl ProcessorSlot {
    fn process(&mut self, ctx: &mut ProcessCtx<'_>) {
        match self {
            Self::Vco(node) => node.process(ctx),
            Self::Output(node) => node.process(ctx),
            Self::Delay(node) => node.process(ctx),
            Self::Silence(node) => node.process(ctx),
        }
    }

    fn master(&self) -> Option<&[f32]> {
        match self {
            Self::Output(node) => Some(node.master_slice()),
            _ => None,
        }
    }
}

fn build_processor(
    kind: NodeKind,
    node: NodeId,
    params: &waver_core::ParamRegistry,
) -> ProcessorSlot {
    match kind {
        NodeKind::Vco => {
            let freq = params
                .get(node, ParamId::new(0))
                .unwrap_or_else(|| Arc::new(waver_core::ParamCell::new(440.0)));
            let amp = params
                .get(node, ParamId::new(1))
                .unwrap_or_else(|| Arc::new(waver_core::ParamCell::new(0.5)));
            let wave = params
                .get(node, ParamId::new(2))
                .unwrap_or_else(|| Arc::new(waver_core::ParamCell::new(0.0)));
            ProcessorSlot::Vco(Vco::with_params(freq, amp, wave))
        }
        NodeKind::Output => ProcessorSlot::Output(Output::new()),
        NodeKind::Delay => ProcessorSlot::Delay(Delay::new()),
        NodeKind::Silence => ProcessorSlot::Silence(Silence),
        _ => ProcessorSlot::Silence(Silence),
    }
}

/// Pre-allocated mono port buffers for one output jack.
struct PortBuffer {
    data: [f32; BLOCK],
}

impl PortBuffer {
    fn zero(&mut self, frames: usize) {
        let n = frames.min(BLOCK);
        self.data[..n].fill(0.0);
    }

    fn slice(&self, frames: usize) -> &[f32] {
        &self.data[..frames.min(BLOCK)]
    }
}

/// State owned by the cpal callback. Must not be wrapped in a `Mutex`.
pub struct Engine {
    sample_rate: f32,
    channels: usize,
    patch: Arc<CompiledPatch>,
    order: Vec<NodeId>,
    processors: HashMap<NodeId, ProcessorSlot>,
    output_bufs: HashMap<(NodeId, PortId), PortBuffer>,
    scratch_in: [[f32; BLOCK]; MAX_INPUTS],
}

impl Engine {
    /// Build an engine for a negotiated stream format.
    pub fn new(sample_rate: f32, channels: usize) -> Self {
        Self {
            sample_rate,
            channels: channels.max(1),
            patch: Arc::new(CompiledPatch::empty()),
            order: Vec::new(),
            processors: HashMap::new(),
            output_bufs: HashMap::new(),
            scratch_in: [[0.0; BLOCK]; MAX_INPUTS],
        }
    }

    /// Apply a command popped from the SPSC queue. Called at block boundaries.
    pub fn apply_rt(&mut self, cmd: RtCommand) {
        match cmd {
            RtCommand::SwapSchedule(patch) => self.rebuild(patch),
            RtCommand::AllNotesOff => {}
        }
    }

    fn rebuild(&mut self, patch: Arc<CompiledPatch>) {
        self.order = patch.schedule.order().to_vec();
        self.processors.clear();
        self.output_bufs.clear();

        for &node_id in &self.order {
            let kind = patch
                .schedule
                .kind_of(node_id)
                .unwrap_or(NodeKind::Silence);
            let slot = build_processor(kind, node_id, &patch.params);
            self.processors.insert(node_id, slot);

            let counts = kind.port_counts();
            for raw in 0..counts.outputs {
                let port = PortId::new(raw);
                self.output_bufs
                    .insert((node_id, port), PortBuffer { data: [0.0; BLOCK] });
            }
        }

        self.patch = patch;
    }

    /// Render one block into an interleaved device buffer.
    pub fn process_block(&mut self, interleaved: &mut [f32]) {
        let frames = interleaved.len() / self.channels;
        if frames == 0 {
            return;
        }

        for buf in self.output_bufs.values_mut() {
            buf.zero(frames);
        }

        let order: Vec<_> = self.order.clone();
        for node_id in order {
            self.process_node(node_id, frames);
        }

        let master = self.last_output_master();
        if let Some(mono) = master {
            for (i, sample) in interleaved.iter_mut().enumerate() {
                *sample = mono[i / self.channels];
            }
        } else {
            interleaved.fill(0.0);
        }
    }

    fn process_node(&mut self, node_id: NodeId, frames: usize) {
        let kind = self
            .patch
            .schedule
            .kind_of(node_id)
            .unwrap_or(NodeKind::Silence);
        let input_count = kind.port_counts().inputs as usize;
        let output_count = kind.port_counts().outputs as usize;

        for port_raw in 0..input_count {
            let port = PortId::new(port_raw as u32);
            let to = PortRef { node: node_id, port };
            self.scratch_in[port_raw].fill(0.0);
            for source in self.patch.schedule.sources_to(to) {
                if let Some(buf) = self.output_bufs.get(&(source.node, source.port)) {
                    let src = buf.slice(frames);
                    for (dst, &val) in self.scratch_in[port_raw][..frames].iter_mut().zip(src) {
                        *dst += val;
                    }
                }
            }
        }

        let input_refs: Vec<&[f32]> = (0..input_count)
            .map(|p| &self.scratch_in[p][..frames] as &[f32])
            .collect();

        let mut local_out = [[0.0f32; BLOCK]; MAX_INPUTS];

        if let Some(slot) = self.processors.get_mut(&node_id) {
            let mut outs0 = [&mut local_out[0][..frames]];
            let outputs: &mut [&mut [f32]] = if output_count > 0 {
                &mut outs0
            } else {
                &mut []
            };
            let mut ctx = ProcessCtx {
                sample_rate: self.sample_rate,
                block: frames,
                inputs: &input_refs,
                outputs,
            };
            slot.process(&mut ctx);
        }

        if output_count > 0 {
            let port = PortId::new(0);
            if let Some(buf) = self.output_bufs.get_mut(&(node_id, port)) {
                buf.data[..frames].copy_from_slice(&local_out[0][..frames]);
            }
        }
    }

    fn last_output_master(&self) -> Option<&[f32]> {
        for &node_id in self.order.iter().rev() {
            if self.patch.schedule.kind_of(node_id) == Some(NodeKind::Output) {
                if let Some(slot) = self.processors.get(&node_id) {
                    return slot.master();
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use waver_core::{Graph, NodeKind, PortId, PortRef};

    use super::Engine;

    fn port(node: waver_core::NodeId, index: u32) -> PortRef {
        PortRef {
            node,
            port: PortId::new(index),
        }
    }

    #[test]
    fn process_block_writes_silence_when_empty() {
        let mut engine = Engine::new(48_000.0, 2);
        let mut buf = [1.0f32; 128];
        engine.process_block(&mut buf);
        assert!(buf.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn vco_to_output_produces_signal() {
        let mut graph = Graph::new();
        let vco = graph.insert(NodeKind::Vco);
        let out = graph.insert(NodeKind::Output);
        graph.connect(port(vco, 0), port(out, 0));
        let patch = graph.compile_patch(None).expect("compile");

        let mut engine = Engine::new(48_000.0, 1);
        engine.apply_rt(waver_core::RtCommand::SwapSchedule(Arc::new(patch)));

        let mut buf = [0.0f32; 64];
        engine.process_block(&mut buf);
        let rms = (buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32).sqrt();
        assert!(rms > 0.01, "expected non-silent output, rms={rms}");
    }
}
