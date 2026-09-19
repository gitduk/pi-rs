//! Typed snap judgments, delegated to a configured judgment model.
//!
//! The model behind this tool is not pi's: it answers questions about a state
//! rather than generating prose, so its answers branch, sort and threshold
//! directly. The schema — three question kinds over one state — is the
//! vendor-neutral contract; the wire an endpoint actually speaks is mapped
//! here and nowhere else.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Ctx, Tier, Tool, ToolError, ToolOutput, fetch::MAX_BYTES};

// A judgment is a sub-second answer; a hung endpoint must not stall the turn
// that asked for one much longer than that.
const TIMEOUT: Duration = Duration::from_secs(30);

// A broken endpoint's body reaches the model as the error's prose, so cap
// what it can pour into the transcript.
const MAX_ERROR_BODY: usize = 2_000;

/// The judgment endpoint configured under `[judge]`.
pub struct Judge {
    /// The origin as configured; the wire path is appended per request.
    base_url: String,
    /// Expanded already — the `$NAME` indirection belongs to config.
    api_key: Option<String>,
    /// The model to ask, as the endpoint names it. A call may name another.
    model: Option<String>,
    client: std::sync::OnceLock<reqwest::Client>,
}

impl Judge {
    pub fn new(base_url: String, api_key: Option<String>, model: Option<String>) -> Self {
        Self {
            base_url,
            api_key,
            model,
            client: std::sync::OnceLock::new(),
        }
    }

    // Built on first use. A TLS stack that will not start is a failure the
    // model should read, not one that takes the process down at startup.
    fn client(&self) -> Result<&reqwest::Client, ToolError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let built = reqwest::Client::builder()
            .user_agent(concat!("pi/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ToolError::Invalid(format!("no http client on this machine: {e}")))?;
        Ok(self.client.get_or_init(|| built))
    }

    /// Send the request and read the answer whole, under the wire cap.
    async fn answer(&self, body: &Value) -> Result<String, ToolError> {
        let url = format!("{}/systemone", self.base_url.trim_end_matches('/'));
        let sent = self.client()?.post(url);
        let sent = match &self.api_key {
            Some(key) => sent.bearer_auth(key),
            None => sent,
        };
        let mut response = sent
            .timeout(TIMEOUT)
            .json(body)
            .send()
            .await
            .map_err(|e| ToolError::Invalid(format!("judgment endpoint unreachable: {e}")))?;

        let status = response.status();
        // Read in chunks rather than whole: the length a server declares is
        // not the length it sends, and the cap has to hold either way.
        let mut raw = Vec::with_capacity(
            response.content_length().unwrap_or(0).min(MAX_BYTES as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await.map_err(|e| {
            ToolError::Invalid(format!("judgment endpoint stopped mid-response: {e}"))
        })? {
            let room = (MAX_BYTES + 1).saturating_sub(raw.len());
            raw.extend_from_slice(&chunk[..chunk.len().min(room)]);
            // One byte past the cap is proof this is not a judgment answer:
            // those are small JSON, so clipping means `base_url` is pointed
            // somewhere else, and half of that answer helps nobody.
            if raw.len() > MAX_BYTES {
                return Err(ToolError::Invalid(format!(
                    "judgment endpoint answered with over {} MiB — not a judgment response; check `base_url`",
                    MAX_BYTES / (1024 * 1024)
                )));
            }
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        if !status.is_success() {
            let detail: String = text.chars().take(MAX_ERROR_BODY).collect();
            return Err(ToolError::Invalid(format!(
                "judgment endpoint returned {status}: {detail}"
            )));
        }
        Ok(text)
    }
}

#[derive(Deserialize)]
struct Args {
    /// What the questions are judged against: prose, or an object the
    /// instructions reference by dot path.
    state: Value,
    /// Override the configured model for this call.
    #[serde(default)]
    model: Option<String>,
    /// Question id → the question. Ids reach the answer, never the model.
    questions: BTreeMap<String, Question>,
}

#[derive(Serialize, Deserialize)]
struct Question {
    #[serde(rename = "type")]
    kind: Kind,
    instructions: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    criteria: Option<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Choice,
    Score,
    Noul,
}

impl Kind {
    fn name(&self) -> &'static str {
        match self {
            Kind::Choice => "choice",
            Kind::Score => "score",
            Kind::Noul => "noul",
        }
    }
}

impl Question {
    // Checked here rather than left to the endpoint: the shape a kind takes
    // is this tool's contract, and a refusal the model can fix in place beats
    // a round trip that comes back as somebody else's 422.
    fn validate(&self, id: &str) -> Result<(), ToolError> {
        let bad = |want: &str| {
            Err(ToolError::Invalid(format!(
                "questions.{id}: a {} question takes `criteria` as {want}",
                self.kind.name()
            )))
        };
        match self.kind {
            Kind::Choice => {
                if !matches!(&self.criteria, Some(c) if c.is_object()) {
                    return bad("a map of `{option: description}`");
                }
            }
            Kind::Score => {
                if !matches!(&self.criteria, Some(c) if c.is_array()) {
                    return bad("an ordered array of levels, scored by position");
                }
            }
            Kind::Noul => {}
        }
        Ok(())
    }
}

#[async_trait]
impl Tool for Judge {
    fn name(&self) -> &str {
        "judge"
    }

    fn description(&self) -> &str {
        "Ask a judgment model typed questions about one state and get structured \
         answers: a choice among supplied options, a score along levels you \
         define, or the probability that a statement is true. Use it for snap \
         judgments you would otherwise reason through in-line — classifying \
         output, ranking options, checking a condition — and put every question \
         that shares the state into one call: they run in parallel, and adding \
         questions barely changes the response time. Each question asks one \
         thing a knowledgeable person could answer in seconds; split a bigger \
         judgment into separate questions and weigh the answers in your own \
         logic. Answers are constrained to the options or levels you supplied — \
         never generated prose — so branch, sort and threshold on them \
         directly. Not the model for writing, rewriting or open-ended \
         reasoning."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "state": {
                    "description": "What the questions are judged against: a \
                        string, or an object the instructions reference by dot \
                        path (e.g. `output.block_2`).",
                },
                "model": {
                    "type": "string",
                    "description": "Ask a different model than the configured \
                        one, named as the endpoint names it.",
                },
                "questions": {
                    "type": "object",
                    "description": "Question id → question. The id is yours — \
                        each answer comes back under it — so write the full \
                        question in `instructions`. Ask speculatively: a \
                        question whose answer only matters for some inputs is \
                        close to free.",
                    "additionalProperties": {
                        "type": "object",
                        "properties": {
                            "type": {
                                "enum": ["choice", "score", "noul"],
                                "description": "choice: pick one option. \
                                    score: a position along ordered levels. \
                                    noul: probability a statement is true.",
                            },
                            "instructions": {
                                "type": "string",
                                "description": "The question itself, complete \
                                    and self-standing; reference state parts \
                                    by dot path where it matters.",
                            },
                            "criteria": {
                                "description": "choice: `{option: \
                                    description}` map. score: ordered array \
                                    of levels. noul: optional `{yes, no}` \
                                    clarifications.",
                            },
                        },
                        "required": ["type", "instructions"],
                    },
                },
            },
            "required": ["state", "questions"],
        })
    }

    fn tier(&self) -> Tier {
        Tier::Net
    }

    async fn execute(&self, args: Value, ctx: &Ctx) -> Result<ToolOutput, ToolError> {
        let args: Args = crate::parse_args(args)?;
        for (id, question) in &args.questions {
            question.validate(id)?;
        }

        let mut body = json!({
            "state": args.state,
            "questions": args.questions,
        });
        if let Some(model) = args.model.as_ref().or(self.model.as_ref()) {
            body["model"] = json!(model);
        }

        // Raced, not awaited: Esc reaches this dial the way it reaches every
        // other blocking wait in the toolset.
        let text = tokio::select! {
            r = self.answer(&body) => r?,
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
        };
        // Readable, not verbatim: pretty for the model that reads it whole,
        // one digest line for the display that shows only the first. A body
        // outside the expected shape still passes through undecorated.
        let Ok(shaped) = serde_json::from_str::<Value>(&text) else {
            return Ok(ToolOutput::text(text));
        };
        let Ok(pretty) = serde_json::to_string_pretty(&shaped) else {
            return Ok(ToolOutput::text(text));
        };
        let mut out = ToolOutput::text(pretty);
        if let Some(line) = digest(shaped.get("answers")) {
            out = out.with_preview(line);
        }
        Ok(out)
    }
}

/// `phase=done  readiness=1.25  shipped_and_verified=0.95` — one value per
/// answer, the kind deciding which field is the value: a choice's option, a
/// score's position, a noul's probability. Distributions and legends stay in
/// the body; this line is for a glance, not a reading.
fn digest(answers: Option<&Value>) -> Option<String> {
    let answers = answers?.as_object()?;
    let mut line = String::new();
    for (id, answer) in answers {
        if !line.is_empty() {
            line.push_str("  ");
        }
        let value = match answer.get("type").and_then(Value::as_str) {
            Some("choice") => answer
                .get("choice")
                .and_then(Value::as_str)
                .map(str::to_string),
            Some(kind @ ("noul" | "score")) => answer.get(kind).map(Value::to_string),
            _ => None,
        };
        match value {
            Some(v) => {
                line.push_str(id);
                line.push('=');
                line.push_str(&v);
            }
            None => line.push_str(id),
        }
    }
    (!line.is_empty()).then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_digest_names_one_value_per_answer() {
        let answers = json!({
            "phase": {"type": "choice", "choice": "done", "confidence": 1.0},
            "readiness": {"type": "score", "score": 1.25},
            "urgent": {"type": "noul", "noul": 0.98},
            "odd": {"type": "noul"},
        });
        assert_eq!(
            digest(Some(&answers)).unwrap(),
            "odd  phase=done  readiness=1.25  urgent=0.98"
        );
        assert_eq!(digest(Some(&json!({}))), None);
        assert_eq!(digest(None), None);
    }

    fn question(kind: Kind, criteria: Option<Value>) -> Question {
        Question {
            kind,
            instructions: "q".into(),
            criteria,
        }
    }

    #[test]
    fn criteria_shapes_are_checked_before_the_round_trip() {
        let id = "q";
        assert!(
            question(Kind::Choice, Some(json!({"a": "b"})))
                .validate(id)
                .is_ok()
        );
        assert!(
            question(Kind::Choice, Some(json!(["a"])))
                .validate(id)
                .is_err()
        );
        assert!(question(Kind::Choice, None).validate(id).is_err());
        assert!(
            question(Kind::Score, Some(json!(["a", "b"])))
                .validate(id)
                .is_ok()
        );
        assert!(
            question(Kind::Score, Some(json!({"a": "b"})))
                .validate(id)
                .is_err()
        );
        assert!(question(Kind::Score, None).validate(id).is_err());
        assert!(question(Kind::Noul, None).validate(id).is_ok());
    }
}
