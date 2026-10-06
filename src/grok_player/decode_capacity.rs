use std::collections::VecDeque;
use std::time::Instant;

// a reading averages this many recent decodes
const SAMPLE_WINDOW: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecodePath {
    Cpu,
    Device,
}

// frames a second the pool could decode, timed from its decodes rather than counted from the frames playback took
pub(super) struct DecodeCapacity {
    workers: usize,
    since_generation: u64,
    // one worker's seconds per frame, both eyes of a stereo frame counted
    cpu_frame_seconds: VecDeque<f64>,
    // seconds per frame between results from a device that had more frames to pull
    device_frame_seconds: VecDeque<f64>,
    last_device_return: Option<Instant>,
    // the device had more frames to pull when the last result came back
    device_busy_since_last_return: bool,
    latest: Option<DecodePath>,
}

fn push_recent(samples: &mut VecDeque<f64>, seconds: f64) {
    samples.push_back(seconds);
    if samples.len() > SAMPLE_WINDOW {
        samples.pop_front();
    }
}

fn mean(samples: &VecDeque<f64>, fewest: usize) -> Option<f64> {
    if samples.len() < fewest {
        return None;
    }
    Some(samples.iter().sum::<f64>() / samples.len() as f64)
}

impl DecodeCapacity {
    pub fn new(workers: usize) -> Self {
        DecodeCapacity {
            workers,
            since_generation: 0,
            cpu_frame_seconds: VecDeque::new(),
            device_frame_seconds: VecDeque::new(),
            last_device_return: None,
            device_busy_since_last_return: false,
            latest: None,
        }
    }

    // a new source, scale, colour or stereo output decodes at its own speed
    pub fn restart(&mut self, generation: u64) {
        *self = DecodeCapacity {
            since_generation: generation,
            ..DecodeCapacity::new(self.workers)
        };
    }

    pub fn record_cpu(&mut self, generation: u64, seconds: f64, jobs_per_frame: usize) {
        if generation < self.since_generation {
            return;
        }
        push_recent(&mut self.cpu_frame_seconds, seconds * jobs_per_frame as f64);
        self.latest = Some(DecodePath::Cpu);
    }

    pub fn record_device_return(
        &mut self,
        generation: u64,
        at: Instant,
        jobs_per_frame: usize,
        work_waiting: bool,
    ) {
        if generation < self.since_generation {
            return;
        }
        if let (Some(previous), true) =
            (self.last_device_return, self.device_busy_since_last_return)
        {
            let gap = at.saturating_duration_since(previous).as_secs_f64();
            push_recent(&mut self.device_frame_seconds, gap * jobs_per_frame as f64);
        }
        self.last_device_return = Some(at);
        self.device_busy_since_last_return = work_waiting;
        self.latest = Some(DecodePath::Device);
    }

    pub fn frames_per_second(&self) -> Option<f64> {
        // one decode per worker, which refilling the lookahead window of two per worker always gives
        let fewest = self.workers;
        match self.latest? {
            DecodePath::Cpu => mean(&self.cpu_frame_seconds, fewest)
                .filter(|seconds| *seconds > 0.0)
                .map(|seconds| self.workers as f64 / seconds),
            DecodePath::Device => mean(&self.device_frame_seconds, fewest)
                .filter(|seconds| *seconds > 0.0)
                .map(|seconds| 1.0 / seconds),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const WORKERS: usize = 4;

    #[test]
    fn nothing_reads_until_enough_decodes_are_timed() {
        let mut capacity = DecodeCapacity::new(WORKERS);
        for _ in 1..WORKERS {
            capacity.record_cpu(0, 0.1, 1);
        }
        assert_eq!(capacity.frames_per_second(), None);
        capacity.record_cpu(0, 0.1, 1);
        assert_eq!(capacity.frames_per_second(), Some(40.0));
    }

    #[test]
    fn both_eyes_of_a_frame_halve_the_reading() {
        let mut capacity = DecodeCapacity::new(WORKERS);
        for _ in 0..WORKERS {
            capacity.record_cpu(0, 0.1, 2);
        }
        assert_eq!(capacity.frames_per_second(), Some(20.0));
    }

    #[test]
    fn a_restart_drops_the_old_readings_and_late_old_results() {
        let mut capacity = DecodeCapacity::new(WORKERS);
        for _ in 0..WORKERS {
            capacity.record_cpu(1, 0.1, 1);
        }
        capacity.restart(2);
        assert_eq!(capacity.frames_per_second(), None);
        for _ in 0..WORKERS {
            capacity.record_cpu(1, 1.0, 1);
            capacity.record_cpu(2, 0.05, 1);
        }
        assert_eq!(capacity.frames_per_second(), Some(80.0));
    }

    #[test]
    fn the_device_reads_only_the_gaps_it_spent_busy() {
        let mut capacity = DecodeCapacity::new(WORKERS);
        let start = Instant::now();
        let at = |milliseconds: u64| start + Duration::from_millis(milliseconds);
        // a starved stretch, then results 25 ms apart with work waiting
        capacity.record_device_return(0, at(0), 1, false);
        capacity.record_device_return(0, at(500), 1, true);
        for result in 1..=WORKERS as u64 {
            capacity.record_device_return(0, at(500 + result * 25), 1, true);
        }
        let reading = capacity.frames_per_second().expect("a device reading");
        assert!((reading - 40.0).abs() < 1e-6, "{reading}");
    }
}
