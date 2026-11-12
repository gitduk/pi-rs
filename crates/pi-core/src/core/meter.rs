//! What a run spent, and the readings only its events can give.
//!
//! Nothing here draws: the tally is one run's share of the wire's meter, and
//! the snapshot is what a surface reads once it is time to say it.

use pi_store::icons;

use std::sync::Arc;
use std::time::Duration;

use llm::meter::Meter;
use llm::stream::Usage;
use llm::totals::Totals;

/// Every value a status line can draw on, as far as it is known right now:
/// the run in flight's own figures, not the session's.
///
/// A zero count reads as a dash (the provider stated nothing); every other
/// zero drops its part. Owned, not borrowed, so it outlives the lane.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Snapshot {
    pub elapsed: Option<Duration>,
    pub input: u64,
    pub output: u64,
    /// Percent of the prompt tokens this run sent that were read from cache,
    /// floored; `None` until a request of the run has reported its prompt.
    pub cached: Option<u64>,
    /// In dollars. Zero is an unpriced model rather than a free run.
    pub cost: f64,
    /// What the run's last request occupied against the model's window, as
    /// the provider counted it; `None` until one is counted.
    pub ctx: Option<(usize, usize)>,
    /// Output tokens a second, over the time the model spent streaming them;
    /// `None` until one turn of the run has streamed anything.
    pub speed: Option<f64>,
    pub compactions: usize,
    pub queued: usize,
    /// The model as its endpoint names it, as a line draws it.
    pub model_name: String,
    /// The worktree the session is working in, or None in the repository's own
    /// checkout.
    pub worktree: Option<String>,
}

/// What a run spent, read off the one counter every call passes: the wire's
/// meter, minus what it held when the run began. The session's figure is the
/// same meter undiminished, so the two can never disagree.
#[derive(Debug, Default, Clone)]
pub struct Tally {
    // The meter every call is charged to, and what it read when this run
    // began: what the run spent is the difference.
    meter: Arc<Meter>,
    base: Totals,
    ctx: Option<(usize, usize)>,
    compactions: usize,
    // The run's warnings, kept whole for `/status`.
    compacted: Option<agent::Report>,
    refits: Vec<String>,
    gaps: Vec<String>,
    // Output tokens of the turns that streamed, and the time they streamed
    // for: the run's speed, not skewed by waits or tool calls between.
    generated: (u64, Duration),
    // What the run that last ended reported, and whether this one has said
    // anything yet: a status line reads the run in flight, or the one that
    // last ended.
    ended: pi_store::statusline::Shown,
    reported: bool,
}

impl Tally {
    /// Start a run's counts afresh: what it spends is what the meter gains
    /// from here on, each call priced at the model it actually went to.
    pub fn seed(&mut self, meter: Arc<Meter>) {
        let ended = self.reading(&self.run_spend());
        // Read after the run's own figure is taken: the meter may already hold
        // the session's earlier calls.
        let base = meter.read();
        *self = Self {
            meter,
            base,
            ended,
            ..Self::default()
        };
    }

    /// Calls that are no run of their own — a compaction asked for between
    /// turns — are session spend: count them past, without putting them on
    /// the last run's row.
    pub fn pass_by(&mut self) {
        self.base = self.meter.read();
    }

    /// Start as a session that has run before: the numbers its row last
    /// showed, until this run says its own.
    pub fn kept(shown: Option<pi_store::statusline::Shown>) -> Self {
        Self {
            ended: shown.unwrap_or_default(),
            ..Self::default()
        }
    }

    /// Read one event for what the meter cannot say: where the context stands,
    /// how many compactions, how fast a turn streamed.
    pub fn on(&mut self, event: &agent::Event) {
        match event {
            agent::Event::Usage(_) => self.reported = true,
            agent::Event::TurnEnd {
                usage, generating, ..
            } => {
                self.reported = true;
                if let Some(took) = generating {
                    self.generated.0 += usage.output;
                    self.generated.1 += *took;
                }
            }
            agent::Event::Context { used, window } => {
                self.reported = true;
                self.ctx = Some((*used, *window));
            }
            agent::Event::Compacted(report) => {
                self.compactions += 1;
                self.compacted = Some(*report);
                // The last count is of a transcript that is no longer there;
                // only the next request's count says what this one occupies.
                self.ctx = None;
            }
            agent::Event::Refit(why) => self.refits.push(why.clone()),
            agent::Event::Warning(gap) => self.gaps.push(gap.clone()),
            agent::Event::Done {
                ctx, compactions, ..
            } => {
                self.reported = true;
                if ctx.is_some() {
                    self.ctx = *ctx;
                }
                self.compactions = *compactions;
            }
            _ => {}
        }
    }

    /// The one place a snapshot is built. What the events cannot say is asked
    /// for here: a one-shot run has no clock and no queue, and neither is a
    /// number the run reports.
    pub fn snapshot(
        &self,
        model_name: &str,
        worktree: Option<&str>,
        elapsed: Option<Duration>,
        queued: usize,
    ) -> Snapshot {
        let run = self.run_spend();
        let reading = self.reading(&run);
        Snapshot {
            elapsed,
            input: run.usage.input,
            output: run.usage.output,
            cached: reading.cached,
            cost: reading.cost,
            ctx: reading.ctx,
            speed: self.speed(),
            compactions: self.compactions,
            queued,
            model_name: model_name.to_string(),
            worktree: worktree.map(str::to_string),
        }
    }

    // Output tokens a second over the time spent streaming them.
    fn speed(&self) -> Option<f64> {
        let (tokens, took) = self.generated;
        (!took.is_zero()).then(|| tokens as f64 / took.as_secs_f64())
    }

    /// The last compaction of this run, or of a `/compact` since.
    pub fn compacted(&self) -> Option<&agent::Report> {
        self.compacted.as_ref()
    }

    /// Why this run resent smaller, in the order it happened.
    pub fn refits(&self) -> &[String] {
        &self.refits
    }

    /// What the endpoint left out of this run's replies.
    pub fn gaps(&self) -> &[String] {
        &self.gaps
    }

    /// The context used against the window, as the last event said.
    pub fn ctx(&self) -> Option<(usize, usize)> {
        self.ctx
    }

    // What a row should show: this run's numbers once it has any, else the ones
    // the run before left — a `!` spends nothing, a first request lands later.
    fn reading(&self, run: &Totals) -> pi_store::statusline::Shown {
        match self.reported || *run != Totals::default() {
            true => pi_store::statusline::Shown {
                cached: cached(&run.usage),
                cost: run.cost,
                ctx: self.ctx,
            },
            false => self.ended.clone(),
        }
    }

    // What this run has spent so far: the meter, less what it held at seed.
    fn run_spend(&self) -> Totals {
        self.meter.read().since(&self.base)
    }
}

// Over the same sums `input` and the cost are, so the three agree.
fn cached(u: &Usage) -> Option<u64> {
    let sent = u.prompt();
    (sent > 0).then(|| u.cache_read * 100 / sent)
}

/// A compaction's report in one line, for `/status` and the surfaces that
/// print it as it happens.
pub fn compaction_line(r: &agent::Report) -> String {
    let mut parts = Vec::new();
    if r.superseded > 0 {
        parts.push(format!("{} superseded", r.superseded));
    }
    if r.aged_out > 0 {
        parts.push(format!("{} aged out", r.aged_out));
    }
    if r.args_taken > 0 {
        parts.push(format!("{} arguments taken", r.args_taken));
    }

    if r.notices_pruned > 0 {
        parts.push(format!("{} notices pruned", r.notices_pruned));
    }
    if r.dropped > 0 {
        let how = if r.summarized {
            "summarized"
        } else {
            "dropped"
        };
        parts.push(format!("{} messages {how}", r.dropped));
    }
    let detail = if parts.is_empty() {
        String::new()
    } else {
        format!("{}{}", icons::PART_SEP, parts.join(", "))
    };
    let warn = if r.still_over {
        format!("{}still over budget", icons::PART_SEP)
    } else {
        String::new()
    };
    format!("compacted{detail}{warn}")
}

/// What a list's own fields are widened to, so one column of prices lines up
/// under another.
#[derive(Default)]
pub(super) struct Widths {
    format: usize,
    window: usize,
}

// Takes the three pieces rather than a config entry: the running model may
// never have had one — a passed-through name has no entry to read.
pub(super) fn summary(
    format: &str,
    window: u32,
    p: &llm::model::Pricing,
    widths: &Widths,
) -> String {
    let size = llm::figures::size(u64::from(window));
    let mut parts = vec![
        pi_store::text::pad(format, widths.format),
        pi_store::text::pad(&size, widths.window),
    ];
    if p.input_per_mtok > 0.0 || p.output_per_mtok > 0.0 {
        parts.push(format!(
            "${:.2}/${:.2} per Mtok",
            p.input_per_mtok, p.output_per_mtok
        ));
    }
    parts.join(icons::PART_SEP)
}

// The width each field is widened to: the widest of them in the list.
pub(super) fn column_widths<'a>(items: impl Iterator<Item = (&'a str, u32)>) -> Widths {
    let mut widths = Widths::default();
    for (format, window) in items {
        let size = llm::figures::size(u64::from(window));
        widths.format = widths.format.max(pi_store::text::width(format));
        widths.window = widths.window.max(pi_store::text::width(&size));
    }
    widths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Default::default()
        }
    }

    // Round numbers, so the arithmetic an assertion checks is checkable by
    // eye: a token in costs 10 per million, one out 100.
    fn priced() -> llm::model::Pricing {
        llm::model::Pricing {
            input_per_mtok: 10.0,
            output_per_mtok: 100.0,
            ..Default::default()
        }
    }

    // A meter and a tally that started on it, as a run does.
    fn seeded() -> (Arc<Meter>, Tally) {
        let meter = Arc::new(Meter::default());
        let mut t = Tally::default();
        t.seed(meter.clone());
        (meter, t)
    }

    // What one call leaves on the meter: the wire charges every call this way,
    // and it, not the events, is what a run's numbers are read off.
    fn charge(meter: &Meter, usage: &Usage) {
        let mut totals = meter.read();
        totals.add(usage, priced().cost(usage));
        meter.reset(totals);
    }

    // The run's numbers are the meter's, less what it held at seed: a call
    // counted once, however many events describe it.
    #[test]
    fn a_tally_reads_the_meter_since_the_run_began() {
        let (meter, mut t) = seeded();
        t.on(&agent::Event::TurnStart { turn: 1 });
        charge(&meter, &usage(100, 5));
        charge(&meter, &usage(100, 20));
        let mid = t.snapshot("m", None, None, 0);
        assert_eq!((mid.input, mid.output), (200, 25));

        t.on(&agent::Event::TurnEnd {
            usage: usage(100, 30),
            text: String::new(),
            generating: Some(Duration::from_secs(2)),
        });
        charge(&meter, &usage(400, 7));
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output), (600, 32));
        // 600 in at 10/mtok, 32 out at 100/mtok.
        assert!((s.cost - 0.0092).abs() < 1e-9, "cost was {}", s.cost);
        // Speed is the finished turn's: 30 out over its 2s of streaming.
        assert_eq!(s.speed, Some(15.0));
    }

    // The session's figure and the run's are one count: what the meter held
    // before the run is all that separates them.
    #[test]
    fn the_session_total_is_the_same_meter_undiminished() {
        let meter = Arc::new(Meter::default());
        charge(&meter, &usage(1_000, 100));
        let mut t = Tally::default();
        t.seed(meter.clone());
        charge(&meter, &usage(2_000, 200));
        let run = t.snapshot("m", None, None, 0);
        assert_eq!((run.input, run.output), (2_000, 200));
        assert_eq!(meter.read().usage.output, 300);
    }

    // The cache share is weighted over the run's requests, not the latest
    // one's, and a mid-stream report of output alone leaves it be.
    #[test]
    fn the_cache_share_is_the_runs() {
        let (meter, mut t) = seeded();
        assert_eq!(t.snapshot("m", None, None, 0).cached, None);
        let hit = |input, cache_read| Usage {
            input,
            cache_read,
            ..Default::default()
        };
        charge(&meter, &hit(1_000, 0));
        charge(&meter, &hit(51, 27_800));
        // 27,800 of 28,851 prompt tokens.
        assert_eq!(t.snapshot("m", None, None, 0).cached, Some(96));
        t.on(&agent::Event::Usage(usage(0, 12)));
        assert_eq!(t.snapshot("m", None, None, 0).cached, Some(96));
    }

    // A run's own total is the meter's word too: the events only say where the
    // context stands and how many compactions there were.
    #[test]
    fn a_finished_run_leaves_the_count_to_the_meter() {
        let (meter, mut t) = seeded();
        t.on(&agent::Event::TurnStart { turn: 1 });
        charge(&meter, &usage(8_400, 390));
        t.on(&agent::Event::TurnEnd {
            usage: usage(8_400, 390),
            text: String::new(),
            generating: None,
        });
        t.on(&agent::Event::Done {
            turns: 2,
            usage: usage(8_400, 390),
            ctx: Some((72_400, 114_000)),
            compactions: 1,
        });
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output), (8_400, 390));
        assert!((s.cost - 0.123).abs() < 1e-9, "cost was {}", s.cost);
        assert_eq!(s.ctx, Some((72_400, 114_000)));
        assert_eq!(s.compactions, 1);
        // Nothing streamed, so no time to divide by.
        assert_eq!(s.speed, None);
    }

    // A `!` command and a compaction begin no turn, and a host that reports
    // nothing has none to report: nothing may mint counts out of either.
    #[test]
    fn no_event_mints_counts() {
        let quiet = Tally::default().snapshot("m", None, None, 0);
        assert_eq!((quiet.input, quiet.output), (0, 0));

        let (_meter, mut t) = seeded();
        t.on(&agent::Event::TurnStart { turn: 1 });
        let started = t.snapshot("m", None, None, 0);
        assert_eq!((started.input, started.output), (0, 0));

        t.on(&agent::Event::Done {
            turns: 3,
            usage: Usage::default(),
            ctx: None,
            compactions: 0,
        });
        let s = t.snapshot("m", None, None, 0);
        assert_eq!((s.input, s.output), (0, 0));
    }

    // Every field the events can fill is filled here; the rest (the
    // surface's own) has nowhere else to be patched in afterwards.
    #[test]
    fn a_snapshot_asks_for_what_no_event_states() {
        let s = Tally::default().snapshot("sonnet", Some("f1"), Some(Duration::from_secs(3)), 2);
        assert_eq!(s.model_name, "sonnet");
        assert_eq!(s.worktree.as_deref(), Some("f1"));
        assert_eq!(s.elapsed, Some(Duration::from_secs(3)));
        assert_eq!(s.queued, 2);
    }
}
