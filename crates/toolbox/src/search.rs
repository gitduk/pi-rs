//! Web search without a key: the results page an engine serves a browser,
//! read for its titles, links and snippets. Engines change their markup and
//! turn bots away, so one that fails says why and the next is asked.

use async_trait::async_trait;
use base64::Engine as _;
use scraper::{ElementRef, Html, Selector};
use serde::Deserialize;
use serde_json::{Value, json};

use tool::{Ctx, Tier, Tool, ToolError, ToolOutput};

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
// What curl will download of a page; a results page is a few hundred KB.
const MAX_BYTES: &str = "2M";
// Engines answer a bare client with a block page or none at all.
const BROWSER: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                       (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

#[derive(Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    engine: Option<Engine>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Engine {
    Duckduckgo,
    Bing,
}

impl Engine {
    // Asked in this order when the call names none.
    const ALL: [Engine; 2] = [Engine::Duckduckgo, Engine::Bing];

    fn name(self) -> &'static str {
        match self {
            Engine::Duckduckgo => "duckduckgo",
            Engine::Bing => "bing",
        }
    }
}

/// One result as the engine listed it, markup gone.
#[derive(Debug, PartialEq, Eq)]
struct Hit {
    title: String,
    url: String,
    snippet: String,
}

pub struct Search;

impl Search {
    pub const NAME: &'static str = "search";

    // The engine's results page for `query`, as text. Fetched by `curl`:
    // engines that let curl through turn this binary's own TLS client away.
    async fn page(&self, engine: Engine, query: &str) -> Result<String, String> {
        let mut curl = tokio::process::Command::new("curl");
        // No redirects and https only: the two fixed addresses are all it
        // reaches. Uncompressed, so the size cap is what lands in memory.
        curl.args(["--silent", "--show-error", "--proto", "=https"])
            .args(["--max-time", &TIMEOUT.as_secs().to_string()])
            .args(["--max-filesize", MAX_BYTES])
            .args(["--user-agent", BROWSER])
            .args(["--header", "Accept: text/html,application/xhtml+xml"])
            .args(["--header", "Accept-Language: en-US,en;q=0.9"])
            .args(["--write-out", "\n%{http_code}"]);
        // `q=` before the query keeps a leading `@` from naming a file.
        let q = format!("q={query}");
        match engine {
            // The form's own POST: a GET of the same page draws a bot check.
            Engine::Duckduckgo => curl
                .args(["--data-urlencode", &q])
                .arg("https://html.duckduckgo.com/html/"),
            Engine::Bing => curl
                .args(["--get", "--data-urlencode", &q])
                .args(["--data-urlencode", "setlang=en"])
                .arg("https://www.bing.com/search"),
        };
        let out = curl
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => "search needs `curl` on PATH".to_string(),
                _ => format!("could not run curl: {e}"),
            })?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        let body = String::from_utf8_lossy(&out.stdout);
        let (page, status) = body.rsplit_once('\n').unwrap_or(("", &body));
        match status.trim() {
            "200" => Ok(page.to_string()),
            status => Err(format!("answered {status}")),
        }
    }
}

#[async_trait]
impl Tool for Search {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Search the web and return the results as a numbered list: title, \
         URL and the engine's snippet, markup removed. Use it to find the \
         page that answers a question, then `fetch` that page to read it; a \
         snippet is a hint, not a source. Asks DuckDuckGo, then Bing if that \
         fails; name `engine` to ask only one. Engines sometimes refuse \
         automated requests, and a refusal is reported as one, never as no \
         results."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What to search for." },
                "engine": {
                    "type": "string",
                    "enum": ["duckduckgo", "bing"],
                    "description": "Ask only this engine. Default: duckduckgo, then bing.",
                },
                "limit": { "type": "integer", "description": "Results to return. Default 8, max 20." },
            },
            "required": ["query"],
            "additionalProperties": false,
        })
    }

    fn tier(&self) -> Tier {
        Tier::Net
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = tool::parse_args(args)?;
        let query = args.query.trim();
        if query.is_empty() {
            return Err(ToolError::Invalid(
                "an empty query searches for nothing".into(),
            ));
        }
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let engines = match args.engine {
            Some(one) => vec![one],
            None => Engine::ALL.to_vec(),
        };

        let mut failed = Vec::new();
        for engine in engines {
            let page = tokio::select! {
                page = self.page(engine, query) => page,
                _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            };
            // A parse is milliseconds of CPU: off the runtime's threads.
            let hits = match page {
                Ok(html) => tokio::task::spawn_blocking(move || read(engine, &html))
                    .await
                    .unwrap_or_else(|e| Err(format!("reading its page failed: {e}"))),
                Err(why) => Err(why),
            };
            tracing::info!(
                target: "pi::search",
                engine = engine.name(),
                ok = hits.is_ok(),
                "searched"
            );
            match hits.and_then(|hits| on_topic(query, hits)) {
                Ok(hits) => {
                    let mut body = listing(engine, query, &hits[..hits.len().min(limit)]);
                    for (engine, why) in &failed {
                        body.push_str(&format!("\nnote: {engine} failed first: {why}\n"));
                    }
                    let preview = format!("{} · {} results", engine.name(), hits.len().min(limit));
                    return Ok(ToolOutput::text(body).with_preview(preview));
                }
                Err(why) => failed.push((engine.name(), why)),
            }
        }
        let why: Vec<String> = failed
            .iter()
            .map(|(engine, why)| format!("{engine}: {why}"))
            .collect();
        Err(ToolError::Invalid(format!(
            "no engine answered — {}",
            why.join("; ")
        )))
    }
}

// Results that share no word with the query are not an answer: an engine
// that takes the client for a bot may serve unrelated ones instead of refusing.
fn on_topic(query: &str, hits: Vec<Hit>) -> Result<Vec<Hit>, String> {
    // `serde_json` is answered by pages about serde: compounds count by part.
    let words: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 2)
        .collect();
    let mentions = |hit: &Hit| {
        let seen = format!("{} {} {}", hit.title, hit.url, hit.snippet).to_lowercase();
        words.iter().any(|w| seen.contains(w.as_str()))
    };
    if hits.is_empty() || words.is_empty() || hits.iter().any(mentions) {
        Ok(hits)
    } else {
        Err(
            "its results have nothing to do with the query — likely served to a \
             suspected bot"
                .into(),
        )
    }
}

// Where an engine's page keeps what this tool reads.
struct Layout {
    result: &'static str,
    title: &'static str,
    snippet: &'static str,
    target: fn(&str) -> String,
    // Present only when nothing matched.
    none: &'static str,
    // Present only on its bot check, when that is known.
    blocked: Option<&'static str>,
}

impl Engine {
    fn layout(self) -> Layout {
        match self {
            Engine::Duckduckgo => Layout {
                result: "div.result:not(.result--ad)",
                title: "a.result__a",
                snippet: ".result__snippet",
                target: duckduckgo_target,
                none: ".no-results",
                blocked: Some("[class^=anomaly-modal]"),
            },
            Engine::Bing => Layout {
                result: "li.b_algo",
                title: "h2 a",
                snippet: ".b_caption p, p",
                target: bing_target,
                none: ".b_no",
                blocked: None,
            },
        }
    }
}

// The results on an engine's page; a page with none is a refusal or a
// changed layout, which the caller hears about as such.
fn read(engine: Engine, html: &str) -> Result<Vec<Hit>, String> {
    let doc = Html::parse_document(html);
    let layout = engine.layout();
    let (title, snippet) = (selector(layout.title), selector(layout.snippet));
    let hits: Vec<Hit> = doc
        .select(&selector(layout.result))
        .filter_map(|r| {
            let a = r.select(&title).next()?;
            let url = a.value().attr("href")?;
            Some(Hit {
                title: text(a),
                url: (layout.target)(url),
                snippet: r.select(&snippet).next().map(text).unwrap_or_default(),
            })
        })
        .collect();
    let found = |css: &str| doc.select(&selector(css)).next().is_some();
    if !hits.is_empty() || found(layout.none) {
        Ok(hits)
    } else if layout.blocked.is_some_and(found) {
        Err("it answered with a bot check instead of results".into())
    } else {
        Err(
            "its page held no results this tool can read — a bot check, or a \
             changed layout"
                .into(),
        )
    }
}

// Its links mostly point straight at the page; some go through its redirect,
// which carries the target in `uddg`.
fn duckduckgo_target(href: &str) -> String {
    let absolute = match href.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => href.to_string(),
    };
    reqwest::Url::parse(&absolute)
        .ok()
        .filter(|u| u.host_str() == Some("duckduckgo.com") && u.path() == "/l/")
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "uddg")
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or(absolute)
}

// Bing links through `/ck/a`, the target base64url-encoded in `u` behind an
// `a1`; anything else is already the target.
fn bing_target(href: &str) -> String {
    reqwest::Url::parse(href)
        .ok()
        .filter(|u| u.path() == "/ck/a")
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "u")
                .map(|(_, v)| v.into_owned())
        })
        .and_then(|u| {
            let encoded = u.strip_prefix("a1")?;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(encoded.trim_end_matches('='))
                .ok()?;
            String::from_utf8(bytes).ok()
        })
        .unwrap_or_else(|| href.to_string())
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).expect("the selectors here are fixed and valid")
}

// An element's text as a reader sees it: entities decoded, whitespace folded.
fn text(el: ElementRef) -> String {
    el.text()
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn listing(engine: Engine, query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("{} found nothing for “{query}”.\n", engine.name());
    }
    let mut out = format!("{} results for “{query}”:\n", engine.name());
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}\n", i + 1, hit.title, hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The parts of each engine's page this tool reads, as the engine serves
    // them; ads and markup inside titles included.
    #[test]
    fn each_engine_s_page_reads_as_titles_links_and_snippets() {
        let ddg = r#"<div id="links">
            <div class="result results_links result--ad"><a class="result__a" href="https://ad.example/">Ad</a></div>
            <div class="result results_links web-result">
              <h2 class="result__title"><a class="result__a" href="https://tokio.rs/">Tokio - An <b>async</b> runtime</a></h2>
              <a class="result__snippet" href="https://tokio.rs/"><b>Tokio</b> is a library &amp; runtime.</a>
            </div>
            <div class="result web-result"><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdocs.rs%2Ftokio&amp;rut=x">docs</a></div>
        </div>"#;
        let hits = read(Engine::Duckduckgo, ddg).unwrap();
        assert_eq!(
            hits[0],
            Hit {
                title: "Tokio - An async runtime".into(),
                url: "https://tokio.rs/".into(),
                snippet: "Tokio is a library & runtime.".into(),
            }
        );
        assert_eq!(hits[1].url, "https://docs.rs/tokio");
        assert_eq!(hits.len(), 2, "the ad is left out");

        let bing = r#"<ol id="b_results"><li class="b_algo">
            <h2><a href="https://www.bing.com/ck/a?!&amp;&amp;p=x&amp;u=a1aHR0cHM6Ly9ydXN0LWxhbmcub3JnLw&amp;ntb=1"><strong>Rust</strong> Programming Language</a></h2>
            <div class="b_caption"><p class="b_lineclamp2">Rust is blazingly fast.</p></div>
        </li></ol>"#;
        let hits = read(Engine::Bing, bing).unwrap();
        assert_eq!(hits[0].title, "Rust Programming Language");
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert_eq!(hits[0].snippet, "Rust is blazingly fast.");
    }

    // A refusal and a changed layout are failures to report, never an empty
    // list that reads as "nothing on the web".
    #[test]
    fn a_page_without_results_says_which_kind_it_is() {
        let check = "<html><div class=anomaly-modal__mask></div><div class=anomaly-modal__modal>Select all squares</div></html>";
        let err = read(Engine::Duckduckgo, check).unwrap_err();
        assert!(err.contains("bot check"), "{err}");

        let changed = "<html><div class=new-layout>results</div></html>";
        let err = read(Engine::Bing, changed).unwrap_err();
        assert!(err.contains("layout"), "{err}");
        let script = "<html><script>loadCaptcha()</script></html>";
        assert!(
            !read(Engine::Duckduckgo, script)
                .unwrap_err()
                .contains("instead")
        );

        let none = "<html><div class=no-results>No results.</div></html>";
        assert_eq!(read(Engine::Duckduckgo, none).unwrap(), vec![]);

        let hit = |title: &str| Hit {
            title: title.into(),
            url: "https://example.com/".into(),
            snippet: String::new(),
        };
        assert!(on_topic("ratatui rust", vec![hit("Cheap motels in Las Vegas")]).is_err());
        assert!(on_topic("Ratatui", vec![hit("x"), hit("ratatui docs")]).is_ok());
        assert!(on_topic("杭州 天气", vec![hit("杭州市_百度百科")]).is_ok());
        assert!(on_topic("serde_json", vec![hit("Overview · Serde")]).is_ok());
    }
}
