//! The journal: what pi did, as opposed to what the model saw.
//!
//! The transcript beside it already holds every message, tool call and result,
//! so this file holds the rest — the decisions, the wire traffic and the
//! timings that never reach a message. Reading a bug back means reading the two
//! together rather than reproducing it.
//!
//! One file per run, named for the session the run started on. `/new` starts a
//! second transcript without starting a second journal — a bug that spans the
//! two is one story, and it reads as one only if it stays in one file — so the
//! seam is recorded instead.
//!
//! One JSON object per line. `jq` is the intended reader; a line is never
//! wrapped and never depends on the line before it.
//!
//! Records are filed under a `pi::` target — `pi::loop`, `pi::wire`, `pi::tool`
//! and so on — which is what `ev` carries and what a reader filters on. A call
//! that states no target takes its module path instead and still arrives,
//! because every crate in this workspace is on the list too: a diagnostic that
//! goes missing for want of a convention is the failure this file exists to
//! prevent. What is left out below `trace` is the dependencies.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// How much of one string field survives. Raising the level raises this too:
/// at `info` the journal is a timeline and a truncated patch still identifies
/// itself, while `debug` is what you turn on to read the patch.
const FIELD_CAP_INFO: usize = 1_024;
const FIELD_CAP_DEBUG: usize = 64 * 1_024;

/// A wedged run can produce records without bound. Past this the journal says
/// so and stops, rather than filling the disk of the machine it is diagnosing.
const FILE_CAP: u64 = 64 * 1024 * 1024;

/// Journals outlive the sessions they describe by two weeks. Long enough for
/// "it did something odd on Monday", short enough not to accumulate.
const KEEP: Duration = Duration::from_secs(14 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum LogLevel {
    Off,
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn filter(self) -> LevelFilter {
        match self {
            LogLevel::Off => LevelFilter::OFF,
            LogLevel::Error => LevelFilter::ERROR,
            LogLevel::Warn => LevelFilter::WARN,
            LogLevel::Info => LevelFilter::INFO,
            LogLevel::Debug => LevelFilter::DEBUG,
            LogLevel::Trace => LevelFilter::TRACE,
        }
    }

    fn field_cap(self) -> usize {
        match self {
            LogLevel::Trace | LogLevel::Debug => FIELD_CAP_DEBUG,
            _ => FIELD_CAP_INFO,
        }
    }
}

/// Names the record's own skeleton owns. `msg` is not among them: an event's
/// format string is exactly what should fill it.
const HEAD: [&str; 5] = ["ts", "ms", "lvl", "ev", "in"];

/// Field names whose value never reaches the file.
///
/// A backstop, not the rule: the rule is that call sites do not pass secrets.
/// Matched on the whole name and on the `_key` / `_token` / `_secret` suffix,
/// so `api_key` goes and `api_key_env` — the name of a variable, which is
/// exactly what a key bug needs — stays.
const SECRET: [&str; 7] = [
    "api_key",
    "apikey",
    "authorization",
    "token",
    "secret",
    "password",
    "bearer",
];

fn secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET.contains(&lower.as_str())
        || lower.ends_with("_key")
        || lower.ends_with("_token")
        || lower.ends_with("_secret")
}

/// Enough to tell two keys apart, not enough to reconstruct either. FNV-1a
/// because the question is only "is this the same string as last time".
fn fingerprint(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("<redacted {h:08x}>")
}

/// Cut on a char boundary, and say how much went. A silently short value reads
/// as the whole value, which is how a truncated log tells you a lie.
fn clip(s: &str, cap: usize) -> Value {
    if s.len() <= cap {
        return Value::String(s.to_string());
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Value::String(format!("{}…+{} bytes", &s[..end], s.len() - end))
}

// ---------------------------------------------------------------- timestamps

/// RFC 3339, UTC, milliseconds. Hand-rolled: a calendar is thirty lines and a
/// date crate is a dependency the rest of the binary has no use for.
fn rfc3339(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, dd) = civil(days);
    format!(
        "{y:04}-{m:02}-{dd:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        d.subsec_millis()
    )
}

/// Days since the epoch to a civil date (Howard Hinnant's `civil_from_days`).
fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// --------------------------------------------------------------------- sink

struct Sink {
    out: BufWriter<File>,
    written: u64,
    capped: bool,
}

/// The open journal. One per process; the layer holds it and nothing else
/// writes to the file. Where it is on disk is [`path`], not a field here —
/// there is one journal, and two copies of its name only stay equal by luck.
struct Journal {
    /// Wall clock is what pairs a record with everything else on the machine;
    /// the monotonic one is what measures. Neither substitutes for the other.
    start: Instant,
    field_cap: usize,
    sink: Mutex<Sink>,
}

impl Journal {
    fn open(path: &Path, field_cap: usize) -> std::io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Prompts, paths and patches, the same as the transcript.
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            start: Instant::now(),
            field_cap,
            sink: Mutex::new(Sink {
                out: BufWriter::new(file),
                written,
                capped: false,
            }),
        })
    }

    /// One record. Every failure here is swallowed: a run must not die of its
    /// own logging, and a poisoned lock would otherwise take the process with
    /// it on the next record.
    fn write(&self, record: Map<String, Value>) {
        let Ok(mut sink) = self.sink.lock() else {
            return;
        };
        if sink.capped {
            return;
        }
        let Ok(mut line) = serde_json::to_vec(&Value::Object(record)) else {
            return;
        };
        line.push(b'\n');
        sink.written += line.len() as u64;
        let _ = sink.out.write_all(&line);
        if sink.written >= FILE_CAP {
            sink.capped = true;
            let _ = writeln!(
                sink.out,
                r#"{{"lvl":"WARN","ev":"pi::journal","msg":"journal capped at {FILE_CAP} bytes; nothing further is recorded"}}"#
            );
        }
        // Flushed per record on purpose: the records worth having are the ones
        // written just before the thing that killed the process.
        let _ = sink.out.flush();
    }
}

// -------------------------------------------------------------------- layer

/// Collects `tracing` fields into a JSON object, redacting and clipping on the
/// way in so nothing oversized is ever held.
struct Fields<'a> {
    into: &'a mut Map<String, Value>,
    cap: usize,
}

impl Fields<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        let name = match field.name() {
            // `tracing`'s own name for the format string.
            "message" => "msg",
            // A call site that reuses a header's name would otherwise overwrite
            // it — a wrong `ms` reads as a real one. Kept, under a name that
            // cannot collide, rather than dropped or allowed to win.
            n if HEAD.contains(&n) => return self.shadowed(n, value),
            other => other,
        };
        self.into.insert(name.to_string(), value);
    }

    fn shadowed(&mut self, name: &str, value: Value) {
        self.into.insert(format!("{name}_"), value);
    }

    fn text(&mut self, field: &Field, s: &str) {
        let value = if secret(field.name()) {
            Value::String(fingerprint(s))
        } else {
            clip(s, self.cap)
        };
        self.put(field, value);
    }
}

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.text(field, value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.text(field, &format!("{value:?}"));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::Bool(value));
    }

    /// The whole chain, not the outermost link. An error's own line is
    /// routinely the least specific thing about it — "error sending request"
    /// over "Connection refused" — and the cause is what a journal is read for.
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        let mut text = value.to_string();
        let mut source = value.source();
        while let Some(next) = source {
            text.push_str(": ");
            text.push_str(&next.to_string());
            source = next.source();
        }
        self.text(field, &text);
    }
}

/// What a span carries to the records inside it.
struct Scope {
    fields: Map<String, Value>,
    opened: Instant,
}

struct JournalLayer {
    journal: std::sync::Arc<Journal>,
}

/// Which records reach the file.
///
/// Ours, at the chosen level; a dependency's, only at `trace`. Without the
/// second half the journal is mostly hyper's connection pool, and `trace` — the
/// level you reach for when nothing else explained it — is unreadable. That
/// level is also the one place a dependency's own account is worth having,
/// which is where the line lifts.
///
/// "Ours" is every crate in the workspace as well as the `pi::` namespace, so a
/// `tracing` call that forgets the target convention is still recorded. The
/// alternative — matching `pi::` alone — drops such a call silently at every
/// level, which is precisely the kind of bug this file exists to catch.
fn ours(level: LevelFilter) -> Targets {
    const MINE: [&str; 7] = ["pi", "cli", "agent", "brain", "tools", "hashline", "syntax"];
    let theirs = if level == LevelFilter::TRACE {
        level
    } else {
        LevelFilter::OFF
    };
    Targets::new()
        .with_targets(MINE.map(|t| (t, level)))
        .with_default(theirs)
}

impl JournalLayer {
    /// The skeleton every record shares.
    fn head(&self, meta: &Metadata, ev: &str, ctx_names: String) -> Map<String, Value> {
        let mut rec = Map::new();
        rec.insert("ts".into(), Value::String(rfc3339(SystemTime::now())));
        rec.insert(
            "ms".into(),
            Value::from(self.journal.start.elapsed().as_millis() as u64),
        );
        rec.insert("lvl".into(), Value::String(meta.level().to_string()));
        rec.insert("ev".into(), Value::String(ev.to_string()));
        if !ctx_names.is_empty() {
            rec.insert("in".into(), Value::String(ctx_names));
        }
        rec
    }
}

/// The span path a record sits under, outermost first: `turn>tool`.
fn path_of<S>(span: &tracing_subscriber::registry::SpanRef<'_, S>) -> String
where
    S: for<'a> LookupSpan<'a>,
{
    span.scope()
        .from_root()
        .map(|s| s.name())
        .collect::<Vec<_>>()
        .join(">")
}

impl<S> Layer<S> for JournalLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = Map::new();
        attrs.record(&mut Fields {
            into: &mut fields,
            cap: self.journal.field_cap,
        });
        span.extensions_mut().insert(Scope {
            fields,
            opened: Instant::now(),
        });
    }

    fn on_record(&self, id: &tracing::Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut ext = span.extensions_mut();
        let Some(scope) = ext.get_mut::<Scope>() else {
            return;
        };
        values.record(&mut Fields {
            into: &mut scope.fields,
            cap: self.journal.field_cap,
        });
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let here = event
            .parent()
            .and_then(|id| ctx.span(id))
            .or_else(|| ctx.event_span(event));
        let meta = event.metadata();
        let mut rec = self.head(
            meta,
            meta.target(),
            here.as_ref().map(path_of).unwrap_or_default(),
        );

        // Enclosing scopes first, innermost last, so a field an event states
        // itself wins over the same name inherited from the span around it.
        if let Some(here) = &here {
            for span in here.scope().from_root() {
                if let Some(scope) = span.extensions().get::<Scope>() {
                    for (k, v) in &scope.fields {
                        rec.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        event.record(&mut Fields {
            into: &mut rec,
            cap: self.journal.field_cap,
        });
        self.journal.write(rec);
    }

    /// A span's close is where its duration is known, which is most of why the
    /// spans exist: "which step was slow" is not answerable from events alone.
    fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let ext = span.extensions();
        let Some(scope) = ext.get::<Scope>() else {
            return;
        };
        // Named for the span, not for the crate the span was opened in: the
        // whole point of the record is which step finished.
        let mut rec = self.head(span.metadata(), "pi::span", path_of(&span));
        for (k, v) in &scope.fields {
            rec.insert(k.clone(), v.clone());
        }
        rec.insert(
            "dur_ms".into(),
            Value::from(scope.opened.elapsed().as_millis() as u64),
        );
        rec.insert("msg".into(), Value::String(format!("{} done", span.name())));
        self.journal.write(rec);
    }
}

// ------------------------------------------------------------------- install

/// The journal this process is writing, if it is writing one.
///
/// A process fact, held as one: there is exactly one journal, every surface
/// that wants to name it is far from where it was opened, and threading a path
/// through the terminal loop to print one line is worse than saying so here.
static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn path() -> Option<&'static Path> {
    PATH.get().map(PathBuf::as_path)
}

/// Where journals live, beside the transcripts they belong to.
fn dir() -> Option<PathBuf> {
    tools::state::dir().map(|d| d.join("logs"))
}

/// Drop journals nothing will be read back from. Failures are ignored: a full
/// or read-only log directory is a reason to log less, never to stop the run.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        if entry.path().extension().is_none_or(|e| e != "jsonl") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > KEEP);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Open the journal for this session and make it the process's `tracing` sink.
///
/// Returns the path so the caller can say where it went. `None` means nothing
/// is being recorded — the level is off, there is no state directory, or the
/// file would not open; none of those is worth failing a run over, which is
/// why the error is reported and dropped rather than returned.
pub fn install(id: &str, level: LogLevel) {
    if level == LogLevel::Off {
        return;
    }
    let Some(dir) = dir() else { return };
    let path = dir.join(format!("{}.jsonl", tools::state::file_stem(id)));
    let journal = match Journal::open(&path, level.field_cap()) {
        Ok(j) => std::sync::Arc::new(j),
        Err(e) => {
            eprintln!("warning: no journal ({e}); --log off silences this");
            return;
        }
    };
    let layer = JournalLayer { journal }.with_filter(ours(level.filter()));
    if tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer)).is_ok() {
        let _ = PATH.set(path);
    }
    // Off the startup path: a fortnight of journals is hundreds of files to
    // stat, almost never with anything to delete, and the run has a request to
    // send. Losing the sweep to an early exit costs nothing — the next run
    // does it.
    tokio::task::spawn_blocking(move || prune(&dir));
}

/// The state the run started from, recorded once.
///
/// Everything here was already settled before the journal opened — the config
/// read, the workspace resolved, the prior session loaded — so it is stated
/// rather than observed. A surprising run is usually surprising because of one
/// of these, and none of them is visible anywhere else afterwards.
pub fn opening(
    id: &str,
    args: &crate::Args,
    config: &crate::config::Config,
    project: &crate::config::Project,
    root: &Path,
    prior: Option<&crate::session::Stored>,
) {
    tracing::info!(
        target: "pi::session",
        version = env!("CARGO_PKG_VERSION"),
        argv = %std::env::args().skip(1).collect::<Vec<_>>().join(" "),
        workspace = %root.display(),
        config = args.config.as_deref().unwrap_or("~/.pi.toml"),
        models = config.models.len(),
        rebound_keys = config.keys.len(),
        default_model = config.defaults.model.as_deref().unwrap_or("-"),
        project_model = project.defaults.model.as_deref().unwrap_or("-"),
        project_max_tier = ?project.defaults.max_tier,
        resumed = prior.map(|p| p.log.entries().len()).unwrap_or(0),
        session = id,
        "start"
    );
    // Which key does what is a question the terminal cannot be asked after the
    // fact, and a binding that silently did not apply looks exactly like one
    // that did. Debug rather than info: it is a table, not a fact.
    for (action, binds) in &config.keys {
        // Written out rather than debug-printed: the point is to compare it
        // against what the user meant to write, not against the enum.
        let keys = match binds {
            crate::config::Binds::One(k) => k.clone(),
            crate::config::Binds::Many(k) => k.join(", "),
        };
        tracing::debug!(target: "pi::keys", action, keys, "rebound");
    }
}

/// The journal outlives the transcript it was named after: `/new` starts a
/// fresh session in the same process, and a bug that spans the two reads as
/// one story only if it stays in one file. This is the seam between them.
pub fn now_recording(id: &str) {
    tracing::info!(target: "pi::session", session = id, "transcript");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_go_and_the_names_of_secrets_stay() {
        assert!(secret("api_key"));
        assert!(secret("Authorization"));
        assert!(secret("session_token"));
        // The variable a key is read from is not the key.
        assert!(!secret("api_key_env"));
        assert!(!secret("keys"));
    }

    #[test]
    fn a_fingerprint_separates_keys_without_carrying_one() {
        let a = fingerprint("sk-aaaaaaaaaaaaaaaa");
        assert_eq!(a, fingerprint("sk-aaaaaaaaaaaaaaaa"));
        assert_ne!(a, fingerprint("sk-bbbbbbbbbbbbbbbb"));
        assert!(!a.contains("sk-"));
    }

    #[test]
    fn clipping_says_how_much_it_took_and_never_splits_a_char() {
        let long = "élan ".repeat(400);
        let Value::String(s) = clip(&long, 33) else {
            panic!("a string clips to a string")
        };
        assert!(s.contains("…+"));
        // Would have panicked on construction if the cut split the é.
        assert!(s.starts_with("élan"));
        assert_eq!(clip("short", 33), Value::String("short".into()));
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let t = UNIX_EPOCH + Duration::from_millis(1_755_000_000_123);
        assert_eq!(rfc3339(t), "2025-08-12T12:00:00.123Z");
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        // A leap day, which is where a hand-rolled calendar goes wrong.
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_709_164_800)),
            "2024-02-29T00:00:00.000Z"
        );
    }

    /// Run `f` against a real layer and read back what it wrote.
    fn recorded(level: LogLevel, f: impl FnOnce()) -> Vec<Value> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let journal = std::sync::Arc::new(Journal::open(&path, level.field_cap()).unwrap());
        let layer = JournalLayer { journal }.with_filter(ours(level.filter()));
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), f);
        std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn a_call_site_cannot_overwrite_the_records_own_skeleton() {
        let out = recorded(LogLevel::Info, || {
            tracing::info!(target: "pi::t", ms = 9999, lvl = "shouty", "hi");
        });
        // The header's own `ms` is elapsed time, and it survives.
        assert_eq!(out[0]["lvl"], "INFO");
        assert_ne!(out[0]["ms"], 9999);
        // The call site's values are kept, one name over.
        assert_eq!(out[0]["ms_"], 9999);
        assert_eq!(out[0]["lvl_"], "shouty");
        assert_eq!(out[0]["msg"], "hi");
    }

    #[test]
    fn a_dependencys_records_arrive_only_at_trace() {
        let three = || {
            tracing::info!(target: "pi::t", "mine");
            // A workspace crate that forgot the `pi::` convention: still ours.
            tracing::info!(target: "tools::grep", "mine too");
            tracing::info!(target: "hyper::pool", "theirs");
        };
        let at_info = recorded(LogLevel::Info, three);
        assert_eq!(at_info.len(), 2);
        assert_eq!(at_info[1]["ev"], "tools::grep");

        assert_eq!(recorded(LogLevel::Trace, three).len(), 3);
    }

    #[test]
    fn a_span_gives_its_fields_and_its_duration_to_what_it_holds() {
        let out = recorded(LogLevel::Info, || {
            let span = tracing::info_span!(target: "pi::t", "turn", turn = 3);
            span.in_scope(|| tracing::info!(target: "pi::t", tool = "edit", "call"));
        });
        assert_eq!(out[0]["turn"], 3, "inherited from the span");
        assert_eq!(out[0]["in"], "turn");
        // The close record carries what the event's cannot: how long it took.
        assert_eq!(out[1]["ev"], "pi::span");
        assert_eq!(out[1]["msg"], "turn done");
        assert!(out[1]["dur_ms"].is_number());
        // Its own fields too, which is what lets a tool's close record name
        // the tool rather than only the kind of span it was.
        assert_eq!(out[1]["turn"], 3);
    }

    #[test]
    fn the_level_decides_how_much_of_a_payload_survives() {
        let patch = "x".repeat(4_000);
        let at_info = recorded(
            LogLevel::Info,
            || tracing::warn!(target: "pi::t", patch, "rejected"),
        );
        let at_debug = recorded(
            LogLevel::Debug,
            || tracing::warn!(target: "pi::t", patch, "rejected"),
        );
        assert!(at_info[0]["patch"].as_str().unwrap().contains("…+"));
        assert_eq!(at_debug[0]["patch"].as_str().unwrap().len(), 4_000);
    }

    #[test]
    fn an_error_is_recorded_with_what_actually_caused_it() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("connection refused")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("error sending request")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let out = recorded(LogLevel::Info, || {
            let e = Outer(Inner);
            tracing::error!(target: "pi::t", error = &e as &dyn std::error::Error, "failed");
        });
        assert_eq!(out[0]["error"], "error sending request: connection refused");
    }

    #[test]
    fn every_record_is_one_line_of_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let j = Journal::open(&path, 64).unwrap();
        let mut rec = Map::new();
        rec.insert("msg".into(), Value::String("with\na newline".into()));
        j.write(rec);
        j.write(Map::new());
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
        for line in body.lines() {
            serde_json::from_str::<Value>(line).unwrap();
        }
    }

    #[test]
    fn reopening_appends_rather_than_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        Journal::open(&path, 64).unwrap().write(Map::new());
        Journal::open(&path, 64).unwrap().write(Map::new());
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 2);
    }
}
