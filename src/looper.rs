//! Fixed-length loop recorder for drum mode.
//!
//! The loop is `bars` long at `bpm`. Pad hits made while recording are stored
//! at their position in the loop (snapped to the quantize grid) and replayed
//! every pass; recording again overdubs on top. All timing is in microseconds
//! since the transport started, so the logic is deterministic and testable.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One MIDI note message, tagged with the deck key that produced it (for lighting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopNote {
    pub on: bool,
    pub channel: u8,
    pub note: u8,
    pub velocity: u8,
    pub key: Option<u8>,
}

struct Event {
    pos: u64,
    note: LoopNote,
    /// Occurrences at or before this time are skipped: the player already
    /// heard the hit live, so the loop must not trigger it a second time.
    muted_until: u64,
}

struct Pending {
    on_pos: u64,
    down_at: u64,
    notes: Vec<LoopNote>,
}

#[derive(Debug, Clone, Copy)]
pub struct Click {
    pub channel: u8,
    pub note: u8,
}

pub struct Looper {
    len: u64,
    beat: u64,
    step: u64,
    click: Option<Click>,
    events: Vec<Event>,
    pending: HashMap<u8, Pending>,
    sounding: Vec<(u8, u8, Option<u8>)>,
    start: Option<Instant>,
    recording: bool,
    /// Everything scheduled before this time has been emitted.
    emitted_before: u64,
}

const MIN_NOTE_LEN: u64 = 20_000;

impl Looper {
    /// `quantize` is the grid in notes per bar (16 = sixteenths, 0 = off).
    pub fn new(bpm: f64, bars: u32, quantize: u32, click: Option<Click>) -> Looper {
        let beat = (60_000_000.0 / bpm).round() as u64;
        let len = beat * 4 * bars as u64;
        let step = if quantize == 0 { 0 } else { beat * 4 / quantize as u64 };
        Looper {
            len,
            beat,
            step,
            click,
            events: Vec::new(),
            pending: HashMap::new(),
            sounding: Vec::new(),
            start: None,
            recording: false,
            emitted_before: 0,
        }
    }

    pub fn is_running(&self) -> bool {
        self.start.is_some()
    }

    pub fn is_recording(&self) -> bool {
        self.recording
    }

    fn elapsed(&self, now: Instant) -> Option<u64> {
        self.start.map(|s| now.saturating_duration_since(s).as_micros() as u64)
    }

    fn start(&mut self, now: Instant) {
        self.start = Some(now);
        self.emitted_before = 0;
        for event in &mut self.events {
            event.muted_until = 0;
        }
    }

    /// REC: starts the transport recording if stopped, else toggles overdub.
    pub fn toggle_record(&mut self, now: Instant) {
        if self.is_running() {
            self.recording = !self.recording;
        } else {
            self.start(now);
            self.recording = true;
        }
    }

    /// PLAY: starts or stops the loop. Returns note-offs to send when stopping.
    pub fn toggle_play(&mut self, now: Instant) -> Vec<LoopNote> {
        if self.is_running() {
            self.stop(now)
        } else {
            self.start(now);
            Vec::new()
        }
    }

    /// Stops the transport, keeping the recorded loop. Returns note-offs for
    /// anything the loop left sounding.
    pub fn stop(&mut self, now: Instant) -> Vec<LoopNote> {
        // Close notes still held so they do not hang on the next playback.
        let keys: Vec<u8> = self.pending.keys().copied().collect();
        for key in keys {
            self.pad_up(now, key);
        }
        self.start = None;
        self.recording = false;
        self.sounding
            .drain(..)
            .map(|(channel, note, key)| LoopNote { on: false, channel, note, velocity: 0, key })
            .collect()
    }

    /// Stops and erases the loop.
    pub fn clear(&mut self, now: Instant) -> Vec<LoopNote> {
        let offs = self.stop(now);
        self.events.clear();
        self.pending.clear();
        offs
    }

    /// A pad was hit live; `notes` are its note-ons. Stored only while recording.
    pub fn pad_down(&mut self, now: Instant, key: u8, notes: &[LoopNote]) {
        let Some(at) = self.elapsed(now).filter(|_| self.recording) else {
            return;
        };
        let pos = at % self.len;
        let on_pos = if self.step == 0 { pos } else { (pos + self.step / 2) / self.step * self.step % self.len };
        let muted_until = at + self.step.max(1_000);
        for note in notes {
            self.events.push(Event { pos: on_pos, note: LoopNote { key: Some(key), ..*note }, muted_until });
        }
        self.pending.insert(key, Pending { on_pos, down_at: at, notes: notes.to_vec() });
        self.sort();
    }

    /// The pad was released: store the matching note-offs.
    pub fn pad_up(&mut self, now: Instant, key: u8) {
        let (Some(p), Some(at)) = (self.pending.remove(&key), self.elapsed(now)) else {
            return;
        };
        let longest = self.len - self.step.max(MIN_NOTE_LEN);
        let held = at.saturating_sub(p.down_at).clamp(MIN_NOTE_LEN, longest);
        let off_pos = (p.on_pos + held) % self.len;
        let muted_until = at + self.step.max(1_000);
        for note in &p.notes {
            self.events.push(Event { pos: off_pos, note: LoopNote { on: false, velocity: 0, key: Some(key), ..*note }, muted_until });
        }
        self.sort();
    }

    fn sort(&mut self) {
        // Note-offs first at equal positions, so a repeated note is released before it retriggers.
        self.events.sort_by_key(|e| (e.pos, e.note.on));
    }

    /// Returns every message that has come due since the last call, in order.
    pub fn poll(&mut self, now: Instant) -> Vec<LoopNote> {
        let Some(at) = self.elapsed(now) else {
            return Vec::new();
        };
        let mut due: Vec<(u64, LoopNote)> = Vec::new();
        for pass in (self.emitted_before / self.len)..=(at / self.len) {
            for event in &self.events {
                let t = pass * self.len + event.pos;
                if t >= self.emitted_before && t <= at && t > event.muted_until {
                    due.push((t, event.note));
                }
            }
        }
        if let (true, Some(click)) = (self.recording, self.click) {
            let mut t = self.emitted_before.div_ceil(self.beat) * self.beat;
            while t <= at {
                let velocity = if t % (self.beat * 4) == 0 { 110 } else { 60 };
                let tick = LoopNote { on: true, channel: click.channel, note: click.note, velocity, key: None };
                due.push((t, tick));
                due.push((t, LoopNote { on: false, velocity: 0, ..tick }));
                t += self.beat;
            }
        }
        due.sort_by_key(|(t, _)| *t); // stable: keeps off-before-on within a tick
        self.emitted_before = at + 1;

        let notes: Vec<LoopNote> = due.into_iter().map(|(_, n)| n).collect();
        for n in notes.iter().filter(|n| n.key.is_some()) {
            self.sounding.retain(|s| (s.0, s.1) != (n.channel, n.note));
            if n.on {
                self.sounding.push((n.channel, n.note, n.key));
            }
        }
        notes
    }

    /// How long until `poll` next has something to do, if anything is scheduled.
    pub fn next_due(&self, now: Instant) -> Option<Duration> {
        let at = self.elapsed(now)?;
        let base = self.emitted_before / self.len * self.len;
        let mut next = self
            .events
            .iter()
            .map(|e| if base + e.pos >= self.emitted_before { base + e.pos } else { base + e.pos + self.len })
            .min();
        if self.recording && self.click.is_some() {
            let beat = self.emitted_before.div_ceil(self.beat) * self.beat;
            next = Some(next.map_or(beat, |n| n.min(beat)));
        }
        next.map(|t| Duration::from_micros(t.saturating_sub(at)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KICK: LoopNote = LoopNote { on: true, channel: 9, note: 36, velocity: 100, key: None };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// 120 bpm, one bar: 2000 ms loop, 125 ms sixteenths.
    fn looper(click: bool) -> (Looper, Instant) {
        let click = click.then_some(Click { channel: 9, note: 37 });
        (Looper::new(120.0, 1, 16, click), Instant::now())
    }

    #[test]
    fn replays_a_hit_once_per_pass_on_the_grid() {
        let (mut lp, t0) = looper(false);
        lp.toggle_record(t0);
        // Hit 20 ms early for the grid line at 500 ms; released 100 ms later.
        lp.pad_down(t0 + ms(480), 11, &[KICK]);
        assert!(lp.poll(t0 + ms(560)).is_empty(), "live hit must not be doubled by the loop");
        lp.pad_up(t0 + ms(580), 11);
        assert!(lp.poll(t0 + ms(1999)).is_empty());

        // Second pass: on at 2500 (quantized), off 100 ms later.
        assert!(lp.poll(t0 + ms(2499)).is_empty());
        assert_eq!(lp.next_due(t0 + ms(2499)), Some(ms(1)));
        let on = lp.poll(t0 + ms(2500));
        assert_eq!(on, vec![LoopNote { key: Some(11), ..KICK }]);
        let off = lp.poll(t0 + ms(2700));
        assert_eq!(off.len(), 1);
        assert!(!off[0].on);

        // A late poll spanning a whole pass still yields exactly one on/off pair.
        let both = lp.poll(t0 + ms(4700));
        assert_eq!(both.iter().map(|n| n.on).collect::<Vec<_>>(), vec![true, false]);
    }

    #[test]
    fn stop_releases_sounding_notes_and_keeps_the_loop() {
        let (mut lp, t0) = looper(false);
        lp.toggle_record(t0);
        lp.pad_down(t0 + ms(500), 3, &[KICK]);
        lp.pad_up(t0 + ms(1500), 3);
        assert_eq!(lp.poll(t0 + ms(2600)).len(), 1); // note-on of the second pass
        let offs = lp.toggle_play(t0 + ms(2700));
        assert_eq!(offs, vec![LoopNote { on: false, channel: 9, note: 36, velocity: 0, key: Some(3) }]);
        assert!(!lp.is_running() && !lp.is_recording());

        // Playing again starts from the top with the same material.
        let t1 = t0 + ms(10_000);
        assert!(lp.toggle_play(t1).is_empty());
        assert_eq!(lp.poll(t1 + ms(500)).len(), 1);
        assert!(lp.clear(t1 + ms(600)).len() == 1);
        assert!(lp.toggle_play(t1 + ms(700)).is_empty());
        assert!(lp.poll(t1 + ms(5000)).is_empty());
    }

    #[test]
    fn hits_are_ignored_unless_recording() {
        let (mut lp, t0) = looper(false);
        lp.pad_down(t0, 1, &[KICK]); // transport stopped
        lp.toggle_play(t0);
        lp.pad_down(t0 + ms(100), 1, &[KICK]); // playing, not recording
        lp.pad_up(t0 + ms(200), 1);
        assert!(lp.poll(t0 + ms(9000)).is_empty());
    }

    #[test]
    fn click_ticks_every_beat_while_recording_only() {
        let (mut lp, t0) = looper(true);
        lp.toggle_record(t0);
        let ticks = lp.poll(t0 + ms(1600)); // beats at 0, 500, 1000, 1500
        let ons: Vec<u8> = ticks.iter().filter(|n| n.on).map(|n| n.velocity).collect();
        assert_eq!(ons, vec![110, 60, 60, 60]);
        assert_eq!(lp.next_due(t0 + ms(1600)), Some(ms(400)));
        lp.toggle_record(t0 + ms(1700)); // overdub off
        assert!(lp.poll(t0 + ms(4000)).is_empty());
        assert_eq!(lp.next_due(t0 + ms(4000)), None);
    }
}
