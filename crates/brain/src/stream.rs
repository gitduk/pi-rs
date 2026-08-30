use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

use crate::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
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
    ToolCall {
        id: Option<String>,
        name: String,
    },
}

/// Normalized stream deltas. `index` addresses a content block within the
/// current assistant message; transports that have no native index synthesize one.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    MessageStart {
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
    /// Raw JSON fragment; accumulated per index and parsed when the block closes.
    ToolArgsDelta {
        index: usize,
        delta: String,
    },
    BlockEnd {
        index: usize,
    },
    /// The turn's finished content, handed over whole rather than folded from
    /// the deltas before it.
    ///
    /// Responses puts the complete `output[]` on `response.completed`, so the
    /// deltas are free to be what they are — something to paint on screen, with
    /// no claim to correctness. It is also the only way to get
    /// `encrypted_content` intact: the copy on `output_item.added` may be
    /// truncated, and a truncated one is not rejected, just unreadable next
    /// turn. Anthropic's terminal frame carries usage and nothing else, so that
    /// wire still folds.
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
}

/// Folds normalized events into one assistant message. Shared by every
/// transport: the events are already normalized, so this exists exactly once.
#[derive(Debug)]
pub struct Accumulator {
    /// The model these blocks are attributed to, as the endpoint names it.
    by: String,
    blocks: BTreeMap<usize, Block>,
    /// Set by a wire whose terminal frame states the turn outright; the folded
    /// blocks are then only what was painted while it arrived.
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
        match ev {
            StreamEvent::MessageStart { usage } => {
                self.usage = usage;
            }
            StreamEvent::BlockStart { index, kind } => {
                self.blocks.entry(index).or_default().kind = Some(kind);
            }
            StreamEvent::TextDelta { index, delta }
            | StreamEvent::ToolArgsDelta { index, delta } => {
                self.blocks.entry(index).or_default().text.push_str(&delta);
            }
            StreamEvent::ReasoningDelta { index, delta } => {
                let b = self.blocks.entry(index).or_default();
                b.kind.get_or_insert(BlockKind::Reasoning);
                b.text.push_str(&delta);
            }
            StreamEvent::ReasoningSignature { index, signature } => {
                self.blocks.entry(index).or_default().signature = Some(signature);
            }
            StreamEvent::BlockEnd { .. } => {}
            StreamEvent::Complete { content, invalid } => {
                self.complete = Some((content, invalid));
            }
            StreamEvent::Done { stop, usage } => {
                self.stop = stop;
                // A terminal frame that omits a counter reports 0; never let
                // that clobber a count an earlier frame already delivered.
                if usage.input > 0 {
                    self.usage.input = usage.input;
                }
                if usage.output > 0 {
                    self.usage.output = usage.output;
                }
                if usage.cache_read > 0 {
                    self.usage.cache_read = usage.cache_read;
                }
                if usage.cache_write > 0 {
                    self.usage.cache_write = usage.cache_write;
                }
            }
        }
    }

    /// The provider's own id, or a local stand-in when it named none. Only
    /// gateways that drop the field reach the second arm; both native formats
    /// make the id mandatory.
    ///
    /// Ids repeated within one turn are deliberately not rewritten. Every
    /// archive checked had the provider's id used verbatim, and a host that
    /// repeats one is answered by the wire's own duplicate-id refusal rather
    /// than by carrying a de-duplicating table for a case never observed.
    fn call_id(&mut self, provider: Option<String>) -> String {
        provider.unwrap_or_else(|| {
            self.next_local_id += 1;
            format!("call_{}", self.next_local_id)
        })
    }

    pub fn finish(mut self) -> Completion {
        let (mut content, mut invalid) = match self.complete.take() {
            Some(stated) => stated,
            None => self.fold(),
        };

        // Every call gets exactly one result, and a result is addressed by the
        // call's id — so an id that arrives twice cannot be answered at all.
        // Kept: the first, which is the one the deltas filled in. Observed once
        // against a translating gateway and not reproducible on demand, so this
        // guards the invariant rather than the cause: whatever sends it, the
        // turn is unsendable the moment two calls share an id.
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut repeated = Vec::new();
        content.retain(|c| match c {
            AssistantContent::ToolCall(call) => {
                let first = seen.insert(call.id.clone());
                if !first {
                    repeated.push(call.id.clone());
                }
                first
            }
            _ => true,
        });
        for id in &repeated {
            tracing::warn!(
                target: "pi::wire", call = %id,
                "the turn named this call twice; the repeat was dropped"
            );
        }

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

    /// The turn rebuilt from the deltas, for a wire whose terminal frame does
    /// not state it.
    fn fold(&mut self) -> (Vec<AssistantContent>, Vec<InvalidToolArgs>) {
        let mut content = Vec::new();
        let mut invalid = Vec::new();
        for (_, b) in std::mem::take(&mut self.blocks) {
            match b.kind {
                Some(BlockKind::ToolCall { id, name }) => {
                    let id = self.call_id(id);
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
                Some(BlockKind::Reasoning) => {
                    content.push(AssistantContent::Reasoning(Reasoning {
                        id: None,
                        content: vec![ReasoningContent::Text {
                            text: b.text,
                            signature: b.signature,
                        }],
                        by: Some(self.by.clone()),
                    }));
                }
                _ => {
                    if !b.text.is_empty() {
                        content.push(AssistantContent::Text(Text { text: b.text }));
                    }
                }
            }
        }
        (content, invalid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A result is addressed by its call's id, so two calls sharing one can
    /// never both be answered — the next request is invalid whichever result
    /// goes back. Seen once against a translating gateway.
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
        // Both native formats make the id mandatory, so only a gateway that
        // drops it reaches this path — and the pair still has to agree, since
        // the result keys on whatever the call carries.
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

    #[test]
    fn reasoning_carries_its_origin_and_signature() {
        let mut a = acc();
        a.push(StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::Reasoning,
        });
        a.push(StreamEvent::ReasoningDelta {
            index: 0,
            delta: "step".into(),
        });
        a.push(StreamEvent::ReasoningSignature {
            index: 0,
            signature: "sig".into(),
        });
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
        assert!(matches!(&content[1], AssistantContent::Text(t) if t.text == "answer"));
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
