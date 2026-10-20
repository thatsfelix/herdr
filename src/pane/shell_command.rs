//! Opt-in, bounded capture of a paired foreground shell time window.
//! OSC boundaries are not authenticated, and PTY background output is inseparable.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::osc::OscStreamCollector;

const MAX_TAIL: usize = 64 * 1024;
// Leave headroom below native notification truncation so the final line survives.
const MAX_SUMMARY_BYTES: usize = 1024;
const MAX_COMPLETIONS_PER_CHUNK: usize = 8;
const MAX_THRESHOLD_MS: u64 = 86_400_000;
const PREFIX: &str = "1337;HerdrCommand;";
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShellCommandCompletion {
    pub generation: u64,
    pub sequence: u64,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub output: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FrozenCommand {
    generation: u64,
    sequence: u64,
    exit_code: i32,
    duration_ms: u64,
    tail: VecDeque<u8>,
    initial_control_state: u8,
}

impl FrozenCommand {
    pub(super) fn finish(self) -> ShellCommandCompletion {
        ShellCommandCompletion {
            generation: self.generation,
            sequence: self.sequence,
            exit_code: self.exit_code,
            duration_ms: self.duration_ms,
            output: clean_output(self.tail, self.initial_control_state),
        }
    }
}

struct Running {
    shell_sequence: u64,
    started: Instant,
    threshold_ms: u64,
    tail: VecDeque<u8>,
    initial_control_state: u8,
}

pub(super) struct ShellCommandTracker {
    collector: OscStreamCollector,
    pub(super) generation: u64,
    sequence: u64,
    last_shell_sequence: u64,
    enabled: bool,
    running: Option<Running>,
    // Small fixed CSI parser; detects alternate-screen entry even within one chunk.
    csi: [u8; 64],
    csi_len: usize,
    escape_state: u8,
    alternate_screen: bool,
    screen_initialized: bool,
    #[cfg(test)]
    screen_control_steps: usize,
}

impl Default for ShellCommandTracker {
    fn default() -> Self {
        Self {
            collector: OscStreamCollector::default(),
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            sequence: 0,
            last_shell_sequence: 0,
            enabled: false,
            running: None,
            csi: [0; 64],
            csi_len: 0,
            escape_state: 0,
            alternate_screen: false,
            screen_initialized: false,
            #[cfg(test)]
            screen_control_steps: 0,
        }
    }
}

impl ShellCommandTracker {
    pub(super) fn needs_screen_initialization(&self) -> bool {
        !self.screen_initialized
    }

    pub(super) fn initialize_screen(&mut self, alternate: bool) {
        if !self.screen_initialized {
            self.alternate_screen = alternate;
            self.screen_initialized = true;
        }
    }

    pub(super) fn cancel(&mut self) {
        self.running = None;
    }

    pub(super) fn observe(&mut self, bytes: &[u8], now: Instant) -> Vec<FrozenCommand> {
        let mut collector = std::mem::take(&mut self.collector);
        let mut completed = Vec::new();
        let mut offset = 0;
        collector.observe_ordered(bytes, |body, end| {
            self.output(&bytes[offset..end]);
            offset = end;
            if let Some(result) = self.boundary(body, now) {
                if completed.len() < MAX_COMPLETIONS_PER_CHUNK {
                    completed.push(result);
                }
            }
        });
        self.output(&bytes[offset..]);
        self.collector = collector;
        completed
    }

    fn output(&mut self, bytes: &[u8]) {
        let mut cursor = 0;
        while cursor < bytes.len() {
            // Like OscStreamCollector, skip ordinary spans instead of running the
            // control state machine for every PTY byte, including before opt-in.
            let skip = match self.escape_state {
                0 | 5 => Some(bytes[cursor..].iter().position(|b| *b == 0x1b)),
                3 => Some(bytes[cursor..].iter().position(|b| matches!(*b, 7 | 0x1b))),
                _ => None,
            };
            if let Some(next_control) = skip {
                let Some(offset) = next_control else {
                    break;
                };
                cursor += offset;
            }
            let byte = bytes[cursor];
            cursor += 1;
            #[cfg(test)]
            {
                self.screen_control_steps += 1;
            }
            match self.escape_state {
                0 if byte == 0x1b => self.escape_state = 1,
                1 if byte == b'[' => {
                    self.escape_state = 2;
                    self.csi_len = 0;
                }
                1 | 3..=6 => advance_control(&mut self.escape_state, byte),
                2 if (0x40..=0x7e).contains(&byte) => {
                    if matches!(byte, b'h' | b'l')
                        && self.csi[..self.csi_len]
                            .strip_prefix(b"?")
                            .is_some_and(|modes| {
                                modes
                                    .split(|b| *b == b';')
                                    .any(|m| matches!(m, b"47" | b"1047" | b"1049"))
                            })
                    {
                        self.alternate_screen = byte == b'h';
                        if self.alternate_screen {
                            self.cancel();
                        }
                    }
                    self.escape_state = 0;
                }
                2 => {
                    if self.csi_len == self.csi.len() {
                        self.cancel();
                        self.escape_state = 0;
                    } else {
                        self.csi[self.csi_len] = byte;
                        self.csi_len += 1;
                    }
                }
                _ => {}
            }
        }
        let Some(running) = self.running.as_mut() else {
            return;
        };
        let overflow = (running.tail.len() + bytes.len()).saturating_sub(MAX_TAIL);
        let discarded_tail = overflow.min(running.tail.len());
        for byte in running.tail.drain(..discarded_tail) {
            advance_control(&mut running.initial_control_state, byte);
        }
        let discarded_input = overflow - discarded_tail;
        for &byte in &bytes[..discarded_input] {
            advance_control(&mut running.initial_control_state, byte);
        }
        running.tail.extend(&bytes[discarded_input..]);
    }

    fn boundary(&mut self, body: &[u8], now: Instant) -> Option<FrozenCommand> {
        let text = std::str::from_utf8(body).ok()?;
        let payload = text.strip_prefix(PREFIX)?;
        if payload == "enable" {
            self.cancel();
            self.enabled = true;
            self.last_shell_sequence = 0;
            return None;
        }
        if payload == "disable" {
            self.enabled = false;
            self.cancel();
            return None;
        }
        if !self.enabled {
            return None;
        }
        let mut fields = payload.split(';');
        let fields = (fields.next(), fields.next(), fields.next(), fields.next());
        match fields {
            (Some("start"), Some(sequence), Some(threshold), None) => {
                let parsed = sequence
                    .parse::<u64>()
                    .ok()
                    .zip(threshold.parse::<u64>().ok());
                let Some((sequence, threshold_ms)) = parsed else {
                    self.cancel();
                    return None;
                };
                let overlapping = self.running.take().is_some();
                if self.alternate_screen
                    || overlapping
                    || sequence <= self.last_shell_sequence
                    || threshold_ms > MAX_THRESHOLD_MS
                {
                    return None;
                }
                self.last_shell_sequence = sequence;
                self.escape_state = 0;
                self.running = Some(Running {
                    shell_sequence: sequence,
                    started: now,
                    threshold_ms,
                    tail: VecDeque::new(),
                    initial_control_state: 0,
                });
                None
            }
            (Some("end"), Some(sequence), Some(exit), None) => {
                let running = self.running.take()?;
                if sequence.parse::<u64>().ok()? != running.shell_sequence {
                    return None;
                }
                let exit_code = exit.parse::<i32>().ok().filter(|v| (0..=255).contains(v))?;
                let duration_ms = now
                    .saturating_duration_since(running.started)
                    .as_millis()
                    .min(u64::MAX as u128) as u64;
                if duration_ms < running.threshold_ms {
                    return None;
                }
                self.sequence = self.sequence.checked_add(1)?;
                Some(FrozenCommand {
                    generation: self.generation,
                    sequence: self.sequence,
                    exit_code,
                    duration_ms,
                    tail: running.tail,
                    initial_control_state: running.initial_control_state,
                })
            }
            _ => {
                self.cancel();
                None
            }
        }
    }
}

fn advance_control(state: &mut u8, byte: u8) {
    *state = match (*state, byte) {
        (0, 0x1b) => 1,
        (1, b'[') => 2,
        // OSC accepts BEL or ST; DCS/APC/PM/SOS accept only ST.
        (1, b']') => 3,
        (1, b'P' | b'_' | b'^' | b'X') => 5,
        (1, 0x1b) => 1,
        (1, _) => 0,
        (2, 0x40..=0x7e) => 0,
        (3, 7) | (4, 7) => 0,
        (3, 0x1b) | (4, 0x1b) => 4,
        (4, b'\\') => 0,
        (4, _) => 3,
        (5, 0x1b) | (6, 0x1b) => 6,
        (6, b'\\') => 0,
        (6, _) => 5,
        (state, _) => state,
    };
}

// Runs after releasing the terminal-core lock. Strip control strings/CSI, retain
// CR progress's final line, and bound the notification summary without losing its end.
fn clean_output(tail: VecDeque<u8>, initial_control_state: u8) -> Option<String> {
    let bytes: Vec<u8> = tail.into_iter().collect();
    let mut clean = Vec::with_capacity(bytes.len());
    let mut state = initial_control_state;
    let mut line_start = 0;
    let mut carriage = false;
    for byte in bytes {
        match state {
            0 if byte == 0x1b => state = 1,
            1..=6 => advance_control(&mut state, byte),
            _ => {
                if byte == b'\r' {
                    carriage = true;
                    continue;
                }
                if carriage {
                    if byte != b'\n' {
                        clean.truncate(line_start);
                    }
                    carriage = false;
                }
                if byte == b'\n' {
                    clean.push(byte);
                    line_start = clean.len();
                } else if byte == 8 {
                    if clean.len() > line_start {
                        clean.pop();
                    }
                } else if byte == b'\t' || byte >= 32 {
                    clean.push(byte);
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&clean);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut start = text.len().saturating_sub(MAX_SUMMARY_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Some(text[start..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn marker(s: &str) -> Vec<u8> {
        format!("\x1b]{PREFIX}{s}\x07").into_bytes()
    }
    fn start(t: &mut ShellCommandTracker, now: Instant) {
        t.observe(&marker("enable"), now);
        t.observe(&marker("start;1;10000"), now);
    }
    #[test]
    fn shell_command_chunk_boundaries_freeze_only_current_output() {
        let now = Instant::now();
        let mut bytes = marker("enable");
        bytes.extend(b"OLD");
        bytes.extend(marker("start;1;0"));
        bytes.extend("\x1b[31m中文42\x1b[0m\r\n".as_bytes());
        bytes.extend(marker("end;1;7"));
        bytes.extend(b"NEXT");
        for split in 0..=bytes.len() {
            let mut tracker = ShellCommandTracker::default();
            let mut result = tracker.observe(&bytes[..split], now);
            result.extend(tracker.observe(&bytes[split..], now));
            assert_eq!(result.len(), 1, "split={split}");
            let result = result.pop().unwrap().finish();
            assert_eq!(result.output.as_deref(), Some("中文42"), "split={split}");
            assert_eq!(result.exit_code, 7);
        }
    }
    #[test]
    fn shell_command_large_summary_preserves_final_unicode_result() {
        let now = Instant::now();
        let mut tracker = ShellCommandTracker::default();
        start(&mut tracker, now);
        let mut output = "中文".repeat(MAX_TAIL).into_bytes();
        output.extend("\r\n最终输出：中文42\r\n".as_bytes());
        tracker.observe(&output, now);
        let completion = tracker
            .observe(&marker("end;1;0"), now + Duration::from_secs(11))
            .pop()
            .unwrap()
            .finish();
        let summary = completion.output.unwrap();
        assert!(summary.len() <= MAX_SUMMARY_BYTES);
        assert!(summary.ends_with("最终输出：中文42"));
        assert!(!summary.contains('\u{fffd}'));
    }

    #[test]
    fn shell_command_tail_is_bounded_and_cancel_drops_completion() {
        let now = Instant::now();
        let mut t = ShellCommandTracker::default();
        start(&mut t, now);
        t.observe(&vec![b'x'; MAX_TAIL * 4], now);
        assert_eq!(t.running.as_ref().unwrap().tail.len(), MAX_TAIL);
        t.cancel();
        assert!(t
            .observe(&marker("end;1;0"), now + Duration::from_secs(11))
            .is_empty());
        assert!(t.running.is_none());
    }
    #[test]
    fn shell_command_threshold_empty_duplicate_and_disabled() {
        let now = Instant::now();
        let mut t = ShellCommandTracker::default();
        start(&mut t, now);
        let completed = t.observe(&marker("end;1;0"), now + Duration::from_secs(10));
        assert_eq!(completed[0].clone().finish().output, None);
        assert!(t.observe(&marker("end;1;0"), now).is_empty());
        t.observe(&marker("start;2;10000"), now);
        assert!(t.observe(&marker("end;2;0"), now).is_empty());
        t.observe(&marker("start;3;0"), now);
        t.observe(&marker("disable"), now);
        assert!(t.observe(&marker("end;3;0"), now).is_empty());
    }
    #[test]
    fn shell_command_alternate_screen_and_mismatched_boundaries_fail_closed() {
        let now = Instant::now();
        for split in 0..=8 {
            let mut t = ShellCommandTracker::default();
            start(&mut t, now);
            let alt = b"\x1b[?1049h\x1b[?1049l";
            t.observe(&alt[..split], now);
            t.observe(&alt[split..], now);
            assert!(t
                .observe(&marker("end;1;0"), now + Duration::from_secs(20))
                .is_empty());
        }
        let mut t = ShellCommandTracker::default();
        start(&mut t, now);
        assert!(t
            .observe(&marker("end;2;0"), now + Duration::from_secs(20))
            .is_empty());
        assert!(t
            .observe(&marker("end;1;0"), now + Duration::from_secs(20))
            .is_empty());
    }
    #[test]
    fn shell_command_truncated_control_string_and_completion_count_are_bounded() {
        let now = Instant::now();
        let mut t = ShellCommandTracker::default();
        start(&mut t, now);
        let mut output = b"\x1b]52;".to_vec();
        output.extend(vec![b'S'; MAX_TAIL * 2]);
        output.extend(b"\x07visible");
        t.observe(&output, now);
        let completed = t.observe(&marker("end;1;0"), now + Duration::from_secs(10));
        assert_eq!(
            completed[0].clone().finish().output.as_deref(),
            Some("visible")
        );
        let mut bytes = Vec::new();
        for seq in 2..100 {
            bytes.extend(marker(&format!("start;{seq};0")));
            bytes.extend(marker(&format!("end;{seq};0")));
        }
        assert_eq!(t.observe(&bytes, now).len(), MAX_COMPLETIONS_PER_CHUNK);
    }

    #[test]
    fn shell_command_non_osc_strings_keep_bel_and_fake_screen_modes_hidden() {
        let now = Instant::now();
        for intro in [b'P', b'_', b'^', b'X'] {
            for padding in [0, MAX_TAIL * 2] {
                let mut output = vec![0x1b, intro];
                output.extend(vec![b'i'; padding]);
                output.extend(b"ignored\x07SECRET\x1b[?1049h\x1b\\visible");
                for split in [0, 2, output.len() / 2, output.len() - 7, output.len()] {
                    let mut t = ShellCommandTracker::default();
                    start(&mut t, now);
                    t.observe(&output[..split], now);
                    t.observe(&output[split..], now);
                    let completed = t.observe(&marker("end;1;0"), now + Duration::from_secs(10));
                    assert_eq!(
                        completed.len(),
                        1,
                        "intro={intro} padding={padding} split={split}"
                    );
                    assert_eq!(
                        completed[0].clone().finish().output.as_deref(),
                        Some("visible")
                    );
                    assert!(!t.alternate_screen);
                }
            }
        }
    }

    #[test]
    fn shell_command_screen_tracking_skips_plain_spans_and_initializes_once() {
        let now = Instant::now();
        let plain = vec![b'x'; MAX_TAIL * 2];
        let mut t = ShellCommandTracker::default();
        assert!(t.needs_screen_initialization());
        t.initialize_screen(true);
        assert!(!t.needs_screen_initialization());
        t.initialize_screen(false);
        assert!(t.alternate_screen);

        t.observe(&plain, now);
        assert_eq!(t.screen_control_steps, 0);
        // A mode transition still works when split across otherwise plain chunks.
        t.observe(b"\x1b[?104", now);
        t.observe(b"9l", now);
        assert!(!t.alternate_screen);
        t.observe(&marker("enable"), now);
        let before = t.screen_control_steps;
        t.observe(&plain, now);
        assert_eq!(t.screen_control_steps, before);
        assert!(t.running.is_none());

        // Control-string bodies also take the span fast path; BEL does not end DCS.
        t.observe(b"\x1bP", now);
        let before = t.screen_control_steps;
        t.observe(&plain, now);
        t.observe(b"\x07", now);
        assert_eq!(t.screen_control_steps, before);
        t.observe(b"\x1b\\", now);
        t.observe(b"\x1b]", now);
        let before = t.screen_control_steps;
        t.observe(&plain, now);
        assert_eq!(t.screen_control_steps, before);
        t.observe(b"\x07", now);
        assert_eq!(t.escape_state, 0);
    }

    #[test]
    fn shell_command_screen_transitions_precede_boundaries_in_stream_order() {
        let now = Instant::now();
        for (initial, transition, expected) in [
            (false, b"\x1b[?1049h".as_slice(), 0),
            (true, b"\x1b[?1049l".as_slice(), 1),
        ] {
            let mut stream = transition.to_vec();
            stream.extend(marker("enable"));
            stream.extend(marker("start;1;0"));
            stream.extend(b"42");
            stream.extend(marker("end;1;0"));
            for split in 0..=stream.len() {
                let mut t = ShellCommandTracker::default();
                t.initialize_screen(initial);
                let mut completed = t.observe(&stream[..split], now);
                // A stale terminal-mode observation must not overwrite partial CSI state.
                t.initialize_screen(initial);
                completed.extend(t.observe(&stream[split..], now));
                assert_eq!(completed.len(), expected, "initial={initial} split={split}");
            }
        }
    }

    #[test]
    fn shell_command_independent_terminals_and_consecutive_empty_command() {
        let now = Instant::now();
        let mut left = ShellCommandTracker::default();
        let mut right = ShellCommandTracker::default();
        start(&mut left, now);
        start(&mut right, now);
        left.observe(b"LEFT\x1b[2J\x1b[H42", now);
        right.observe(b"RIGHT", now);
        let end = now + Duration::from_secs(10);
        let a = left
            .observe(&marker("end;1;0"), end)
            .pop()
            .unwrap()
            .finish();
        let b = right
            .observe(&marker("end;1;7"), end)
            .pop()
            .unwrap()
            .finish();
        assert_eq!(a.output.as_deref(), Some("LEFT42"));
        assert_eq!(b.output.as_deref(), Some("RIGHT"));
        assert_ne!(a.generation, b.generation);
        left.observe(&marker("start;2;0"), end);
        let empty = left
            .observe(&marker("end;2;0"), end)
            .pop()
            .unwrap()
            .finish();
        assert_eq!(empty.output, None);
    }

    #[test]
    fn shell_command_progress_and_sequence_survive_reenable() {
        let now = Instant::now();
        let mut t = ShellCommandTracker::default();
        for expected in 1..=2 {
            start(&mut t, now);
            t.observe(b"10%\r100%\r\n42\r\n", now);
            let c = t
                .observe(&marker("end;1;0"), now + Duration::from_secs(10))
                .pop()
                .unwrap()
                .finish();
            assert_eq!(c.sequence, expected);
            assert_eq!(c.output.as_deref(), Some("100%\n42"));
        }
    }
}
