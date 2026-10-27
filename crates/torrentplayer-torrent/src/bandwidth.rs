// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sharing the download link between the video being watched and the rest of the play queue.
//!
//! [`LinkMeter`] estimates what the connection can carry from what all torrents together have
//! delivered. [`Planner`] decides, about once a second, which queued items download in the
//! background and how fast:
//!
//! - **Everything downloads.** Every queued item that is not paused or finished downloads at
//!   once, unless the user chose a maximum (then the highest-ranked ones do).
//! - **Playback first, by bandwidth.** While the watched video's buffer is short, background
//!   torrents share only what is left of the link after its playback rate (plus a margin);
//!   higher-ranked torrents get bigger shares. When the buffer runs low and playback is not
//!   keeping up, background torrents pause (their connections stay open) until it recovers.
//!   Once the buffer is comfortable, nothing is limited, which also measures the link's real
//!   capacity.
//! - **Order.** The item after the current one ranks first, so it plays at once; then the rest
//!   of the queue after the current item; items before it (played or passed over) last. Within
//!   that, queue order: only the user reorders the queue.
//! - **No churn.** With a maximum, an item that started keeps downloading for [`MIN_RUN`]
//!   unless playback needs the bandwidth, so speeds are measured over a meaningful time.
//!
//! Everything here is pure: the app feeds measurements in and applies the [`Plan`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

/// Least background bandwidth in total while it is limited, bytes per second.
pub const MIN_BACKGROUND_BPS: u64 = 64 * 1024;
/// Least share of one background torrent while limited, so its swarm stays connected and its
/// speed measurable.
pub const MIN_SHARE_BPS: u64 = 16 * 1024;
/// Media rate assumed when the watched video's bitrate is unknown (about 8 Mbit/s).
pub const FALLBACK_NEED_BPS: u64 = 1024 * 1024;
/// With a maximum, a started item keeps downloading at least this long (unless playback needs
/// the bandwidth).
pub const MIN_RUN: Duration = Duration::from_secs(20);

/// Buffer at which background downloads run unlimited, and below which they are limited again
/// (hysteresis), in seconds of media.
const RELAX_AT_SECS: f64 = 45.0;
const GUARD_BELOW_SECS: f64 = 25.0;
/// Below this buffer, if playback is also downloading slower than it plays, it is starving.
const STARVING_BELOW_SECS: f64 = 10.0;
/// Playback's need is its media rate times this, to refill what was played and absorb jitter.
const NEED_MARGIN: f64 = 1.5;
/// Starving playback pauses the background for at most this long, to see whether that helps.
const PAUSE_TRIAL: Duration = Duration::from_secs(15);
/// A pause ends sooner if the link carries under half its capacity this long into it: the
/// bandwidth it freed is not being used.
const PAUSE_IDLE_AFTER: Duration = Duration::from_secs(5);
/// After a pause that did not help, pausing is not tried again for this long.
const PAUSE_RETRY_AFTER: Duration = Duration::from_secs(300);
/// How long the meter remembers samples for its peak, and how much of a 3-second average
/// counts as a sample.
const PEAK_WINDOW: Duration = Duration::from_secs(60);
const AVERAGE_OVER: usize = 3;
/// A remembered capacity halves over this long without being reached again.
const CAPACITY_HALF_LIFE: Duration = Duration::from_secs(300);

/// Estimates the link's capacity from the total download rate of all torrents.
#[derive(Debug, Clone)]
pub struct LinkMeter {
    samples: VecDeque<(Instant, u64)>,
    capacity: f64,
    capacity_at: Option<Instant>,
    /// The user's own download limit, which caps the estimate.
    cap: Option<u64>,
}

impl Default for LinkMeter {
    fn default() -> Self {
        Self::new(None)
    }
}

impl LinkMeter {
    /// A meter that knows nothing yet; `cap` is the user's download limit, if any.
    #[must_use]
    pub fn new(cap: Option<u64>) -> Self {
        Self {
            samples: VecDeque::new(),
            capacity: 0.0,
            capacity_at: None,
            cap,
        }
    }

    /// Sets the user's download limit, which caps the estimate (`None`: no limit).
    pub fn set_cap(&mut self, cap: Option<u64>) {
        self.cap = cap;
    }

    /// Records the total download rate (bytes per second) at `now`; call about once a second.
    pub fn update(&mut self, now: Instant, total_bps: u64) {
        self.samples.push_back((now, total_bps));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > PEAK_WINDOW)
        {
            self.samples.pop_front();
        }
        // The remembered capacity decays, so a link that got slower is believed eventually.
        if let Some(at) = self.capacity_at {
            let age = now.saturating_duration_since(at).as_secs_f64();
            self.capacity *= 0.5f64.powf(age / CAPACITY_HALF_LIFE.as_secs_f64());
        }
        self.capacity = self.capacity.max(self.peak());
        self.capacity_at = Some(now);
    }

    /// The highest 3-sample average in the window.
    #[allow(clippy::cast_precision_loss)]
    fn peak(&self) -> f64 {
        let rates: Vec<f64> = self.samples.iter().map(|(_, r)| *r as f64).collect();
        rates
            .windows(AVERAGE_OVER.min(rates.len()).max(1))
            .map(|w| w.iter().sum::<f64>() / w.len() as f64)
            .fold(0.0, f64::max)
    }

    /// The recent total rate: the average of the last few samples.
    #[must_use]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    pub fn current_bps(&self) -> u64 {
        let n = self.samples.len().min(AVERAGE_OVER);
        if n == 0 {
            return 0;
        }
        let sum: u64 = self.samples.iter().rev().take(n).map(|(_, r)| *r).sum();
        sum / n as u64
    }

    /// The estimated capacity, bytes per second; 0 until something was downloaded.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn capacity_bps(&self) -> u64 {
        let measured = self.capacity.max(0.0) as u64;
        self.cap.map_or(measured, |cap| measured.min(cap))
    }
}

/// The video being watched.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Foreground {
    /// Its media rate, bytes per second, if known.
    pub byte_rate: Option<u64>,
    /// Seconds of media downloaded ahead of the playhead, if known.
    pub buffer_secs: Option<f64>,
    /// Its torrent's download rate, bytes per second.
    pub download_bps: u64,
    /// The whole file is downloaded: it needs nothing more.
    pub complete: bool,
}

impl Foreground {
    /// The rate playback needs, with margin, bytes per second.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn need_bps(&self) -> u64 {
        if self.complete {
            return 0;
        }
        (self.byte_rate.unwrap_or(FALLBACK_NEED_BPS) as f64 * NEED_MARGIN) as u64
    }
}

/// A queued item that could download in the background.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The queue item's id.
    pub id: String,
    /// Its torrent (items of one torrent share it): the info hash, or the link.
    pub group: String,
    /// Position in the queue.
    pub position: usize,
    /// The item after the one being watched: always first.
    pub up_next: bool,
    /// Before the current item in the queue (played or passed over): last.
    pub behind: bool,
}

/// How playback is doing, which decides how much the background may take.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Pressure {
    /// Nothing is playing, or its buffer is comfortable: background downloads are unlimited.
    #[default]
    Relaxed,
    /// The buffer is short: the background gets what playback leaves.
    Guarded,
    /// The buffer is low and playback downloads slower than it plays: background torrents
    /// pause for a while to see whether that helps, and are limited as when guarded otherwise.
    Starving,
}

/// What to do until the next plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Items to download in the background, in priority order.
    pub active: Vec<String>,
    /// Each background torrent's limit (by [`Candidate::group`]), bytes per second; torrents
    /// not listed are unlimited.
    pub limits: HashMap<String, u64>,
    /// Pause every background torrent (playback is starving, and pausing may help it).
    pub pause_background: bool,
    /// How playback is doing.
    pub pressure: Pressure,
}

/// Decides the background downloads; keeps the state that makes decisions stable.
#[derive(Debug, Clone, Default)]
pub struct Planner {
    /// The link's capacity estimate.
    pub meter: LinkMeter,
    pressure: Pressure,
    /// When each active item started.
    started: HashMap<String, Instant>,
    /// When the current pause of the background began.
    paused_since: Option<Instant>,
    /// A pause did not help: no pausing until then.
    no_pause_until: Option<Instant>,
}

impl Planner {
    /// Plans the next second. `total_bps` is every torrent's download rate added up;
    /// `candidates` are the items that may download (not paused, not complete, not watched);
    /// `max_active` is the user's maximum of items downloading at once (`None`: no limit).
    pub fn plan(
        &mut self,
        now: Instant,
        total_bps: u64,
        foreground: Option<Foreground>,
        candidates: &[Candidate],
        max_active: Option<usize>,
    ) -> Plan {
        self.meter.update(now, total_bps);
        self.pressure = next_pressure(self.pressure, foreground);

        let ranked = rank(candidates);
        let mut active: Vec<String> = ranked
            .iter()
            .take(max_active.unwrap_or(usize::MAX))
            .map(|c| c.id.clone())
            .collect();
        // With a maximum, items that started recently keep running, unless playback starves.
        if max_active.is_some() && self.pressure != Pressure::Starving {
            for c in &ranked {
                let recent = self
                    .started
                    .get(&c.id)
                    .is_some_and(|t| now.saturating_duration_since(*t) < MIN_RUN);
                if recent && !active.contains(&c.id) {
                    active.push(c.id.clone());
                }
            }
        }
        let keep: HashSet<&String> = active.iter().collect();
        self.started.retain(|id, _| keep.contains(id));
        for id in &active {
            self.started.entry(id.clone()).or_insert(now);
        }

        // Background torrents in priority order (a torrent ranks by its best item).
        let mut groups: Vec<&str> = Vec::new();
        for id in &active {
            if let Some(c) = candidates.iter().find(|c| &c.id == id)
                && !groups.contains(&c.group.as_str())
            {
                groups.push(c.group.as_str());
            }
        }
        Plan {
            limits: self.limits(foreground, &groups),
            pause_background: self.pause_background(now, total_bps),
            pressure: self.pressure,
            active,
        }
    }

    /// Whether to pause the background now. Starving playback pauses it as a trial: when the
    /// link is the bottleneck, playback catches up and the pressure eases. When playback still
    /// starves after the trial, or the link sits half idle during it, the video's own swarm is
    /// the limit and a pause would only stop everything else, so the background resumes
    /// (limited to what playback leaves) and pausing is not tried again for a while.
    fn pause_background(&mut self, now: Instant, total_bps: u64) -> bool {
        if self.pressure != Pressure::Starving {
            self.paused_since = None;
            return false;
        }
        if self.no_pause_until.is_some_and(|until| now < until) {
            return false;
        }
        let paused_for = now.saturating_duration_since(*self.paused_since.get_or_insert(now));
        let capacity = self.meter.capacity_bps();
        let idle = capacity > 0 && total_bps.saturating_mul(2) < capacity;
        if paused_for >= PAUSE_TRIAL || (paused_for >= PAUSE_IDLE_AFTER && idle) {
            self.paused_since = None;
            self.no_pause_until = Some(now + PAUSE_RETRY_AFTER);
            return false;
        }
        true
    }

    /// While the buffer is short (guarded, or starving without a pause), what playback leaves of
    /// the link, shared between `groups` (in priority order) in proportion 1, 1/2, 1/3, …: the
    /// top torrent gets the most.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn limits(&self, foreground: Option<Foreground>, groups: &[&str]) -> HashMap<String, u64> {
        let Some(fg) = foreground else {
            return HashMap::new();
        };
        if self.pressure == Pressure::Relaxed || groups.is_empty() {
            return HashMap::new();
        }
        let budget = self
            .meter
            .capacity_bps()
            .saturating_sub(fg.need_bps())
            .max(MIN_BACKGROUND_BPS) as f64;
        let weights: Vec<f64> = (1..=groups.len()).map(|r| 1.0 / r as f64).collect();
        let total: f64 = weights.iter().sum();
        groups
            .iter()
            .zip(weights)
            .map(|(g, w)| {
                let share = (budget * w / total) as u64;
                ((*g).to_owned(), share.max(MIN_SHARE_BPS))
            })
            .collect()
    }
}

fn next_pressure(previous: Pressure, foreground: Option<Foreground>) -> Pressure {
    let Some(fg) = foreground else {
        return Pressure::Relaxed;
    };
    if fg.complete {
        return Pressure::Relaxed;
    }
    // Without a known buffer, judge by rate alone.
    let Some(buffer) = fg.buffer_secs else {
        return if fg.download_bps >= fg.need_bps() {
            Pressure::Guarded
        } else {
            Pressure::Starving
        };
    };
    let keeping_up = fg.download_bps >= fg.byte_rate.unwrap_or(FALLBACK_NEED_BPS);
    if buffer < STARVING_BELOW_SECS && !keeping_up {
        Pressure::Starving
    } else if buffer >= RELAX_AT_SECS
        || (previous == Pressure::Relaxed && buffer >= GUARD_BELOW_SECS)
    {
        Pressure::Relaxed
    } else {
        Pressure::Guarded
    }
}

/// Candidates in priority order: up next, then the rest after the current item, then the ones
/// before it; within each, in queue order.
fn rank(candidates: &[Candidate]) -> Vec<&Candidate> {
    let mut ranked: Vec<&Candidate> = candidates.iter().collect();
    ranked.sort_by_key(|c| (!c.up_next, c.behind, c.position));
    ranked
}

#[cfg(test)]
// Rates in tests are small whole numbers of bytes.
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn candidate(id: &str, group: &str, position: usize) -> Candidate {
        Candidate {
            id: id.into(),
            group: group.into(),
            position,
            up_next: false,
            behind: false,
        }
    }

    fn many(n: usize) -> Vec<Candidate> {
        (0..n)
            .map(|i| candidate(&i.to_string(), &i.to_string(), i))
            .collect()
    }

    fn watching(buffer_secs: f64, download_bps: u64) -> Foreground {
        Foreground {
            byte_rate: Some(MIB),
            buffer_secs: Some(buffer_secs),
            download_bps,
            complete: false,
        }
    }

    fn ids(ranked: &[&Candidate]) -> Vec<String> {
        ranked.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn the_meter_remembers_a_sustained_peak_and_slowly_forgets_it() {
        let mut m = LinkMeter::default();
        let t0 = Instant::now();
        assert_eq!(m.capacity_bps(), 0);
        for (i, rate) in [10 * MIB, 10 * MIB, 10 * MIB, 2 * MIB].iter().enumerate() {
            m.update(t0 + Duration::from_secs(i as u64), *rate);
        }
        // A single spike does not count; three seconds at 10 MiB/s do.
        assert!((m.capacity_bps() as f64 - 10.0 * MIB as f64).abs() < MIB as f64 * 0.1);
        // Five minutes later (after the window), half is remembered.
        m.update(t0 + CAPACITY_HALF_LIFE + Duration::from_secs(3), 0);
        let cap = m.capacity_bps() as f64;
        assert!(cap > 4.0 * MIB as f64 && cap < 6.0 * MIB as f64, "{cap}");
        // The user's limit caps it.
        let mut capped = LinkMeter::new(Some(MIB));
        capped.update(t0, 10 * MIB);
        assert_eq!(capped.capacity_bps(), MIB);
        capped.set_cap(None);
        assert_eq!(capped.capacity_bps(), 10 * MIB);
    }

    #[test]
    fn in_order_puts_up_next_first_and_items_behind_last() {
        let mut c = vec![
            candidate("behind", "s", 0),
            candidate("a", "x", 2),
            candidate("b", "y", 3),
            candidate("next", "z", 4),
        ];
        c[0].behind = true;
        c[3].up_next = true;
        assert_eq!(ids(&rank(&c)), ["next", "a", "b", "behind"]);
    }

    #[test]
    fn everything_downloads_unless_the_user_chose_a_maximum() {
        let t0 = Instant::now();
        let c = many(12);
        let mut p = Planner::default();
        let plan = p.plan(t0, MIB, None, &c, None);
        assert_eq!(plan.active.len(), 12);
        assert!(plan.limits.is_empty());
        assert!(!plan.pause_background);

        let mut capped = Planner::default();
        let plan = capped.plan(t0, MIB, None, &c, Some(3));
        assert_eq!(plan.active, ["0", "1", "2"]);
    }

    #[test]
    fn a_short_buffer_shares_what_playback_leaves_by_rank() {
        let mut p = Planner::default();
        let t0 = Instant::now();
        // Learn a 10 MiB/s link.
        for i in 0..3 {
            p.meter.update(t0 + Duration::from_secs(i), 10 * MIB);
        }
        let c = many(2);
        let plan = p.plan(
            t0 + Duration::from_secs(3),
            10 * MIB,
            Some(watching(15.0, 2 * MIB)),
            &c,
            None,
        );
        assert_eq!(plan.pressure, Pressure::Guarded);
        assert_eq!(plan.active.len(), 2, "nothing stops");
        // 10 MiB/s minus 1.5 MiB/s for playback, shared 2 : 1.
        let budget = (10 * MIB - MIB * 3 / 2) as f64;
        let first = plan.limits["0"] as f64;
        let second = plan.limits["1"] as f64;
        assert!((first - budget * 2.0 / 3.0).abs() < 2.0, "{first}");
        assert!((second - budget / 3.0).abs() < 2.0, "{second}");
        assert!(!plan.pause_background);
    }

    #[test]
    fn starving_playback_pauses_the_background_and_a_comfortable_one_lifts_limits() {
        let mut p = Planner::default();
        let t0 = Instant::now();
        let c = many(5);
        let plan = p.plan(t0, MIB, Some(watching(3.0, MIB / 2)), &c, None);
        assert_eq!(plan.pressure, Pressure::Starving);
        assert!(plan.pause_background);
        assert_eq!(plan.active.len(), 5, "still active, only paused");

        let plan = p.plan(
            t0 + Duration::from_secs(1),
            MIB,
            Some(watching(60.0, 2 * MIB)),
            &c,
            None,
        );
        assert_eq!(plan.pressure, Pressure::Relaxed);
        assert!(plan.limits.is_empty());
        assert!(!plan.pause_background);
    }

    #[test]
    fn a_pause_that_does_not_help_ends_and_is_not_retried_soon() {
        let mut p = Planner::default();
        let t0 = Instant::now();
        let c = many(3);
        let starving = Some(watching(3.0, MIB / 2));
        let mut plan_at = |s: u64| p.plan(t0 + Duration::from_secs(s), MIB / 2, starving, &c, None);
        // The link's capacity is unknown, so the pause gets its whole trial.
        assert!(plan_at(0).pause_background);
        assert!(plan_at(10).pause_background);
        // Still starving after it: the video's own swarm is the limit. The background resumes,
        // limited to what playback leaves.
        let plan = plan_at(15);
        assert_eq!(plan.pressure, Pressure::Starving);
        assert!(!plan.pause_background);
        assert_eq!(plan.limits.len(), 3);
        // Pausing is tried again only after a while.
        assert!(!plan_at(200).pause_background);
        assert!(plan_at(15 + 300).pause_background);
    }

    #[test]
    fn a_pause_ends_early_when_the_link_sits_idle() {
        let mut p = Planner::default();
        let t0 = Instant::now();
        // Learn a 10 MiB/s link.
        for i in 0..3 {
            p.meter.update(t0 + Duration::from_secs(i), 10 * MIB);
        }
        let c = many(2);
        let starving = Some(watching(3.0, MIB / 2));
        let mut plan_at =
            |s: u64| p.plan(t0 + Duration::from_secs(3 + s), MIB / 2, starving, &c, None);
        assert!(plan_at(0).pause_background);
        // Five seconds in, the link carries a twentieth of what it can: the pause frees
        // bandwidth nothing uses.
        assert!(!plan_at(5).pause_background);
    }

    #[test]
    fn pressure_has_hysteresis() {
        let fg = |b| Some(watching(b, 2 * MIB));
        assert_eq!(
            next_pressure(Pressure::Guarded, fg(30.0)),
            Pressure::Guarded
        );
        assert_eq!(
            next_pressure(Pressure::Guarded, fg(50.0)),
            Pressure::Relaxed
        );
        assert_eq!(
            next_pressure(Pressure::Relaxed, fg(30.0)),
            Pressure::Relaxed
        );
        assert_eq!(
            next_pressure(Pressure::Relaxed, fg(20.0)),
            Pressure::Guarded
        );
        // A low buffer that is refilling quickly is not starving.
        assert_eq!(next_pressure(Pressure::Guarded, fg(5.0)), Pressure::Guarded);
        assert_eq!(
            next_pressure(Pressure::Guarded, Some(watching(5.0, MIB / 2))),
            Pressure::Starving
        );
        // A fully downloaded video needs nothing.
        let mut done = watching(1.0, 0);
        done.complete = true;
        assert_eq!(
            next_pressure(Pressure::Starving, Some(done)),
            Pressure::Relaxed
        );
    }

    #[test]
    fn with_a_maximum_started_items_keep_running_for_a_while() {
        let mut p = Planner::default();
        let t0 = Instant::now();
        let c = [candidate("a", "x", 1), candidate("b", "y", 0)];
        let first = p.plan(t0, 0, None, &c[..1], Some(1));
        assert_eq!(first.active, ["a"]);
        // An item earlier in the queue appears: `a` keeps running beside it for its minimum run.
        let next = p.plan(t0 + Duration::from_secs(1), 0, None, &c, Some(1));
        assert_eq!(next.active, ["b", "a"]);
        let later = p.plan(t0 + MIN_RUN + Duration::from_secs(2), 0, None, &c, Some(1));
        // Its minimum run over, the lower-ranked item gives way.
        assert_eq!(later.active, ["b"]);
    }
}
