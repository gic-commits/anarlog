use crate::diarization::SpeakerSegment;

#[derive(Debug, Clone)]
pub struct DurationSchedulerConfig {
    pub max_duration_ms: u32,
    pub watermark_low: f64,   // 0.8
    pub watermark_high: f64,  // 1.2
}

impl Default for DurationSchedulerConfig {
    fn default() -> Self {
        Self {
            max_duration_ms: 30000,
            watermark_low: 0.8,
            watermark_high: 1.2,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubmitDecision {
    Wait,
    Submit,
    SplitAndSubmit(usize),
}

pub struct DurationScheduler {
    config: DurationSchedulerConfig,
    pending: Vec<SpeakerSegment>,
}

impl DurationScheduler {
    pub fn new(config: DurationSchedulerConfig) -> Self {
        Self {
            config,
            pending: Vec::new(),
        }
    }

    /// 添加一个 SpeakerSegment，返回是否需要提交
    pub fn add_segment(&mut self, seg: SpeakerSegment) -> SubmitDecision {
        self.pending.push(seg);
        self.evaluate()
    }

    /// 所有段已添加完，flush 剩余的段
    pub fn flush(&mut self) -> Vec<Vec<SpeakerSegment>> {
        let mut batches = Vec::new();
        if !self.pending.is_empty() {
            batches.push(self.pending.clone());
            self.pending.clear();
        }
        batches
    }

    /// 取出上次决策提交的段
    pub fn take_submitted(&mut self, split_at: Option<usize>) -> Vec<SpeakerSegment> {
        match split_at {
            Some(n) if n < self.pending.len() => {
                let remaining = self.pending.split_off(n);
                let submitted = std::mem::replace(&mut self.pending, remaining);
                submitted
            }
            _ => std::mem::take(&mut self.pending),
        }
    }

    fn total_duration_ms(&self) -> f64 {
        self.pending
            .iter()
            .map(|seg| (seg.end - seg.start) * 1000.0)
            .sum()
    }

    fn evaluate(&self) -> SubmitDecision {
        let total = self.total_duration_ms();
        let max = self.config.max_duration_ms as f64;
        let low = max * self.config.watermark_low;
        let high = max * self.config.watermark_high;

        if total > high {
            // 超过 120% — 需要切分
            // 找到切分点：累积到 >= max 的位置
            let mut accumulated = 0.0f64;
            for (i, seg) in self.pending.iter().enumerate() {
                accumulated += (seg.end - seg.start) * 1000.0;
                if accumulated >= max || i == self.pending.len() - 1 {
                    return SubmitDecision::SplitAndSubmit(i + 1);
                }
            }
            SubmitDecision::SplitAndSubmit(self.pending.len())
        } else if total >= low {
            // 在 80%-120% 区间内 → 提交
            SubmitDecision::Submit
        } else {
            // 小于 80% → 继续等
            SubmitDecision::Wait
        }
    }
}

/// 高级接口：对 SpeakerSegments 执行完整调度，返回分批结果
pub fn schedule_segments(
    segments: Vec<SpeakerSegment>,
    config: DurationSchedulerConfig,
) -> Vec<Vec<SpeakerSegment>> {
    let mut scheduler = DurationScheduler::new(config);
    let mut batches = Vec::new();

    for seg in segments {
        let decision = scheduler.add_segment(seg);
        match decision {
            SubmitDecision::Submit => {
                batches.push(scheduler.take_submitted(None));
            }
            SubmitDecision::SplitAndSubmit(n) => {
                batches.push(scheduler.take_submitted(Some(n)));
            }
            SubmitDecision::Wait => {}
        }
    }

    // Flush remaining
    batches.extend(scheduler.flush());
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_seg(start: f64, end: f64, speaker: usize) -> SpeakerSegment {
        SpeakerSegment {
            start,
            end,
            speaker,
            embedding_valid: true,
        }
    }

    #[test]
    fn test_single_short_segment_waits() {
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        let mut sched = DurationScheduler::new(config);
        let decision = sched.add_segment(make_seg(0.0, 5.0, 0));
        assert_eq!(decision, SubmitDecision::Wait);
    }

    #[test]
    fn test_accumulate_to_threshold() {
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        // 3 × 10s = 30s = exactly 100% (>= 80%)
        let mut sched = DurationScheduler::new(config);
        let mut decisions = Vec::new();
        for _ in 0..3 {
            let d = sched.add_segment(make_seg(0.0, 10.0, 0));
            decisions.push(d);
        }
        // After 3 segments (30s), should submit
        assert_eq!(decisions[0], SubmitDecision::Wait);
        assert_eq!(decisions[1], SubmitDecision::Wait);
        assert_eq!(decisions[2], SubmitDecision::Submit);
    }

    #[test]
    fn test_early_submit_at_80_pct() {
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        // 2 × 13s = 26s = 87% (>= 80%)
        let mut sched = DurationScheduler::new(config);
        assert_eq!(sched.add_segment(make_seg(0.0, 13.0, 0)), SubmitDecision::Wait);
        assert_eq!(sched.add_segment(make_seg(13.0, 26.0, 0)), SubmitDecision::Submit);
    }

    #[test]
    fn test_split_at_120_pct() {
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        // 1 × 40s = 40s > 36s (120%)
        let decision = DurationScheduler::new(config).add_segment(make_seg(0.0, 40.0, 0));
        assert_eq!(decision, SubmitDecision::SplitAndSubmit(1));
    }

    #[test]
    fn test_split_multiple_segments() {
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        // 4 × 10s = 40s > 36s
        let mut sched = DurationScheduler::new(config);
        for _ in 0..3 {
            sched.add_segment(make_seg(0.0, 10.0, 0));
        }
        // After 3 segments (30s), should submit
        let d4 = sched.add_segment(make_seg(30.0, 40.0, 0));
        // Now total = 40s > 36s, but the segment itself takes it over
        // It should split: first 3 (30s) submitted, 4th stays
        assert_eq!(d4, SubmitDecision::SplitAndSubmit(3));
    }

    #[test]
    fn test_flush_remaining() {
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        let mut sched = DurationScheduler::new(config);
        sched.add_segment(make_seg(0.0, 5.0, 0));
        let batches = sched.flush();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }

    #[test]
    fn test_schedule_segments_full_flow() {
        let segments = vec![
            make_seg(0.0, 12.0, 0),
            make_seg(12.0, 24.0, 0),
            make_seg(24.0, 36.0, 1),
            make_seg(36.0, 40.0, 1),
        ];
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        let batches = schedule_segments(segments, config);
        // 0-24s = 24s (>= 80%), submit batch 0
        // 24-36s = 12s (< 80%), 24-40s = 16s (wait), flush → batch 1
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2); // 0-24s (2 segments, speaker 0)
        assert_eq!(batches[1].len(), 2); // 24-40s (2 segments, speaker 1)
    }

    #[test]
    fn test_single_oversized_segment() {
        let config = DurationSchedulerConfig {
            max_duration_ms: 30000,
            ..Default::default()
        };
        let mut sched = DurationScheduler::new(config);
        let decision = sched.add_segment(make_seg(0.0, 35.0, 0)); // 35s > 30s but < 36s
        assert_eq!(decision, SubmitDecision::Submit); // 35s between 80% and 120%
    }
}
