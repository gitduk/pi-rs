use serde_json::{Value, json};
use std::collections::BTreeMap;

use crate::message::{
    AssistantContent, Message, Origin, ProviderCallId, Reasoning, ReasoningContent, Text, ToolCall,
    ToolCallId,
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
        provider: Option<ProviderCallId>,
        name: String,
    },
}

/// Normalized stream deltas. `index` addresses a content block within the
/// current assistant message; transports that have no native index synthesize one.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    MessageStart {
        id: Option<String>,
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
    pub call: ToolCallId,
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
    origin: Origin,
    id: Option<String>,
    blocks: BTreeMap<usize, Block>,
    stop: StopReason,
    usage: Usage,
    next_local_id: u64,
}

impl Accumulator {
    pub fn new(transport: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            origin: Origin {
                transport: transport.into(),
                model: model.into(),
            },
            id: None,
            blocks: BTreeMap::new(),
            stop: StopReason::default(),
            usage: Usage::default(),
            next_local_id: 0,
        }
    }

    pub fn push(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::MessageStart { id, usage } => {
                self.id = id;
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

    fn mint_id(&mut self, provider: Option<&ProviderCallId>) -> ToolCallId {
        match provider {
            Some(p) => ToolCallId(p.0.clone()),
            None => {
                self.next_local_id += 1;
                ToolCallId(format!("call_{}", self.next_local_id))
            }
        }
    }

    pub fn finish(mut self) -> Completion {
        let mut content = Vec::new();
        let mut invalid = Vec::new();
        let blocks = std::mem::take(&mut self.blocks);

        for (_, b) in blocks {
            match b.kind {
                Some(BlockKind::ToolCall { provider, name }) => {
                    let id = self.mint_id(provider.as_ref());
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
                    content.push(AssistantContent::ToolCall(ToolCall {
                        id,
                        provider,
                        name,
                        args,
                    }));
                }
                Some(BlockKind::Reasoning) => {
                    content.push(AssistantContent::Reasoning(Reasoning {
                        id: None,
                        content: vec![ReasoningContent::Text {
                            text: b.text,
                            signature: b.signature,
                        }],
                        origin: Some(self.origin.clone()),
                    }));
                }
                _ => {
                    if !b.text.is_empty() {
                        content.push(AssistantContent::Text(Text { text: b.text }));
                    }
                }
            }
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

        Completion {
            message: Message::Assistant {
                id: self.id,
                content,
            },
            invalid,
            stop,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc() -> Accumulator {
        Accumulator::new("anthropic", "test-model")
    }

    #[test]
    fn assembles_tool_args_across_deltas() {
        let mut a = acc();
        a.push(StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall {
                provider: Some(ProviderCallId("toolu_1".into())),
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
        // The provider's id is what may travel back on its wire.
        assert_eq!(calls[0].provider.as_ref().unwrap().as_str(), "toolu_1");
    }

    #[test]
    fn malformed_args_still_produce_a_balanced_call() {
        let mut a = acc();
        a.push(StreamEvent::BlockStart {
            index: 0,
            kind: BlockKind::ToolCall {
                provider: None,
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
            id: Some("msg_1".into()),
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
        assert_eq!(r.origin.as_ref().unwrap().model, "test-model");
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
                provider: None,
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
