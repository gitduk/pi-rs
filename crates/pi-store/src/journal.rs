//! The journal: what pi did, as opposed to what the model saw — decisions,
//! wire traffic and timings the transcript beside it doesn't carry.
//!
//! One JSON object per line, meant for `jq`; no line depends on another.
//! One file per session, followed across every run that resumes into it.
//!
//! Records are filed under a `pi::` target, or absent one, the module path.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::filter::{FilterFn, filter_fn};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

// How much of one string field survives; raising the level raises this too.
// `info` keeps a timeline readable; `debug` is for reading one field whole.
const FIELD_CAP_INFO: usize = 1_024;
const FIELD_CAP_DEBUG: usize = 64 * 1_024;

// A wedged run can produce records without bound. Past this the journal says
// so and stops, rather than filling the disk of the machine it is diagnosing.
const FILE_CAP: u64 = 64 * 1024 * 1024;

// Journals outlive the sessions they describe by two weeks. Long enough for
// "it did something odd on Monday", short enough not to accumulate.
const KEEP: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// The journal inside each session's directory, named once for the store that
/// writes it and the sweep that looks for it by name.
pub const JOURNAL_FILE: &str = "journal.jsonl";

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

// Names the record's own skeleton owns. `msg` is not among them: an event's
// format string is exactly what should fill it.
const HEAD: [&str; 5] = ["ts", "ms", "lvl", "ev", "in"];

// A backstop, not the rule — call sites are expected not to pass secrets.
// `api_key_env` stays: it names a variable, not a key.
const SECRET: [&str; 7] = [
    "api_key",
    "apikey",
    "authorization",
    "token",
    "secret",
    "password",
    "bearer",
];

pub(crate) fn secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET.contains(&lower.as_str())
        || lower.ends_with("_key")
        || lower.ends_with("_token")
        || lower.ends_with("_secret")
}
// Enough to tell two keys apart, not enough to reconstruct either. FNV-1a
// because the question is only "is this the same string as last time".
fn fingerprint(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("<redacted {h:08x}>")
}

// Cut on a char boundary, and say how much went. A silently short value reads
// as the whole value, which is how a truncated log tells you a lie.
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

// RFC 3339, UTC, milliseconds. Hand-rolled: a calendar is thirty lines and a
// date crate is a dependency the rest of the binary has no use for.
pub fn rfc3339(t: SystemTime) -> String {
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

// Days since the epoch to a civil date (Howard Hinnant's `civil_from_days`).
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

struct Sink {
    out: BufWriter<File>,
    written: u64,
    capped: bool,
    // A failed flush means the file can no longer be trusted to hold what we
    // send it; the sink stops writing and says so once on stderr.
    failed: bool,
    // Which file `out` writes to now; `retarget` moves it. Kept under the
    // sink's lock so [`path`] reads it back rather than a copy that could drift.
    path: PathBuf,
}

// The open journal: one per run, held only by the layer.
struct Journal {
    // Wall clock is what pairs a record with everything else on the machine;
    // the monotonic one is what measures. Neither substitutes for the other.
    start: Instant,
    field_cap: usize,
    sink: Mutex<Sink>,
}

// A fresh sink on `file`, counting what is already in it.
fn sink_for(file: File, path: &Path) -> Sink {
    let written = file.metadata().map(|m| m.len()).unwrap_or(0);
    Sink {
        out: BufWriter::new(file),
        written,
        capped: false,
        failed: false,
        path: path.to_path_buf(),
    }
}

// Open for appending, 0600, the same as the transcript beside it.
fn open_file(path: &Path) -> std::io::Result<File> {
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
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

impl Journal {
    fn open(path: &Path, field_cap: usize) -> std::io::Result<Self> {
        let file = open_file(path)?;
        Ok(Self {
            start: Instant::now(),
            field_cap,
            sink: Mutex::new(sink_for(file, path)),
        })
    }

    // Points the same journal at another session's file (`/resume` or `/new`)
    // without reinstalling the subscriber; the cap applies to the new file fresh.
    fn retarget(&self, path: &Path) -> std::io::Result<()> {
        let file = open_file(path)?;
        let Ok(mut sink) = self.sink.lock() else {
            // A poisoned lock is a run to be rescued, not one to kill.
            return Ok(());
        };
        *sink = sink_for(file, path);
        Ok(())
    }

    // Every failure here is swallowed except a failed flush, which stops
    // writing for good — a run must not die of its own logging.
    fn write(&self, record: Map<String, Value>) {
        let Ok(mut sink) = self.sink.lock() else {
            return;
        };
        if sink.capped || sink.failed {
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
        // Flushed per record: the records worth having are the ones written just
        // before whatever killed the process. A failed flush warns on stderr.
        if let Err(e) = sink.out.flush() {
            sink.failed = true;
            eprintln!("warning: journal write failed ({e}); recording stops and stays stopped");
        }
    }
}

// Collects `tracing` fields into a JSON object, redacting and clipping on the
// way in so nothing oversized is ever held.
struct Fields<'a> {
    into: &'a mut Map<String, Value>,
    cap: usize,
}

impl Fields<'_> {
    fn put(&mut self, field: &Field, value: Value) {
        let name = match field.name() {
            // `tracing`'s own name for the format string.
            "message" => "msg",
            // A call site reusing a header's name would otherwise overwrite it — a
            // wrong `ms` would read as real. Kept under a name that cannot collide.
            n if HEAD.contains(&n) => return self.shadowed(n, value),
            other => other,
        };
        self.into.insert(name.to_string(), value);
    }

    fn shadowed(&mut self, name: &str, value: Value) {
        self.into.insert(format!("{name}_"), value);
    }

    fn value(&mut self, field: &Field, value: Value) {
        if secret(field.name()) {
            self.put(field, Value::String(fingerprint(&value.to_string())));
        } else {
            self.put(field, value);
        }
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
        self.value(field, Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.value(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.value(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.value(field, Value::Bool(value));
    }

    // The whole chain, not just the outermost line: the cause is usually
    // what a journal is read for, not the least-specific wrapper message.
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

// What a span carries to the records inside it.
struct Scope {
    fields: Map<String, Value>,
    opened: Instant,
}

struct JournalLayer {
    journal: std::sync::Arc<Journal>,
}

// Ours, at the chosen level; a dependency's, only at `trace` — else the
// journal is mostly hyper's pool. "Ours" means where the source lives.
fn ours(level: LevelFilter) -> FilterFn<impl Fn(&Metadata<'_>) -> bool> {
    let theirs = if level == LevelFilter::TRACE {
        level
    } else {
        LevelFilter::OFF
    };
    filter_fn(move |meta| {
        // The `pi::` convention still holds where a build has remapped paths.
        let mine = from_workspace(meta.file()) || meta.target().starts_with("pi::");
        let cap = if mine { level } else { theirs };
        cap >= *meta.level()
    })
}

// Cargo passes rustc a member's path relative, a dependency's absolute. Path
// remapping (`--remap-path-prefix`, `trim-paths`) voids this; a test guards it.
fn from_workspace(file: Option<&str>) -> bool {
    file.is_some_and(|f| Path::new(f).is_relative())
}

impl JournalLayer {
    // The skeleton every record shares.
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

// Outermost first, bracketed with each span's own session id, e.g.
// `turn[p]>tool>subagent[c]>turn[c]>tool` tells parallel subagents apart.
fn path_of<S>(span: &tracing_subscriber::registry::SpanRef<'_, S>) -> String
where
    S: for<'a> LookupSpan<'a>,
{
    span.scope()
        .from_root()
        .map(|s| {
            let who = s
                .extensions()
                .get::<Scope>()
                .and_then(|sc| sc.fields.get("session"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            match who {
                Some(id) => format!("{}[{id}]", s.name()),
                None => s.name().to_string(),
            }
        })
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

    // A span's close is where its duration is known, which is most of why the
    // spans exist: "which step was slow" is not answerable from events alone.
    fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        // Released before `path_of` and the walk below, which read this same
        // span's extensions again: a recursive read can deadlock.
        let opened = {
            let ext = span.extensions();
            match ext.get::<Scope>() {
                Some(scope) => scope.opened,
                None => return,
            }
        };
        // Named for the span, not for the crate the span was opened in: the
        // whole point of the record is which step finished.
        let mut rec = self.head(span.metadata(), "pi::span", path_of(&span));
        // Enclosing scopes first, own fields last, like an event's record:
        // a tool's close then keeps the turn and session it ran under.
        for s in span.scope().from_root() {
            if let Some(sc) = s.extensions().get::<Scope>() {
                for (k, v) in &sc.fields {
                    rec.insert(k.clone(), v.clone());
                }
            }
        }
        rec.insert(
            "dur_ms".into(),
            Value::from(opened.elapsed().as_millis() as u64),
        );
        rec.insert("msg".into(), Value::String(format!("{} done", span.name())));
        self.journal.write(rec);
    }
}

// One journal per run, retargeted in place on session switch, so any
// surface can reach it without the path threaded through the terminal loop.
static JOURNAL: std::sync::OnceLock<std::sync::Arc<Journal>> = std::sync::OnceLock::new();

/// Where the journal is writing right now, for `/status` and the failure line.
/// Read back from the journal itself — the sink holds its own path — so there
/// is no second copy to keep in step.
pub fn path() -> Option<PathBuf> {
    JOURNAL.get()?.sink.lock().ok().map(|s| s.path.clone())
}

/// Drops journals nothing will be read back from, kept a fortnight — shorter
/// than the transcript beside it. Failures just mean logging less, never stopping.
pub fn prune(sessions: &Path) {
    let Ok(buckets) = std::fs::read_dir(sessions) else {
        return;
    };
    for bucket in buckets.flatten() {
        let Ok(entries) = std::fs::read_dir(bucket.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path().join(JOURNAL_FILE);
            if crate::older_than(&path, KEEP) {
                let _ = std::fs::remove_file(&path);
                // And the directory, if the journal was the last thing in it — a run
                // that never got as far as a transcript, since a bucket needs to be empty.
                crate::session::discard_abandoned(&entry.path());
            }
        }
    }
}

/// The `LogLevel` the `PI_LOG` environment variable names.
///
/// `Info` when it is unset or unreadable. A typo falls back rather than
/// failing: not recording is not worth failing a run over, and neither is
/// misspelling how much to record.
pub fn level_from_env() -> LogLevel {
    let Ok(name) = std::env::var("PI_LOG") else {
        return LogLevel::Info;
    };
    <LogLevel as clap::ValueEnum>::from_str(&name, true).unwrap_or(LogLevel::Info)
}

pub fn install(path: &Path, level: LogLevel) {
    if level == LogLevel::Off {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let journal = match Journal::open(path, level.field_cap()) {
        Ok(j) => std::sync::Arc::new(j),
        Err(e) => {
            eprintln!("warning: no journal ({e}); PI_LOG=off silences this");
            return;
        }
    };
    let layer = JournalLayer {
        journal: journal.clone(),
    }
    .with_filter(ours(level.filter()));
    if tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer)).is_ok() {
        let _ = JOURNAL.set(journal);
    }
}

/// The state the run started from, recorded once: config, workspace and prior
/// session, all settled before the journal opened and stated rather than observed.
pub fn opening(
    id: &str,
    // The `--config` the run was given, if any.
    config_path: Option<&str>,
    config: &crate::config::Config,
    project: Option<&Path>,
    root: &Path,
    prior: Option<&crate::session::Stored>,
) {
    tracing::info!(
        target: "pi::session",
        version = env!("CARGO_PKG_VERSION"),
        argv = %std::env::args().skip(1).collect::<Vec<_>>().join(" "),
        workspace = %root.display(),
        config = config_path.unwrap_or("~/.pi/settings.toml"),
        models = config.names().len(),
        rebound_keys = config.keys.len(),
        model = config.model.as_deref().unwrap_or("-"),
        project = %project.map_or("-".into(), |p| p.display().to_string()),
        resumed = prior.map(|p| p.session.entries().len()).unwrap_or(0),
        session = id,
        "start"
    );
    // A silently-ignored rebind looks exactly like one that worked, and the
    // terminal can't be asked after the fact. Debug, not info: it's a table.
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

/// The run has moved to a different session — a `/resume` or a `/new` — so
/// point the journal at that session's file, and mark the seam in it. Each
/// session keeps one journal across every run that touched it, which is how
/// `/status` and the file both follow the session.
pub fn switched(path: &Path, id: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Some(journal) = JOURNAL.get()
        && let Err(e) = journal.retarget(path)
    {
        tracing::warn!(target: "pi::session", session = id, error = %e, "could not move the journal to this session");
    }
    tracing::info!(target: "pi::session", session = id, "transcript");
}

#[cfg(test)]
mod tests {
    use super::*;

    // The transcript sweep only removes an empty bucket, and a bucket holding
    // a directory never is — so an old journal takes its directory with it.
    #[test]
    fn an_old_journal_takes_its_directory_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path();
        let empty = sessions.join("-w").join("said-nothing");
        let lived = sessions.join("-w").join("had-a-transcript");
        for d in [&empty, &lived] {
            std::fs::create_dir_all(d).unwrap();
            let journal = d.join(JOURNAL_FILE);
            std::fs::write(&journal, b"{}\n").unwrap();
            std::fs::File::options()
                .write(true)
                .open(&journal)
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(15 * 24 * 60 * 60))
                .unwrap();
        }
        std::fs::write(lived.join("session.json"), b"{}").unwrap();

        prune(sessions);

        assert!(
            !empty.exists(),
            "a session with nothing but an old journal goes whole"
        );
        assert!(
            lived.exists(),
            "and one with a transcript keeps its directory"
        );
        assert!(!lived.join(JOURNAL_FILE).exists());
    }

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
        assert!(s.contains(&format!("{}+", crate::icons::ELLIPSIS)));
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

    // Run `f` against a real layer and read back what it wrote.
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

    // A dependency's record cannot be staged from here, so the rule is checked
    // on the paths it decides by.
    #[test]
    fn a_dependencys_records_are_told_apart_by_path() {
        assert!(from_workspace(Some("crates/pi/src/store/journal.rs")));
        assert!(!from_workspace(Some(
            "/home/u/.cargo/registry/src/index/hyper-1.0.0/src/pool.rs"
        )));
        assert!(!from_workspace(None));
    }

    // Whatever target a call names, a record from this workspace is ours. Fails
    // the day cargo stops passing members' paths relative.
    #[test]
    fn our_own_records_arrive_whatever_their_target() {
        let out = recorded(LogLevel::Info, || {
            tracing::info!(target: "pi::t", "mine");
            tracing::info!(target: "hyper::pool", "a borrowed name, still mine");
        });
        assert_eq!(out.len(), 2);
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
    fn records_are_filed_under_the_run_that_made_them() {
        // One parent turn calling three subagents, the span shape `subagent.rs`
        // makes. All three share every span name; the session is the id.

        let out = recorded(LogLevel::Info, || {
            let parent = tracing::info_span!(target: "pi::t", "turn", turn = 1, session = "p-1");
            let _parent = parent.enter();
            tracing::info!(target: "pi::t", "the parent's own record");
            let tool = tracing::info_span!(target: "pi::t", "tool", name = "subagent", call = "c0");
            let _tool = tool.enter();
            for n in 0..3 {
                let id = format!("p-1-subagent-{n}");
                let subagent = tracing::info_span!(target: "pi::t", "subagent", session = %id);
                let _subagent = subagent.enter();
                let turn = tracing::info_span!(target: "pi::t", "turn", turn = 1, session = %id);
                let _turn = turn.enter();
                tracing::info!(target: "pi::t", tool = "bash", "a subagent's record");
                drop(_turn);
                drop(_subagent);
            }
            drop(_tool);
            drop(_parent);
        });

        let by_session =
            |id: &str| -> Vec<&Value> { out.iter().filter(|r| r["session"] == id).collect() };

        // Each child leaves three records, and every one carries its id in
        // the body and in the path that files it.
        for n in 0..3 {
            let id = format!("p-1-subagent-{n}");
            let recs = by_session(&id);
            assert_eq!(recs.len(), 3, "child {n} keeps its three records: {out:?}");
            for r in recs {
                let path = r["in"].as_str().unwrap();
                assert!(
                    path.contains(&format!("subagent[{id}]")),
                    "child {n} is filed under its own span: {path}"
                );
            }
        }

        // The parent's own records stay its own: none inherits a child's id.
        let recs = by_session("p-1");
        assert_eq!(recs.len(), 3, "the parent keeps its own three: {out:?}");
        for r in recs {
            let path = r["in"].as_str().unwrap();
            assert!(!path.contains("subagent-"), "the parent's records: {path}");
        }
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
        assert!(
            at_info[0]["patch"]
                .as_str()
                .unwrap()
                .contains(&format!("{}+", crate::icons::ELLIPSIS))
        );
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

    #[test]
    fn retarget_moves_the_records_to_another_session() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.jsonl");
        let second = dir.path().join("b.jsonl");
        let j = Journal::open(&first, 64).unwrap();
        j.write(Map::new());
        j.retarget(&second).unwrap();
        j.write(Map::new());
        assert_eq!(std::fs::read_to_string(&first).unwrap().lines().count(), 1);
        assert_eq!(std::fs::read_to_string(&second).unwrap().lines().count(), 1);
    }
}
