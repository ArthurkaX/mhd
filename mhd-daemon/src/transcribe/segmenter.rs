//! RMS silence segmenter.
//!
//! Takes a stream of `AudioChunk` (50 ms each, 16 kHz mono f32) and
//! emits phrase segments delimited by silence runs.
//!
//! ## Algorithm
//!
//! 1. Compute RMS for each chunk.
//! 2. If RMS < `speech_rms_threshold` → silence, else → speech.
//! 3. When silence runs longer than `silence_ms`, close the current phrase.
//! 4. Phrases shorter than `min_chunk_ms` are extended.
//! 5. Phrases longer than `max_chunk_ms` are force-flushed.

use crate::transcribe::audio::AudioChunk;

/// Status returned by the segmenter to the pipeline.
#[derive(Debug)]
pub enum SegmentEvent {
    /// A complete phrase is ready for transcription.
    Phrase(Vec<f32>),
    /// The stream ended (caller should not expect more segments).
    Flush,
}

/// RMS silence segmenter.
pub struct RmsSegmenter {
    /// RMS threshold (squared amplitude).
    threshold: f32,
    /// Silence duration to trigger phrase end (ms).
    silence_ms: u64,
    /// Minimum phrase duration (ms).
    min_chunk_ms: u64,
    /// Maximum phrase duration (ms) — force flush.
    max_chunk_ms: u64,
    /// Accumulated audio for the current phrase.
    phrase: Vec<f32>,
    /// Number of consecutive silent chunks.
    silent_count: u32,
    /// Expected chunk duration (ms).
    chunk_duration_ms: u64,
    /// Accumulated duration of current phrase (ms).
    phrase_duration_ms: u64,
    /// First timestamp of current phrase.
    phrase_start_ms: u64,
}

impl RmsSegmenter {
    pub fn new(
        threshold: f32,
        silence_ms: u64,
        min_chunk_ms: u64,
        max_chunk_ms: u64,
        chunk_duration_ms: u64,
    ) -> Self {
        RmsSegmenter {
            threshold,
            silence_ms,
            min_chunk_ms,
            max_chunk_ms,
            phrase: Vec::new(),
            silent_count: 0,
            chunk_duration_ms,
            phrase_duration_ms: 0,
            phrase_start_ms: 0,
        }
    }

    /// Feed one audio chunk. Returns zero or one `SegmentEvent`.
    pub fn feed(&mut self, chunk: &AudioChunk) -> Option<SegmentEvent> {
        let rms = compute_rms(&chunk.samples);
        let is_speech = rms >= self.threshold;

        if self.phrase.is_empty() && !is_speech {
            // Discard leading silence
            return None;
        }

        // Start a new phrase if we were silent
        if self.phrase.is_empty() {
            self.phrase_start_ms = chunk.started_at_ms;
        }

        if is_speech {
            // Reset silence counter
            self.silent_count = 0;
            self.phrase.extend_from_slice(&chunk.samples);
            self.phrase_duration_ms += self.chunk_duration_ms;

            // Force-flush if phrase exceeds max duration
            if self.phrase_duration_ms >= self.max_chunk_ms {
                return self.flush_phrase();
            }
        } else {
            // Silence — count consecutive silent chunks
            self.silent_count += 1;
            self.phrase.extend_from_slice(&chunk.samples);
            self.phrase_duration_ms += self.chunk_duration_ms;

            let silence_duration = self.silent_count as u64 * self.chunk_duration_ms;
            if silence_duration >= self.silence_ms
                && self.phrase_duration_ms >= self.min_chunk_ms
            {
                return self.flush_phrase();
            }
        }

        None
    }

    /// Called when the audio stream ends. Returns remaining audio as a phrase.
    pub fn finish(&mut self) -> Option<SegmentEvent> {
        if self.phrase.is_empty() {
            return Some(SegmentEvent::Flush);
        }
        let phrase = std::mem::take(&mut self.phrase);
        self.phrase_duration_ms = 0;
        self.silent_count = 0;
        Some(SegmentEvent::Phrase(phrase))
    }

    fn flush_phrase(&mut self) -> Option<SegmentEvent> {
        let phrase = std::mem::take(&mut self.phrase);
        self.phrase_duration_ms = 0;
        self.silent_count = 0;
        Some(SegmentEvent::Phrase(phrase))
    }
}

/// Compute RMS (root mean square) of f32 samples.
fn compute_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(samples: Vec<f32>, started_at_ms: u64) -> AudioChunk {
        AudioChunk {
            sequence_id: 0,
            sample_rate: 16000,
            samples,
            started_at_ms,
            duration_ms: 50,
        }
    }

    #[test]
    fn test_leading_silence_discarded() {
        let mut seg = RmsSegmenter::new(0.1, 700, 500, 15000, 50);
        // 2 chunks of silence (RMS = 0)
        assert!(seg.feed(&chunk(vec![0.0; 800], 0)).is_none());
        assert!(seg.feed(&chunk(vec![0.0; 800], 50)).is_none());
        // Speech starts
        let speech = chunk(vec![0.5; 800], 100);
        assert!(seg.feed(&speech).is_none()); // collected, not yet full
    }

    #[test]
    fn test_silence_triggers_phrase() {
        let mut seg = RmsSegmenter::new(0.1, 700, 500, 15000, 50);
        // 10 chunks of speech (500ms) — meets min_chunk
        let speech = vec![0.5; 800];
        for i in 0..10 {
            seg.feed(&chunk(speech.clone(), i * 50));
        }
        // 14 more silent chunks (700ms) — should trigger phrase
        for i in 10..24 {
            let ev = seg.feed(&chunk(vec![0.0; 800], i * 50));
            if i == 23 {
                assert!(ev.is_some(), "should flush at 14th silent chunk");
            }
        }
    }

    #[test]
    fn test_flush_on_max_duration() {
        let mut seg = RmsSegmenter::new(0.001, 700, 500, 200, 50);
        // Feed speech chunks: at 4th chunk (200ms) should force-flush (200 >= 200)
        let speech = vec![0.5; 800];
        let mut ev = None;
        for i in 0..5 {
            ev = seg.feed(&chunk(speech.clone(), i * 50));
            if ev.is_some() {
                break;
            }
        }
        assert!(ev.is_some(), "should flush at 4th chunk (200ms >= max 200ms)");
    }

    #[test]
    fn test_finish_returns_remaining() {
        let mut seg = RmsSegmenter::new(0.1, 700, 500, 15000, 50);
        let speech = vec![0.5; 800];
        for i in 0..5 {
            seg.feed(&chunk(speech.clone(), i * 50));
        }
        let ev = seg.finish();
        assert!(matches!(ev, Some(SegmentEvent::Phrase(_))));
    }

    #[test]
    fn test_finish_empty_returns_flush() {
        let mut seg = RmsSegmenter::new(0.1, 700, 500, 15000, 50);
        let ev = seg.finish();
        assert!(matches!(ev, Some(SegmentEvent::Flush)));
    }
}
