use super::compression;
use crate::{input::FrameInput, Frame};
use parking_lot::RwLock;
use std::collections::VecDeque;
use std::sync::Arc;
use tracing::info;

#[derive(Default)]
struct InputEncoderRef<T>
where
    T: Default + bytemuck::Pod,
{
    pending: VecDeque<FrameInput<T>>,

    last_acked: FrameInput<T>,
    last_encoded: FrameInput<T>,
}

/// A buffer of all inputs that have not been yet acknowledged by a connected remote peer.
///
/// This struct wraps an Arc, so it's safe to make clones and pass it around.
#[derive(Clone, Default)]
pub(super) struct InputEncoder<T>(Arc<RwLock<InputEncoderRef<T>>>)
where
    T: Default + bytemuck::Pod;

impl<T: Default + bytemuck::Pod> InputEncoder<T> {
    /// Adds an input to as the latest element in the queue.
    pub fn push(&self, input: FrameInput<T>) {
        self.0.write().pending.push_front(input);
    }

    /// Gets the frame of the last input that was encoded via `[encode]`.
    pub fn last_encoded_frame(&self) -> Frame {
        self.0.read().last_encoded.frame
    }
}

impl<T: Default + bytemuck::Pod + Clone> InputEncoder<T> {
    /// Acknowledges a given frame. All inputs with of a prior frame will be dropped.
    ///
    /// This will update the reference input that is used to delta-encode.
    pub fn acknowledge_frame(&self, ack_frame: Frame) {
        let mut queue = self.0.write();
        // Get rid of our buffered input
        let last = queue.pending.iter().filter(|i| i.frame < ack_frame).last();
        if let Some(last) = last {
            queue.last_acked = last.clone();
            queue.pending.retain(|i| i.frame >= ack_frame);
        }
    }

    /// Encodes all pending output as a byte buffer.
    ///
    /// To minimize the size of the produced buffer, the sequence of is delta
    /// encoded by `[compression::encode]` relative to the last acknowledged
    /// input, which is updated via `[acknowledge_frame]`.
    ///
    /// This will not remove any of the inputs in the queue, but will update
    /// the value returned by `[last_encoded_frame]` to reflect the highest
    /// frame that has been encoded.
    pub fn encode(&self) -> (Frame, Vec<u8>) {
        let mut queue = self.0.write();
        let pending = &queue.pending;
        if !pending.is_empty() {
            let start_frame = pending.back().unwrap().frame;
            let bits =
                compression::encode(&queue.last_acked.input, pending.iter().map(|f| &f.input));
            queue.last_encoded = queue.pending.front().unwrap().clone();
            (start_frame, bits)
        } else {
            (0, Vec::new())
        }
    }
}

#[derive(Default)]
struct InputDecoderRef<T>
where
    T: Default + bytemuck::Pod,
{
    last_decoded: FrameInput<T>,
}

/// A stateful decoder that decodes delta patches created by `[InputEncoder]`.
///
/// This struct wraps an Arc, so it's safe to make clones and pass it around.
#[derive(Default, Clone)]
pub(super) struct InputDecoder<T>(Arc<RwLock<InputDecoderRef<T>>>)
where
    T: Default + bytemuck::Pod;

impl<T: Default + bytemuck::Pod> InputDecoder<T> {
    /// Gets the frame of the most recently decoded input if available.
    ///
    /// If no input has been decoded yet, this will be the NULL_FRAME.
    pub fn last_decoded_frame(&self) -> Frame {
        self.0.read().last_decoded.frame
    }
}

impl<T: Default + bytemuck::Pod + Clone> InputDecoder<T> {
    /// Resets the internal state of the decoder to it's default.
    pub fn reset(&self) {
        self.0.write().last_decoded = Default::default();
    }

    pub fn decode(
        &self,
        start_frame: Frame,
        bits: impl AsRef<[u8]>,
    ) -> Result<Vec<FrameInput<T>>, compression::DecodeError> {
        let mut decoder = self.0.write();
        let last_decoded_frame = decoder.last_decoded.frame;
        let current_frame = if crate::is_null(decoder.last_decoded.frame) {
            start_frame - 1
        } else {
            last_decoded_frame
        };
        let frame_inputs = compression::decode(&decoder.last_decoded.input, bits)?
            .into_iter()
            .enumerate()
            .map(|(i, input)| FrameInput::<T> {
                frame: start_frame + i as i32,
                input,
            })
            .filter(|input| input.frame > current_frame)
            .collect::<Vec<_>>();

        if let Some(latest) = frame_inputs.last() {
            decoder.last_decoded = latest.clone();
        }

        debug_assert!(decoder.last_decoded.frame >= last_decoded_frame);

        Ok(frame_inputs)
    }
}
