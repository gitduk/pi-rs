//! Optional process diagnostics. Session recovery never depends on these logs.
//!
//! PI_LOG enables one JSONL file for the lifetime of the process.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

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

// A wedged run can produce records without bound. Past this the log says
// so and stops, rather than filling the disk of the machine it is diagnosing.
const FILE_CAP: u64 = 64 * 1024 * 1024;

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

fn secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET.contains(&lower.as_str())
        || lower.ends_with("_key")
        || lower.ends_with("_token")
        || lower.ends_with("_secret")
}
// Enough to tell two keys apart, not enough to reconstruct either. FNV-1a
// because the question is only "is this the same string as last time".
fn fingerprint(s: &str) -> String {
    format!("<redacted {:08x}>", tool::stamp::fnv(s.as_bytes()))
}

fn clip(s: &str, cap: usize) -> Value {
    Value::String(llm::slice::clip_bytes(s, cap))
}

struct Sink {
    out: BufWriter<File>,
    written: u64,
    state: Writes,
}

// Both stops are permanent for this process.
enum Writes {
    Open,
    // Past `FILE_CAP`: the file holds what it can.
    Capped,
    // The caller decides how and when to report a diagnostic output failure.
    Failed(String),
}

// One output shared by the subscriber and diagnostic status queries.
struct Diagnostics {
    // Wall clock is what pairs a record with everything else on the machine;
    // the monotonic one is what measures. Neither substitutes for the other.
    start: Instant,
    field_cap: usize,
    sink: Mutex<Sink>,
    path: PathBuf,
}

// Private from creation: diagnostic fields can contain sensitive context.
fn open_file(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl Diagnostics {
    fn open(path: &Path, field_cap: usize) -> std::io::Result<Self> {
        let file = open_file(path)?;
        Ok(Self {
            start: Instant::now(),
            field_cap,
            sink: Mutex::new(Sink {
                out: BufWriter::new(file),
                written: 0,
                state: Writes::Open,
            }),
            path: path.to_path_buf(),
        })
    }

    // Output failures stop logging permanently without changing execution.
    fn write(&self, record: Map<String, Value>) {
        let Ok(mut sink) = self.sink.lock() else {
            return;
        };
        if !matches!(sink.state, Writes::Open) {
            return;
        }
        let Ok(mut line) = serde_json::to_vec(&Value::Object(record)) else {
            return;
        };
        line.push(b'\n');
        const CAPPED: &[u8] = b"{\"lvl\":\"WARN\",\"ev\":\"pi::diagnostics\",\"msg\":\"diagnostic log reached its size limit\"}\n";
        if sink
            .written
            .saturating_add(line.len() as u64 + CAPPED.len() as u64)
            > FILE_CAP
        {
            sink.state = Writes::Capped;
            line = CAPPED.to_vec();
        }
        sink.written += line.len() as u64;
        if let Err(error) = sink.out.write_all(&line) {
            sink.state = Writes::Failed(error.to_string());
            return;
        }
        // Flushed per record: the records worth having are the ones written just
        // before whatever killed the process. A failed flush stops it for good.
        if let Err(e) = sink.out.flush() {
            sink.state = Writes::Failed(e.to_string());
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
    // what diagnostics need, not only the outermost wrapper message.
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

struct DiagnosticsLayer {
    diagnostics: std::sync::Arc<Diagnostics>,
}

// Dependency traces can include raw request content and headers.
fn ours(level: LevelFilter) -> FilterFn<impl Fn(&Metadata<'_>) -> bool> {
    filter_fn(move |meta| {
        (from_workspace(meta.file()) || meta.target().starts_with("pi::")) && level >= *meta.level()
    })
}

// Cargo passes rustc a member's path relative, a dependency's absolute. Path
// remapping (`--remap-path-prefix`, `trim-paths`) voids this; a test guards it.
fn from_workspace(file: Option<&str>) -> bool {
    file.is_some_and(|f| Path::new(f).is_relative())
}

impl DiagnosticsLayer {
    // The skeleton every record shares.
    fn head(&self, meta: &Metadata, ev: &str, ctx_names: String) -> Map<String, Value> {
        let mut rec = Map::new();
        rec.insert(
            "ts".into(),
            Value::String(crate::text::rfc3339(SystemTime::now())),
        );
        rec.insert(
            "ms".into(),
            Value::from(self.diagnostics.start.elapsed().as_millis() as u64),
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

impl<S> Layer<S> for DiagnosticsLayer
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
            cap: self.diagnostics.field_cap,
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
            cap: self.diagnostics.field_cap,
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
            cap: self.diagnostics.field_cap,
        });
        self.diagnostics.write(rec);
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
        self.diagnostics.write(rec);
    }
}

// One immutable destination, independent of session creation and switching.
static DIAGNOSTICS: std::sync::OnceLock<std::sync::Arc<Diagnostics>> = std::sync::OnceLock::new();

pub fn path() -> Option<PathBuf> {
    Some(DIAGNOSTICS.get()?.path.clone())
}

/// A diagnostic output failure or limit never changes execution or persistence.
pub fn failure() -> Option<String> {
    match &DIAGNOSTICS.get()?.sink.lock().ok()?.state {
        Writes::Failed(why) => Some(why.clone()),
        Writes::Capped => Some(format!("diagnostic log reached {FILE_CAP} bytes")),
        Writes::Open => None,
    }
}

pub fn failed() -> bool {
    DIAGNOSTICS
        .get()
        .and_then(|log| {
            log.sink
                .lock()
                .ok()
                .map(|sink| matches!(sink.state, Writes::Failed(_) | Writes::Capped))
        })
        .unwrap_or(false)
}

/// Missing or invalid PI_LOG values leave diagnostics disabled.
pub fn level_from_env() -> LogLevel {
    std::env::var("PI_LOG")
        .ok()
        .and_then(|name| <LogLevel as clap::ValueEnum>::from_str(&name, true).ok())
        .unwrap_or(LogLevel::Off)
}

/// Install an optional process log before any execution starts.
pub fn install(level: LogLevel) -> std::io::Result<Option<PathBuf>> {
    if level == LogLevel::Off {
        return Ok(None);
    }
    if let Some(path) = path() {
        return Ok(Some(path));
    }
    let root = crate::dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no directory for process diagnostics",
        )
    })?;
    let stamp = crate::text::rfc3339(SystemTime::now()).replace(':', "-");
    let path = root
        .join("logs")
        .join(format!("{stamp}-{}.jsonl", std::process::id()));
    let diagnostics = std::sync::Arc::new(Diagnostics::open(&path, level.field_cap())?);
    let layer = DiagnosticsLayer {
        diagnostics: diagnostics.clone(),
    }
    .with_filter(ours(level.filter()));
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .map_err(std::io::Error::other)?;
    let _ = DIAGNOSTICS.set(diagnostics);
    Ok(Some(path))
}

/// Record resolved startup context without copying prompt or request content.
pub fn opening(
    id: &str,
    // The `--config` the run was given, if any.
    config_path: Option<&str>,
    config: &crate::config::Config,
    project: Option<&Path>,
    root: &Path,
    prior: Option<&crate::session::Stored>,
) {
    if tracing::enabled!(target: "pi::session", tracing::Level::INFO) {
        let config_path = config_path
            .map(std::path::PathBuf::from)
            .or_else(crate::config::global_path);
        tracing::info!(
            target: "pi::session",
            version = env!("CARGO_PKG_VERSION"),
            workspace = %root.display(),
            config = %config_path.map_or_else(|| "-".into(), |p| p.display().to_string()),
            models = config.names().len(),
            discovered_models = config.discovered.len(),
            rebound_keys = config.keys.len(),
            model = config.model.as_deref().unwrap_or("-"),
            project = %project.map_or("-".into(), |p| p.display().to_string()),
            resumed = prior.map(|p| p.session.entries().len()).unwrap_or(0),
            session = id,
            "start"
        );
    }
    if tracing::enabled!(target: "pi::keys", tracing::Level::DEBUG) {
        // Key bindings help diagnose configuration that was accepted but had no effect.
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
        assert!(s.contains("more bytes"));
        // Would have panicked on construction if the cut split the é.
        assert!(s.starts_with("élan"));
        assert_eq!(clip("short", 33), Value::String("short".into()));
    }

    // Run `f` against a real layer and read back what it wrote.
    fn recorded(level: LogLevel, f: impl FnOnce()) -> Vec<Value> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let diagnostics = std::sync::Arc::new(Diagnostics::open(&path, level.field_cap()).unwrap());
        let layer = DiagnosticsLayer { diagnostics }.with_filter(ours(level.filter()));
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
        assert!(from_workspace(Some("crates/pi/src/store/diagnostics.rs")));
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
        assert!(at_info[0]["patch"].as_str().unwrap().contains("more bytes"));
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
        let j = Diagnostics::open(&path, 64).unwrap();
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
    fn process_logs_are_private_and_cannot_replace_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("process.jsonl");
        Diagnostics::open(&path, 64).unwrap().write(Map::new());
        assert!(Diagnostics::open(&path, 64).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
