use hypr_onnx::ndarray::{ArrayBase, Axis, IxDyn, ViewRepr};
use hypr_onnx::ort::{self, session::Session, value::TensorRef};

use crate::segmentation::Segment;

const SEGMENTATION_ONNX: &[u8] = include_bytes!("./data/segmentation.onnx");
const FRAME_SIZE: usize = 270;
const FRAME_START: usize = 721;

/// Incremental voice activity detector.
///
/// Wraps the same ONNX model as [`Segmenter`](crate::segmentation::Segmenter)
/// but maintains internal state across `feed` calls.
pub struct IncrementalVad {
    session: Session,
    sample_rate: usize,
    window_size: usize,
    all_samples: Vec<i16>,
    /// Absolute sample index of `all_samples[0]` after front-pruning.
    origin: usize,
    buffer: Vec<i16>,
    offset: usize,
    is_speaking: bool,
    speech_start: usize,
}

impl IncrementalVad {
    pub fn new(sample_rate: u32) -> Result<Self, crate::Error> {
        let session = hypr_onnx::load_model_from_bytes(SEGMENTATION_ONNX)?;
        let sample_rate = sample_rate as usize;
        Ok(Self {
            session,
            sample_rate,
            window_size: sample_rate * 10,
            all_samples: Vec::new(),
            origin: 0,
            buffer: Vec::new(),
            offset: FRAME_START,
            is_speaking: false,
            speech_start: 0,
        })
    }

    pub fn feed(&mut self, samples: &[i16]) -> Result<Vec<Segment>, crate::Error> {
        self.all_samples.extend_from_slice(samples);
        self.buffer.extend_from_slice(samples);

        let mut segments = Vec::new();

        while self.buffer.len() >= self.window_size {
            self.process_window(&mut segments)?;
        }

        Ok(segments)
    }

    /// Run inference over the first full window in `buffer`, update speaking
    /// state, emit any segments whose trailing silence was observed, then
    /// drain the consumed window.
    fn process_window(&mut self, out: &mut Vec<Segment>) -> Result<(), crate::Error> {
        let window = &self.buffer[..self.window_size];

        // Phase 1: run inference, collect speech flags
        let speech_flags = {
            let array = hypr_onnx::ndarray::Array1::from_iter(window.iter().map(|&x| x as f32))
                .insert_axis(Axis(0))
                .insert_axis(Axis(1))
                .into_dyn();

            let inputs = ort::inputs![TensorRef::from_array_view(array.view())?];
            let run_output = self.session.run(inputs)?;
            let output_tensor = run_output.values().next().unwrap();
            let outputs = output_tensor.try_extract_array::<f32>()?;

            let mut flags = Vec::new();
            for row in outputs.outer_iter() {
                for sub_row in row.axis_iter(Axis(0)) {
                    let max_index = find_max_index(sub_row)?;
                    flags.push(max_index != 0);
                }
            }
            flags
        }; // `run_output` dropped here → mutable borrow released

        // Phase 2: update state (no outstanding borrow on self.session)
        for is_speech in &speech_flags {
            if *is_speech {
                if !self.is_speaking {
                    self.speech_start = self.offset;
                    self.is_speaking = true;
                }
            } else if self.is_speaking {
                self.finish_segment(out);
            }
            self.offset += FRAME_SIZE;
        }

        self.buffer.drain(..self.window_size);
        Ok(())
    }

    pub fn finish(&mut self) -> Vec<Segment> {
        let mut segments = Vec::new();
        // The trailing buffer is shorter than a full window. `feed` only ever
        // processes complete windows, so speech in the tail would otherwise be
        // dropped. Pad with silence to a full window and run one last pass so
        // tail speech produces a segment (mirrors `Segmenter::pad_samples`).
        if !self.buffer.is_empty() && self.buffer.len() < self.window_size {
            self.buffer
                .extend(vec![0i16; self.window_size - self.buffer.len()]);
            if let Err(e) = self.process_window(&mut segments) {
                eprintln!("[incremental_vad] trailing window inference failed: {e}");
            }
        }
        if self.is_speaking {
            self.finish_segment(&mut segments);
        }
        self.buffer.clear();
        segments
    }

    /// Total number of samples fed and still retained since construction
    /// (absolute index of the last retained sample + 1). Indices passed to
    /// [`Self::sample_slice`] / [`Self::prune_before`] are absolute relative
    /// to the recording origin, not to the retained buffer.
    pub fn all_samples_len(&self) -> usize {
        self.origin + self.all_samples.len()
    }

    /// Slice the retained global sample buffer by absolute sample indices,
    /// clamped to the available range.
    pub fn sample_slice(&self, start_sample: usize, end_sample: usize) -> Vec<i16> {
        let lo = start_sample
            .saturating_sub(self.origin)
            .min(self.all_samples.len());
        let hi = end_sample
            .saturating_sub(self.origin)
            .min(self.all_samples.len())
            .max(lo);
        if lo < hi {
            self.all_samples[lo..hi].to_vec()
        } else {
            Vec::new()
        }
    }

    /// Drop retained samples before `sample_idx` (absolute). Callers must only
    /// prune samples that no completed/future VAD segment nor `sample_slice`
    /// request will reference again (e.g. after a group has been emitted).
    pub fn prune_before(&mut self, sample_idx: usize) {
        if sample_idx <= self.origin {
            return;
        }
        let drop = sample_idx
            .saturating_sub(self.origin)
            .min(self.all_samples.len());
        if drop == 0 {
            return;
        }
        self.all_samples.drain(..drop);
        self.origin += drop;
    }

    fn finish_segment(&mut self, out: &mut Vec<Segment>) {
        let sr = self.sample_rate as f64;
        let start_s = self.speech_start as f64 / sr;
        let end_s = self.offset as f64 / sr;

        let lo = ((start_s * sr) as usize)
            .saturating_sub(self.origin)
            .min(self.all_samples.len());
        let hi = ((end_s * sr) as usize)
            .saturating_sub(self.origin)
            .min(self.all_samples.len())
            .max(lo);

        out.push(Segment {
            start: start_s,
            end: end_s,
            samples: if lo < hi {
                self.all_samples[lo..hi].to_vec()
            } else {
                Vec::new()
            },
        });

        self.is_speaking = false;
    }
}

fn find_max_index(row: ArrayBase<ViewRepr<&f32>, IxDyn>) -> Result<usize, crate::Error> {
    row.iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .ok_or(crate::Error::EmptyRowError)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_bytes_to_i16_s16le(bytes: &[u8]) -> Vec<i16> {
        let data = if bytes.len() > 44 {
            &bytes[44..]
        } else {
            bytes
        };
        data.chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect()
    }

    #[test]
    fn finish_processes_trailing_partial_window() {
        // Use a 16kHz mono WAV slice (~29.1s), just under 3 full 10s windows,
        // so the tail audio sits in a partial buffer after the last window.
        let audio = wav_bytes_to_i16_s16le(hypr_data::english_1::AUDIO);
        let sr: u32 = 16000;
        let total = sr as usize * 29;
        assert!(audio.len() >= total, "test audio too short");
        let audio = &audio[..total];

        let mut vad = IncrementalVad::new(sr).unwrap();

        // Feed in small chunks; only full 10s windows get processed here.
        let mut emitted = Vec::new();
        for chunk in audio.chunks(1600) {
            emitted.extend(vad.feed(chunk).unwrap());
        }
        assert!(
            emitted.iter().all(|s| s.end <= 20.0),
            "feed must only complete segments within processed windows"
        );

        // Before the fix, finish() dropped the residual buffer and the tail
        // speech produced no segment. Now it must emit a segment whose end
        // reaches the end of the fed audio (speech present in the tail).
        let flushed = vad.finish();
        let audio_dur = audio.len() as f64 / sr as f64;
        let last_end = flushed.iter().map(|s| s.end).last().unwrap_or(0.0);
        assert!(
            last_end >= audio_dur - 1.5,
            "tail segment end {last_end:.2}s should cover audio end {audio_dur:.2}s; flushed={:?}",
            flushed.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn finish_with_empty_buffer_is_noop() {
        let mut vad = IncrementalVad::new(16000).unwrap();
        assert!(vad.finish().is_empty());
    }
}
