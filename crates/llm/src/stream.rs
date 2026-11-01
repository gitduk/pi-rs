use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::message::{AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Usage {
    /// The prompt as the provider counted it. Transports report cached tokens
    /// apart from `input`, so the prompt is all three.
    pub fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }

    /// Fold another call's counters in. Every field, so a counter added to the
    /// struct without being added here is a compile error rather than a number
    /// that quietly stops accumulating.
    pub fn add(&mut self, other: &Usage) {
        let Usage {
            input,
            output,
            cache_read,
            cache_write,
        } = other;
        self.input += input;
        self.output += output;
        self.cache_read += cache_read;
        self.cache_write += cache_write;
    }

    /// Take in what `ev` says of one call's counts: a start states them
    /// whole; later frames only the ones they carry, since one that omits a
    /// counter reports 0 and must not clobber a count already delivered.
    pub fn observe(&mut self, ev: &StreamEvent) {
        match ev {
            StreamEvent::MessageStart { usage } => *self = *usage,
            StreamEvent::Usage { usage } | StreamEvent::Done { usage, .. } => {
                if usage.input > 0 {
                    self.input = usage.input;
                }
                if usage.output > 0 {
                    self.output = usage.output;
                }
                if usage.cache_read > 0 {
                    self.cache_read = usage.cache_read;
                }
                if usage.cache_write > 0 {
                    self.cache_write = usage.cache_write;
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopReason {
    #[default]
    EndTurn,
    ToolUse,
    MaxTokens,
    Refusal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    Text,
    Reasoning,
    // Reasoning the provider sealed: its whole opaque payload rides the start.
    RedactedReasoning { data: String },
    ToolCall { id: Option<String>, name: String },
}

/// Normalized stream deltas. `index` addresses a content block within the
/// current assistant message; transports that have no native index synthesize one.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    MessageStart {
        usage: Usage,
    },
    // Provider-reported cumulative counts for this call, not an increment.
    // A counter omitted by the provider is zero and leaves its last value intact.
    Usage {
        usage: Usage,
    },
    BlockStart {
        index: usize,
        kind: BlockKind,
    },
    TextDelta {
        index: usize,
        delta: String,
    },
    ReasoningDelta {
        index: usize,
        delta: String,
    },
    ReasoningSignature {
        index: usize,
        signature: String,
    },
    // Raw JSON fragment; accumulated per index and parsed when the block closes.
    ToolArgsDelta {
        index: usize,
        delta: String,
    },
    BlockEnd {
        index: usize,
    },
    // Handed over whole, not folded from deltas: Responses sends the complete
    // `output[]` on completion; deltas may carry truncated `encrypted_content`.
    Complete {
        content: Vec<AssistantContent>,
        invalid: Vec<InvalidToolArgs>,
    },
    Done {
        stop: StopReason,
        usage: Usage,
    },
}

/// A tool call whose streamed arguments were not valid JSON. The call still
/// enters the message with `{}` so the wire stays balanced; the loop owes the
/// model an error result for it.
#[derive(Debug, Clone, PartialEq)]
pub struct InvalidToolArgs {
    pub call: String,
    pub name: String,
    pub raw: String,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub message: Message,
    pub invalid: Vec<InvalidToolArgs>,
    pub stop: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Default)]
struct Block {
    kind: Option<BlockKind>,
    text: String,
    signature: Option<String>,
    // The wire closed it, so nothing more of it was coming.
    ended: bool,
}

/// Folds normalized events into one assistant message. Shared by every
/// transport: the events are already normalized, so this exists exactly once.
#[derive(Debug)]
pub struct Accumulator {
    // The model these blocks are attributed to, as the endpoint names it.
    by: String,
    blocks: BTreeMap<usize, Block>,
    // Set by a wire whose terminal frame states the turn outright; the folded
    // blocks are then only what was painted while it arrived.
    complete: Option<(Vec<AssistantContent>, Vec<InvalidToolArgs>)>,
    stop: StopReason,
    usage: Usage,
    next_local_id: u64,
}

impl Accumulator {
    pub fn new(by: String) -> Self {
        Self {
            by,
            blocks: BTreeMap::new(),
            complete: None,
            stop: StopReason::default(),
            usage: Usage::default(),
            next_local_id: 0,
        }
    }

    pub fn push(&mut self, ev: StreamEvent) {
        self.usage.observe(&ev);
        match ev {
            StreamEvent::MessageStart { .. } | StreamEvent::Usage { .. } => {}
            StreamEvent::BlockStart { index, kind } => {
                self.blocks.entry(index).or_default().kind = Some(kind);
            }
            StreamEvent::TextDelta { index, delta } => {
                self.blocks.entry(index).or_default().text.push_str(&delta);
            }
            // Arguments for a call whose start the wire dropped (it said so):
            // folded as text they would read as the model's words.
            StreamEvent::ToolArgsDelta { index, delta } => {
                if let Some(b) = self.blocks.get_mut(&index)
                    && matches!(b.kind, Some(BlockKind::ToolCall { .. }))
                {
                    b.text.push_str(&delta);
                }
            }
            StreamEvent::ReasoningDelta { index, delta } => {
                let b = self.blocks.entry(index).or_default();
                b.kind.get_or_insert(BlockKind::Reasoning);
                b.text.push_str(&delta);
            }
            StreamEvent::ReasoningSignature { index, signature } => {
                self.blocks.entry(index).or_default().signature = Some(signature);
            }
            StreamEvent::BlockEnd { index } => {
                if let Some(b) = self.blocks.get_mut(&index) {
                    b.ended = true;
                }
            }
            StreamEvent::Complete { content, invalid } => {
                self.complete = Some((content, invalid));
            }
            StreamEvent::Done { stop, .. } => self.stop = stop,
        }
    }

    pub fn usage(&self) -> Usage {
        self.usage
    }

    // Provider's own id, or a local stand-in if none. Duplicate ids within a
    // turn are not rewritten — the wire's own duplicate-id refusal handles that.
    fn call_id(&mut self, provider: Option<String>) -> String {
        provider.unwrap_or_else(|| {
            self.next_local_id += 1;
            format!("call_{}", self.next_local_id)
        })
    }

    pub fn finish(self) -> Completion {
        self.close(true)
    }

    /// What arrived before the stream was stopped. Reasoning the wire never
    /// closed is kept to read but not attributed, so it never replays: its
    /// signature, which comes last, is missing.
    pub fn cut_short(self) -> Completion {
        self.close(false)
    }

    fn close(mut self, whole: bool) -> Completion {
        let (mut content, mut invalid) = match self.complete.take() {
            Some(stated) => stated,
            None => self.fold(whole),
        };

        // A host that omits finish_reason would otherwise end the turn with
        // tool calls pending; the calls themselves are the authority.
        let has_calls = content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)));
        let stop = match self.stop {
            StopReason::EndTurn if has_calls => StopReason::ToolUse,
            other => other,
        };

        content.shrink_to_fit();
        invalid.shrink_to_fit();
        Completion {
            message: Message::Assistant { content },
            invalid,
            stop,
            usage: self.usage,
        }
    }

    // The turn rebuilt from the deltas, for a wire whose terminal frame does
    // not state it.
    fn fold(&mut self, whole: bool) -> (Vec<AssistantContent>, Vec<InvalidToolArgs>) {
        let mut content = Vec::new();
        let mut invalid = Vec::new();
        let mut seen = BTreeSet::new();
        for (_, b) in std::mem::take(&mut self.blocks) {
            match b.kind {
                Some(BlockKind::ToolCall { id, name }) => {
                    let id = self.call_id(id);
                    if repeated(&mut seen, &id) {
                        continue;
                    }
                    let raw = if b.text.trim().is_empty() {
                        "{}"
                    } else {
                        b.text.trim()
                    };
                    let args = match serde_json::from_str::<Value>(raw) {
                        Ok(v) => v,
                        Err(e) => {
                            invalid.push(InvalidToolArgs {
                                call: id.clone(),
                                name: name.clone(),
                                raw: b.text.clone(),
                                error: e.to_string(),
                            });
                            json!({})
                        }
                    };
                    content.push(AssistantContent::ToolCall(ToolCall { id, name, args }));
                }
                Some(BlockKind::RedactedReasoning { data }) => {
                    content.push(AssistantContent::Reasoning(Reasoning {
                        id: None,
                        content: vec![ReasoningContent::Encrypted(data)],
                        by: (whole || b.ended).then(|| self.by.clone()),
                    }));
                }
                Some(BlockKind::Reasoning) => {
                    if !b.text.is_empty() || b.signature.is_some() {
                        content.push(AssistantContent::Reasoning(Reasoning {
                            id: None,
                            content: vec![ReasoningContent::Text {
                                text: b.text,
                                signature: b.signature,
                            }],
                            by: (whole || b.ended).then(|| self.by.clone()),
                        }));
                    }
                }
                // Whitespace alone says nothing, and some wires refuse it back.
                _ => {
                    if !b.text.trim().is_empty() {
                        content.push(AssistantContent::Text(Text { text: b.text }));
                    }
                }
            }
        }
        (content, invalid)
    }
}

/// Whether `id` already named a call in this turn. A result is addressed by
/// its call's id, so a repeat can't be answered at all: the first (the one the
/// deltas filled in) is kept, and the repeat goes before its arguments are
/// judged — its fault must not land on the call that stays.
pub(crate) fn repeated(seen: &mut BTreeSet<String>, id: &str) -> bool {
    if seen.insert(id.to_string()) {
        return false;
    }
    tracing::warn!(
        target: "pi::wire", call = %id,
        "the turn named this call twice; the repeat was dropped"
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two calls sharing an id can never both be answered — the next request
    // is invalid regardless of which result goes back.
    #[test]
    fn a_call_id_the_turn_names_twice_is_answered_once() {
        let mut a = acc();
        for (index, args) in [(0usize, "{\"path\": \"a.txt\"}"), (1, "")] {
            a.push(StreamEvent::BlockStart {
                index,
                kind: BlockKind::ToolCall {
                    id: Some("call_same".into()),
                    name: "read".into(),
                },
            });
            if !args.is_empty() {
                a.push(StreamEvent::ToolArgsDelta {
                    index,
                    delta: args.into(),
                });
            }
            a.push(StreamEvent::BlockEnd { index });
        }
        let done = a.finish();
        let Message::Assistant { content } = &done.message else {
            panic!("assistant")
        };
        let calls: Vec<_> = content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "{content:?}");
        // The one the deltas filled in, not the empty repeat.
        assert_eq!(calls[0].args["path"], "a.txt");
        assert_eq!(done.stop, StopReason::ToolUse);
    }

    // The repeat's bad JSON must not refuse the call that stays.
    #[test]
    fn a_repeated_call_s_bad_arguments_do_not_land_on_the_one_kept() {
        let mut a = acc();
        for (index, args) in [(0usize, "{\"path\": \"a.txt\"}"), (1, "{oops")] {
            a.push(StreamEvent::BlockStart {
                index,
                kind: BlockKind::ToolCall {
                    id: Some("call_same".into()),
                    name: "read".into(),
                },
            });
            a.push(StreamEvent::ToolArgsDelta {
                index,
                delta: args.into(),
            });
            a.push(StreamEvent::BlockEnd { index });
        }
        let done = a.finish();
        assert_eq!(done.message.tool_calls().count(), 1);
        assert!(done.invalid.is_empty(), "{:?}", done.invalid);
    }

    // A call whose start was lost leaves its arguments with nowhere to go;
    // they must not surface as prose.
    #[test]
    fn arguments_without_their_call_are_not_text() {
        let mut a = acc();
        a.push(StreamEvent::ToolArgsDelta {
            index: 0,
            delta: "{\"path\": \"a.rs\"}".into(),
        });
        a.push(StreamEvent::BlockEnd { index: 0 });
        let done = a.finish();
        let Message::Assistant { content } = &done.message else {
            panic!("assistant")
        };
        assert!(content.is_empty(), "{content:?}");
    }

    fn acc() -> Accumulator {
        Accumulator::new("test-model".into())
    }

    #[test]
    fn assembles_tool_args_across_deltas() {
        let mut a = acc();
        a.push(StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall {
                id: Some("toolu_1".into()),
                name: "read".into(),
            },
        });
        for chunk in [r#"{"pa"#, r#"th":"#, r#""a.rs"}"#] {
            a.push(StreamEvent::ToolArgsDelta {
                index: 0,
                delta: chunk.into(),
            });
        }
        a.push(StreamEvent::BlockEnd { index: 0 });
        a.push(StreamEvent::Done {
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        });

        let done = a.finish();
        assert!(done.invalid.is_empty());
        let calls: Vec<_> = done.message.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args["path"], "a.rs");
        assert_eq!(calls[0].id, "toolu_1");
    }

    #[test]
    fn a_call_the_provider_did_not_name_gets_a_local_id() {
        // Both native formats make the id mandatory; only a gateway that drops
        // it reaches this path, and the result must key on whatever it carries.
        let mut a = acc();
        for (i, (id, name)) in [(Some("toolu_1"), "read"), (None, "grep")]
            .into_iter()
            .enumerate()
        {
            a.push(StreamEvent::BlockStart {
                index: i,
                kind: BlockKind::ToolCall {
                    id: id.map(str::to_string),
                    name: name.into(),
                },
            });
            a.push(StreamEvent::ToolArgsDelta {
                index: i,
                delta: "{}".into(),
            });
            a.push(StreamEvent::BlockEnd { index: i });
        }
        a.push(StreamEvent::Done {
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        });

        let done = a.finish();
        let calls: Vec<_> = done.message.tool_calls().collect();
        assert_eq!(calls.len(), 2);
        // Named ids are passed through untouched; two calls sharing one id is
        // the provider's bug, not something to paper over by renaming.
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[1].id, "call_1");
    }

    #[test]
    fn malformed_args_still_produce_a_balanced_call() {
        let mut a = acc();
        a.push(StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall {
                id: None,
                name: "read".into(),
            },
        });
        a.push(StreamEvent::ToolArgsDelta {
            index: 0,
            delta: r#"{"path": "#.into(),
        });
        a.push(StreamEvent::Done {
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        });

        let done = a.finish();
        assert_eq!(done.invalid.len(), 1);
        assert_eq!(done.invalid[0].name, "read");
        // The call must still exist, or the next request has a tool_result
        // answering nothing.
        let calls: Vec<_> = done.message.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args, json!({}));
        assert_eq!(calls[0].id, done.invalid[0].call);
    }

    #[test]
    fn terminal_zeros_never_clobber_a_live_count() {
        let mut a = acc();
        a.push(StreamEvent::MessageStart {
            usage: Usage {
                input: 120,
                cache_read: 40,
                ..Default::default()
            },
        });
        a.push(StreamEvent::Usage {
            usage: Usage {
                output: 5,
                ..Default::default()
            },
        });
        assert_eq!(
            (a.usage().input, a.usage().output, a.usage().cache_read),
            (120, 5, 40)
        );
        a.push(StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: Usage {
                output: 7,
                ..Default::default()
            },
        });

        let done = a.finish();
        assert_eq!(
            done.usage,
            Usage {
                input: 120,
                output: 7,
                cache_read: 40,
                cache_write: 0
            }
        );
    }

    // Stopped mid-thought: the open block is kept to read but never replays,
    // while one the wire closed before the stop still does.
    #[test]
    fn reasoning_cut_off_before_its_end_is_not_attributed() {
        let mut a = acc();
        for index in [0, 1] {
            a.push(StreamEvent::BlockStart {
                index,
                kind: BlockKind::Reasoning,
            });
            a.push(StreamEvent::ReasoningDelta {
                index,
                delta: "step".into(),
            });
        }
        a.push(StreamEvent::BlockEnd { index: 0 });

        let Message::Assistant { content, .. } = a.cut_short().message else {
            panic!()
        };
        let by: Vec<_> = (content.iter())
            .map(|c| match c {
                AssistantContent::Reasoning(r) => (r.text(), r.by.clone()),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(
            by,
            [
                ("step".into(), Some("test-model".into())),
                ("step".into(), None)
            ]
        );
    }

    // A reasoning block opened at 0 holding `inside`, then the answer as
    // text at 1, folded into the turn's content.
    fn reasoned(inside: Vec<StreamEvent>) -> Vec<AssistantContent> {
        let mut a = acc();
        a.push(StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::Reasoning,
        });
        for event in inside {
            a.push(event);
        }
        a.push(StreamEvent::TextDelta {
            index: 1,
            delta: "answer".into(),
        });
        a.push(StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        });
        let Message::Assistant { content, .. } = a.finish().message else {
            panic!()
        };
        content
    }

    #[test]
    fn reasoning_carries_its_origin_and_signature() {
        let content = reasoned(vec![
            StreamEvent::ReasoningDelta {
                index: 0,
                delta: "step".into(),
            },
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: "sig".into(),
            },
        ]);
        // Reasoning must precede text: Anthropic rejects the other order.
        let AssistantContent::Reasoning(r) = &content[0] else {
            panic!("{content:?}")
        };
        assert_eq!(r.by.as_deref(), Some("test-model"));
        assert_eq!(
            r.content[0],
            ReasoningContent::Text {
                text: "step".into(),
                signature: Some("sig".into())
            }
        );
        assert!(matches!(&content[1], AssistantContent::Text(_)));
    }

    #[test]
    fn empty_reasoning_block_is_omitted() {
        let content = reasoned(Vec::new());
        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], AssistantContent::Text(t) if t.text == "answer"));
    }
    #[test]
    fn a_missing_finish_reason_still_ends_the_turn_as_tool_use() {
        let mut a = acc();
        a.push(StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall {
                id: None,
                name: "read".into(),
            },
        });
        a.push(StreamEvent::ToolArgsDelta {
            index: 0,
            delta: "{}".into(),
        });
        a.push(StreamEvent::Done {
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        });

        // The pending calls are the authority, not the host's stop field.
        assert_eq!(a.finish().stop, StopReason::ToolUse);
    }
}
