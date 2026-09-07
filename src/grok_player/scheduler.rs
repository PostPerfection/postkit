use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use super::compositor::{Compositor, Layers};
use super::decode_pool::{DecodeJob, DecodePool};
use super::timeline::Timeline;
use super::{
    Command, DecodeScale, MILLISECONDS_PER_SECOND, OverlayRectangle, Rgba8Frame, Shared, Status,
    SubtitleSlot,
};
use crate::subtitle_formats::{StyledCue, StyledRun, VAlign};

// a worker finishing a frame wakes the scheduler sooner than this
const IDLE_WAIT: Duration = Duration::from_millis(100);
const PRESENTATION_WINDOW_FRAMES: usize = 24;
const SRT_EXTENSION: &str = "srt";
const ASS_EXTENSIONS: [&str; 2] = ["ass", "ssa"];

#[derive(Clone, Copy)]
pub(super) struct Clock {
    start: Instant,
    start_frame: u64,
    fps: f64,
}

impl Clock {
    fn new(start: Instant, start_frame: u64, fps: f64) -> Self {
        Clock {
            start,
            start_frame,
            fps,
        }
    }

    fn target_frame(&self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        self.start_frame + (elapsed * self.fps).floor().max(0.0) as u64
    }

    fn display_time(&self, frame: u64) -> Instant {
        let ahead = frame.saturating_sub(self.start_frame) as f64 / self.fps;
        self.start + Duration::from_secs_f64(ahead)
    }

    fn frame_period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.fps)
    }
}

fn frames_to_request(
    current: u64,
    frame_count: u64,
    lookahead: usize,
    pending: impl Fn(u64) -> bool,
) -> Vec<u64> {
    if frame_count == 0 {
        return Vec::new();
    }
    let last = frame_count - 1;
    let end = current.saturating_add(lookahead as u64).min(last);
    (current.min(last)..=end)
        .filter(|index| !pending(*index))
        .collect()
}

#[derive(Default)]
struct SubtitleTrack {
    cues: Vec<StyledCue>,
    visible: bool,
}

pub(super) fn run(shared: Arc<Shared>, commands: Receiver<Command>, finished: Sender<Command>) {
    let mut scheduler = Scheduler::new(shared, finished);
    loop {
        scheduler.tick();
        scheduler.pump();
        scheduler.publish_status();
        match commands.recv_timeout(scheduler.next_wait()) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(command) => scheduler.handle(command),
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

struct Scheduler {
    shared: Arc<Shared>,
    pool: DecodePool,
    timeline: Option<Timeline>,
    in_flight: BTreeSet<u64>,
    generation: u64,
    current_frame: u64,
    needs_publish: bool,
    playing: bool,
    eof: bool,
    reduce: u8,
    clock: Option<Clock>,
    last_plain: Option<Arc<Rgba8Frame>>,
    overlays: Vec<OverlayRectangle>,
    subtitle: SubtitleTrack,
    caption: SubtitleTrack,
    compositor: Compositor,
    presentations: VecDeque<Instant>,
    dropped_frames: u64,
    delayed_frames: u64,
}

impl Scheduler {
    fn new(shared: Arc<Shared>, finished: Sender<Command>) -> Self {
        let pool = DecodePool::start(finished);
        shared.set_lookahead(pool.lookahead_frames());
        Scheduler {
            shared,
            pool,
            timeline: None,
            in_flight: BTreeSet::new(),
            generation: 0,
            current_frame: 0,
            needs_publish: false,
            playing: false,
            eof: false,
            reduce: DecodeScale::Full.reduce(),
            clock: None,
            last_plain: None,
            overlays: Vec::new(),
            subtitle: SubtitleTrack::default(),
            caption: SubtitleTrack::default(),
            compositor: Compositor::new(),
            presentations: VecDeque::new(),
            dropped_frames: 0,
            delayed_frames: 0,
        }
    }

    // ─── the clock ─────────────────────────────────────────────────────────

    fn tick(&mut self) {
        let Some(frame_count) = self.timeline.as_ref().map(|timeline| timeline.frame_count) else {
            return;
        };
        if frame_count == 0 {
            return;
        }
        if self.needs_publish {
            self.publish_current();
            return;
        }
        if !self.playing {
            return;
        }
        let Some(clock) = self.clock else {
            return;
        };
        let next = self.current_frame + 1;
        if next >= frame_count {
            self.reach_end();
            return;
        }
        let now = Instant::now();
        if now < clock.display_time(next) {
            return;
        }
        let target = clock.target_frame(now).clamp(next, frame_count - 1);
        let Some(index) = self.pool.newest_decoded_in(next, target) else {
            self.skip_over_a_failed_frame(target);
            return;
        };
        self.dropped_frames += index - next;
        if now > clock.display_time(index) + clock.frame_period() {
            self.delayed_frames += 1;
        }
        let Some(plain) = self.pool.decoded(index) else {
            return;
        };
        self.present(index, plain);
    }

    fn skip_over_a_failed_frame(&mut self, target: u64) {
        let Some(reason) = self.pool.failure(target) else {
            return;
        };
        tracing::error!("frame {target} did not decode: {reason}");
        self.dropped_frames += target - self.current_frame;
        self.current_frame = target;
    }

    fn publish_current(&mut self) {
        let index = self.current_frame;
        if let Some(plain) = self.pool.decoded(index) {
            self.needs_publish = false;
            self.present(index, plain);
            return;
        }
        if let Some(reason) = self.pool.failure(index) {
            tracing::error!("frame {index} did not decode: {reason}");
            self.needs_publish = false;
        }
    }

    fn reach_end(&mut self) {
        self.playing = false;
        self.eof = true;
        self.clock = None;
        self.presentations.clear();
        self.shared.fire_update();
    }

    fn present(&mut self, index: u64, plain: Arc<Rgba8Frame>) {
        self.current_frame = index;
        self.last_plain = Some(plain.clone());
        self.compose_and_publish(&plain);
        self.pool.forget_before(index.saturating_sub(1));
        self.presentations.push_back(Instant::now());
        while self.presentations.len() > PRESENTATION_WINDOW_FRAMES {
            self.presentations.pop_front();
        }
    }

    fn compose_and_publish(&mut self, picture: &Arc<Rgba8Frame>) {
        let Some(timeline) = self.timeline.as_ref() else {
            return;
        };
        let overlay_scale = f64::from(picture.width) / f64::from(timeline.width.max(1));
        let time_ms = (self.current_frame as f64 / timeline.fps * MILLISECONDS_PER_SECOND) as u64;
        let mut cue_tracks: Vec<&[StyledCue]> = Vec::new();
        for track in [&self.subtitle, &self.caption] {
            if track.visible {
                cue_tracks.push(&track.cues);
            }
        }
        let layers = Layers {
            overlays: &self.overlays,
            overlay_scale,
            cue_tracks: &cue_tracks,
            time_ms,
        };
        match self.compositor.compose(picture, layers) {
            Ok(composed) => self.shared.publish(composed),
            Err(reason) => tracing::error!("composing frame {}: {reason}", self.current_frame),
        }
    }

    // an overlay or visibility change while paused has no new frame coming
    fn recompose(&mut self) {
        if self.playing {
            return;
        }
        let Some(plain) = self.last_plain.clone() else {
            return;
        };
        self.compose_and_publish(&plain);
    }

    // ─── keeping the cache full ────────────────────────────────────────────

    fn pump(&mut self) {
        let Some(frame_count) = self.timeline.as_ref().map(|timeline| timeline.frame_count) else {
            return;
        };
        let lookahead = self.pool.lookahead_frames();
        let wanted = {
            let pool = &self.pool;
            self.in_flight.retain(|index| !pool.holds(*index));
            let in_flight = &self.in_flight;
            frames_to_request(self.current_frame, frame_count, lookahead, |index| {
                pool.holds(index) || in_flight.contains(&index)
            })
        };
        let generation = self.generation;
        let reduce = self.reduce;
        // reading a whole window at once stalls the clock
        let batch = self.pool.worker_count;
        let Some(timeline) = self.timeline.as_mut() else {
            return;
        };
        for index in wanted.into_iter().take(batch) {
            match timeline.codestream(index) {
                Ok((codestream, render, mxf)) => {
                    self.in_flight.insert(index);
                    self.pool.submit(DecodeJob {
                        generation,
                        frame_index: index,
                        codestream,
                        reduce,
                        render,
                        mxf,
                    });
                }
                Err(reason) => self.pool.record_failure(generation, index, reason),
            }
        }
    }

    fn next_wait(&self) -> Duration {
        if !self.playing {
            return IDLE_WAIT;
        }
        let Some(clock) = self.clock else {
            return IDLE_WAIT;
        };
        let due = clock.display_time(self.current_frame + 1);
        let now = Instant::now();
        if now < due {
            return due - now;
        }
        // past due with nothing decoded, and a finished decode wakes this sooner
        clock.frame_period()
    }

    // ─── commands ──────────────────────────────────────────────────────────

    fn handle(&mut self, command: Command) {
        match command {
            Command::Load(source, reply) => {
                let outcome = self.load(&source);
                // the caller reads duration and size the moment load returns
                self.publish_status();
                let _ = reply.send(outcome);
            }
            Command::Stop => self.stop(),
            Command::SetPaused(paused) => self.set_paused(paused),
            Command::TogglePause => self.set_paused(self.playing),
            Command::Seek(seconds) => {
                let from = self.position().unwrap_or(0.0);
                self.seek_to_seconds(from + seconds);
            }
            Command::SeekAbsolute(seconds) => self.seek_to_seconds(seconds),
            Command::Step(delta) => {
                self.set_paused(true);
                let landing = self.current_frame as i64 + delta;
                self.seek_to_frame(landing.max(0) as u64);
            }
            Command::SetDecodeScale(scale) => {
                if self.reduce == scale.reduce() {
                    return;
                }
                self.reduce = scale.reduce();
                self.restart_decoding();
            }
            Command::SetSubtitleFile(slot, file, reply) => {
                let _ = reply.send(self.set_subtitle_file(slot, file.as_deref()));
                self.recompose();
            }
            Command::SetSubtitleVisibility(slot, visible) => {
                self.track_mut(slot).visible = visible;
                self.recompose();
            }
            Command::SetOverlay(rectangles) => {
                self.overlays = rectangles;
                self.recompose();
            }
            Command::DecodeFinished => {}
            Command::Shutdown => {}
        }
    }

    fn load(&mut self, source: &Path) -> Result<(), String> {
        self.stop();
        let timeline = Timeline::open(source)?;
        self.shared
            .set_source_size(Some((timeline.width, timeline.height)));
        self.timeline = Some(timeline);
        self.current_frame = 0;
        self.needs_publish = true;
        self.restart_decoding();
        Ok(())
    }

    fn stop(&mut self) {
        self.timeline = None;
        self.last_plain = None;
        self.playing = false;
        self.eof = false;
        self.clock = None;
        self.current_frame = 0;
        self.needs_publish = false;
        self.dropped_frames = 0;
        self.delayed_frames = 0;
        self.presentations.clear();
        self.restart_decoding();
        self.shared.clear_frame();
        self.shared.set_source_size(None);
        self.shared.fire_update();
    }

    fn restart_decoding(&mut self) {
        self.generation += 1;
        self.in_flight.clear();
        self.pool.restart(self.generation);
        if self.timeline.is_some() {
            self.needs_publish = true;
        }
        if self.playing {
            // otherwise the refill stall counts as dropped frames
            self.clock = Some(Clock::new(Instant::now(), self.current_frame, self.fps()));
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if paused {
            self.playing = false;
            self.clock = None;
            self.presentations.clear();
            return;
        }
        if self.timeline.is_none() {
            return;
        }
        // play on the last frame starts over, mpv's keep-open behaviour
        if self.eof {
            self.seek_to_frame(0);
        }
        self.playing = true;
        self.clock = Some(Clock::new(Instant::now(), self.current_frame, self.fps()));
    }

    fn seek_to_seconds(&mut self, seconds: f64) {
        let fps = self.fps();
        let frame = (seconds.max(0.0) * fps).round() as u64;
        self.seek_to_frame(frame);
    }

    fn seek_to_frame(&mut self, frame: u64) {
        let Some(frame_count) = self.timeline.as_ref().map(|timeline| timeline.frame_count) else {
            return;
        };
        if frame_count == 0 {
            return;
        }
        self.current_frame = frame.min(frame_count - 1);
        self.eof = false;
        self.presentations.clear();
        self.restart_decoding();
    }

    fn set_subtitle_file(&mut self, slot: SubtitleSlot, file: Option<&Path>) -> Result<(), String> {
        let Some(file) = file else {
            self.track_mut(slot).cues.clear();
            return Ok(());
        };
        let cues = parse_subtitles(file)?;
        self.compositor.prepare_subtitles()?;
        let top_of_frame = slot == SubtitleSlot::Caption;
        let track = self.track_mut(slot);
        track.cues = cues;
        track.visible = true;
        if top_of_frame {
            for cue in &mut track.cues {
                cue.valign = Some(VAlign::Top);
            }
        }
        Ok(())
    }

    fn track_mut(&mut self, slot: SubtitleSlot) -> &mut SubtitleTrack {
        match slot {
            SubtitleSlot::Subtitle => &mut self.subtitle,
            SubtitleSlot::Caption => &mut self.caption,
        }
    }

    // ─── what the public methods read ──────────────────────────────────────

    fn fps(&self) -> f64 {
        self.timeline
            .as_ref()
            .map(|timeline| timeline.fps)
            .unwrap_or(1.0)
    }

    fn position(&self) -> Option<f64> {
        let timeline = self.timeline.as_ref()?;
        Some(self.current_frame as f64 / timeline.fps)
    }

    fn playback_rate(&self) -> Option<f64> {
        if self.presentations.len() < 2 {
            return None;
        }
        let first = *self.presentations.front()?;
        let last = *self.presentations.back()?;
        let span = last.saturating_duration_since(first).as_secs_f64();
        (span > 0.0).then(|| (self.presentations.len() - 1) as f64 / span)
    }

    fn publish_status(&self) {
        let timeline = self.timeline.as_ref();
        self.shared.set_status(Status {
            position: self.position(),
            duration: timeline.map(|timeline| timeline.frame_count as f64 / timeline.fps),
            paused: !self.playing,
            filename: timeline.map(|timeline| timeline.title.clone()),
            dropped_frames: self.dropped_frames,
            delayed_frames: self.delayed_frames,
            decoder_fps: self.playback_rate(),
            container_fps: timeline.map(|timeline| timeline.fps),
            eof: self.eof,
        });
        self.shared
            .set_cached_frames(self.pool.cached_frame_count());
    }
}

fn parse_subtitles(file: &Path) -> Result<Vec<StyledCue>, String> {
    let extension = file
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .unwrap_or_default();
    let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    if extension == SRT_EXTENSION {
        return Ok(crate::subtitle_retime::parse_srt(&text)
            .into_iter()
            .map(|cue| StyledCue::text(cue.start_ms, cue.end_ms, vec![StyledRun::plain(cue.text)]))
            .collect());
    }
    if ASS_EXTENSIONS.contains(&extension.as_str()) {
        return crate::subtitle_formats::ass::parse_ass(&text)
            .map(|parsed| parsed.cues)
            .map_err(|reason| reason.to_string());
    }
    Err(format!(
        "{} is a .{extension} subtitle, and the player reads only .{SRT_EXTENSION}, .{}, and .{}",
        file.display(),
        ASS_EXTENSIONS[0],
        ASS_EXTENSIONS[1]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn the_target_frame_follows_the_wall_clock() {
        let start = Instant::now();
        let clock = Clock::new(start, 10, 24.0);
        assert_eq!(clock.target_frame(start), 10);
        // 2.4 frames have elapsed, so frame 12 is the one due
        assert_eq!(clock.target_frame(start + Duration::from_millis(100)), 12);
        assert_eq!(clock.target_frame(start + Duration::from_secs(1)), 34);
        // a clock read before its own start does not run backwards
        assert_eq!(clock.target_frame(start - Duration::from_secs(1)), 10);
    }

    #[test]
    fn a_frames_display_time_is_its_distance_from_the_start_frame() {
        let start = Instant::now();
        let clock = Clock::new(start, 10, 24.0);
        assert_eq!(clock.display_time(10), start);
        let due = clock.display_time(13).saturating_duration_since(start);
        assert!(
            (due.as_secs_f64() - 0.125).abs() < 1e-9,
            "frame 13 came due at {due:?}"
        );
        assert_eq!(clock.frame_period(), Duration::from_secs_f64(1.0 / 24.0));
    }

    #[test]
    fn the_window_asks_for_the_current_frame_and_the_lookahead() {
        let nothing_pending = |_| false;
        assert_eq!(
            frames_to_request(0, 100, 4, nothing_pending),
            vec![0, 1, 2, 3, 4]
        );
        assert_eq!(
            frames_to_request(97, 100, 4, nothing_pending),
            vec![97, 98, 99],
            "the window stops at the last frame"
        );
        assert!(frames_to_request(0, 0, 4, nothing_pending).is_empty());
    }

    #[test]
    fn the_window_skips_what_is_cached_or_in_flight() {
        let cached: BTreeSet<u64> = [5, 6, 9].into_iter().collect();
        let in_flight: BTreeSet<u64> = [7].into_iter().collect();
        assert_eq!(
            frames_to_request(5, 100, 5, |index| cached.contains(&index)
                || in_flight.contains(&index)),
            vec![8, 10]
        );
    }
}
