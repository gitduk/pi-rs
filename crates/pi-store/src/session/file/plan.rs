//! Turn a save into new facts. Protocol and context share the same bodies.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use message::{Request, WireKind};
use serde_json::{Value, json};

use super::record::{Address, Body, Item, Record, Start};
use super::{Location, Metadata, State, Transaction};

struct Candidate {
    address: Address,
    body: Vec<Value>,
}

pub(super) fn finish(
    at: &Location,
    before: &State,
    tx: &mut Transaction,
    end: u64,
) -> Result<Vec<Record>> {
    let header = header(at, tx, end);
    let batch = match &header {
        Some(header) => end + serde_json::to_vec(header)?.len() as u64 + 1,
        None => end,
    };
    project(at, tx, batch)?;
    metadata(before, tx)?;
    let mut records = Vec::with_capacity(tx.records.len() + usize::from(header.is_some()));
    if let Some(header) = header {
        records.push(header);
    }
    records.append(&mut tx.records);
    Ok(records)
}

fn project(at: &Location, tx: &mut Transaction, batch: u64) -> Result<()> {
    let has_input = tx
        .records
        .iter()
        .any(|record| matches!(record, Record::Input(item) if item.entry.is_some()));
    // Native-only child saves have nothing to project onto protocol records.
    if tx.pending.is_empty() && (!has_input || at.cached_index().unbound.is_empty()) {
        return Ok(());
    }
    let (mut candidates, unbound, originals) = retained(at, &tx.pending, has_input)?;
    let fresh = fresh(&tx.records, batch)?;
    let fresh_start = candidates.len();
    candidates.extend(fresh);
    for (position, entry) in std::mem::take(&mut tx.pending) {
        attach(tx, position, entry, batch, &candidates, &originals)?;
    }
    reference_context(&mut tx.records, &candidates[fresh_start..], &unbound);
    Ok(())
}

fn header(at: &Location, tx: &Transaction, end: u64) -> Option<Record> {
    if end == 0 {
        let metadata = tx.state.metadata.clone().unwrap_or_else(|| Metadata {
            workspace: String::new(),
            model: String::new(),
            name: None,
            title: None,
            created: 0,
            spent: Default::default(),
            parent: None,
            touched: 0,
            rounds: 0,
            prompt: String::new(),
        });
        let start = Start::new(at.session.clone(), &metadata);
        Some(Record::Session(start))
    } else {
        None
    }
}

fn fresh(records: &[Record], batch: u64) -> Result<Vec<Candidate>> {
    let mut fresh = Vec::new();
    for (part, record) in records.iter().enumerate() {
        if let Some(item) = record.item() {
            let body = item
                .body
                .iter()
                .map(|part| match part {
                    Body::Literal(value) => Ok(value.clone()),
                    _ => anyhow::bail!("unexpected staged body reference"),
                })
                .collect::<Result<Vec<_>>>()?;
            fresh.push(Candidate {
                address: Address(batch, part),
                body,
            });
        }
    }
    Ok(fresh)
}

fn reference_context(records: &mut [Record], fresh: &[Candidate], unbound: &[(String, Candidate)]) {
    for candidate in fresh {
        let Record::Input(item) = &mut records[candidate.address.1] else {
            continue;
        };
        if item.entry.is_none() || item.position.is_some() {
            continue;
        }
        let mut matches = unbound
            .iter()
            .filter(|(_, old)| old.body.starts_with(&candidate.body));
        if let Some((id, old)) = matches.next() {
            // Equal queued prompts can share bytes without claiming an ambiguous identity.
            if matches.next().is_none() {
                item.id = json!(id);
            }
            item.body = encode(&candidate.body, std::slice::from_ref(old));
        }
    }
}

type Retained = (
    Vec<Candidate>,
    Vec<(String, Candidate)>,
    BTreeMap<String, (Request, Option<String>)>,
);

fn retained(
    at: &Location,
    pending: &[(usize, super::super::messages::Entry)],
    has_input: bool,
) -> Result<Retained> {
    let (queued, requests) = {
        let index = at.cached_index();
        (
            if has_input {
                index.unbound.clone()
            } else {
                Default::default()
            },
            pending
                .iter()
                .filter_map(|(_, entry)| match &entry.message.kind {
                    WireKind::Response { request, .. } => index
                        .ids
                        .get(request)
                        .map(|position| index.requests[*position].address),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        )
    };
    let mut originals = original_requests(at, requests)?;
    for (_, entry) in pending {
        if let WireKind::Request(kind) = &entry.message.kind {
            originals.insert(
                entry.message.id.as_str().to_owned(),
                (kind.clone(), entry.native_call.clone()),
            );
        }
    }
    // Only a response to a model input can aggregate older assistant turns.
    // Tool requests and results project onto the fresh checkpoint alone.
    let aggregate = pending.iter().any(|(_, entry)| {
        matches!(
            &entry.message.kind,
            WireKind::Response { request, .. }
                if matches!(originals.get(request.as_str()), Some((Request::ToModel(_), _)))
        )
    });
    let candidates = if aggregate {
        let answers = at.cached_index().answers.clone();
        answers
            .into_iter()
            .map(|address| candidate(at, address))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let queued = queued
        .into_iter()
        .map(|(id, address)| Ok((id.as_str().to_owned(), candidate(at, address)?)))
        .collect::<Result<Vec<_>>>()?;
    Ok((candidates, queued, originals))
}

fn original_requests(
    at: &Location,
    requests: Vec<super::RequestAddress>,
) -> Result<BTreeMap<String, (Request, Option<String>)>> {
    let mut originals = BTreeMap::new();
    // Request metadata is small; read only unresolved requests needed by new responses.
    for address in requests.into_iter().rev() {
        let entry = super::request(at, address)?;
        if let WireKind::Request(kind) = entry.message.kind {
            originals.insert(
                entry.message.id.as_str().to_owned(),
                (kind, entry.native_call),
            );
        }
    }
    Ok(originals)
}

fn candidate(at: &Location, address: Address) -> Result<Candidate> {
    let Value::Array(body) = super::read_body(at, address, 0)? else {
        anyhow::bail!("invalid canonical body");
    };
    Ok(Candidate { address, body })
}

fn attach(
    tx: &mut Transaction,
    position: usize,
    entry: super::super::messages::Entry,
    batch: u64,
    candidates: &[Candidate],
    originals: &BTreeMap<String, (Request, Option<String>)>,
) -> Result<()> {
    let message = &entry.message;
    let wire_body = message
        .body
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<Vec<_>, _>>()?;
    let target = target(tx, &entry, &wire_body, originals);
    if let Some(part) = target {
        let canonical = candidates
            .iter()
            .find(|candidate| candidate.address == Address(batch, part))
            .context("missing staged canonical body")?;
        let item = tx.records[part].item_mut().expect("interaction context");
        bind(item, position, &entry);
        if wire_body != canonical.body {
            item.message_body = Some(encode(&wire_body, candidates));
        }
        return Ok(());
    }
    let owner = entry.native_call.as_ref().and_then(|call| tx.records.iter().position(|record| matches!(record, Record::Assistant(item) if item.entry.is_some() && item.body.iter().any(|body| matches!(body, Body::Literal(value) if value["type"] == "tool_call" && value["value"]["id"] == *call)))));
    let mut item = Item::new(json!(message.id), message.at, Vec::new());
    bind(&mut item, position, &entry);
    if let Some(owner) = owner {
        item.message_body = Some(encode(&wire_body, candidates));
        tx.records[owner]
            .item_mut()
            .expect("tool call owner")
            .calls
            .push(item);
    } else {
        item.body = encode(&wire_body, candidates);
        let record = match &message.kind {
            WireKind::Request(Request::ToModel(_)) => Record::Input(item),
            WireKind::Request(Request::FromModel(_)) => Record::Assistant(item),
            WireKind::Response { .. } => Record::Finished(item),
        };
        tx.records.push(record);
    }
    Ok(())
}

fn target(
    tx: &Transaction,
    entry: &super::super::messages::Entry,
    wire_body: &[Value],
    originals: &BTreeMap<String, (Request, Option<String>)>,
) -> Option<usize> {
    match &entry.message.kind {
        WireKind::Request(Request::ToModel(_)) => tx.records.iter().position(|record| matches!(record, Record::Input(item) if item.position.is_none() && item.id.is_number() && literal_prefix(item, wire_body))),
        WireKind::Response { request, .. } => {
            let original = tx.records.iter().filter_map(Record::item).find(|item| item.id.as_str() == Some(request.as_str()) && item.position.is_some()).map(|item| (item.kind.clone().unwrap_or(Request::ToModel(message::InputKind::User)), item.native_call.clone())).or_else(|| originals.get(request.as_str()).cloned());
            match original {
                Some((Request::ToModel(_), _)) => tx.records.iter().rposition(|record| matches!(record, Record::Assistant(item) if item.entry.is_some() && item.position.is_none())),
                Some((Request::FromModel(_), Some(call))) => tx.records.iter().position(|record| matches!(record, Record::ToolResult(item) if item.position.is_none() && item.body.iter().any(|body| matches!(body, Body::Literal(value) if value["value"]["call"] == call)))),
                _ => None,
            }
        }
        WireKind::Request(Request::FromModel(_)) => None,
    }
}

fn literal_prefix(item: &Item, wire: &[Value]) -> bool {
    item.body.len() <= wire.len()
        && item
            .body
            .iter()
            .zip(wire)
            .all(|(body, wire)| matches!(body, Body::Literal(value) if value == wire))
}

fn bind(item: &mut Item, position: usize, entry: &super::super::messages::Entry) {
    let message = &entry.message;
    item.id = json!(message.id);
    item.position = Some(position);
    item.message_at = (message.at != item.at).then_some(message.at);
    item.by = message.by.clone();
    item.parent = entry.parent.clone();
    item.native_call = entry.native_call.clone();
    match &message.kind {
        WireKind::Request(Request::ToModel(message::InputKind::User)) => {}
        WireKind::Request(kind) => item.kind = Some(kind.clone()),
        WireKind::Response { request, status } => {
            item.request = Some(request.clone());
            item.status = Some(status.into());
        }
    }
}

fn encode(body: &[Value], candidates: &[Candidate]) -> Vec<Body> {
    encode_refs(body, &candidates.iter().collect::<Vec<_>>())
}

fn encode_refs(body: &[Value], candidates: &[&Candidate]) -> Vec<Body> {
    body.iter()
        .map(|block| {
            for candidate in candidates.iter().rev() {
                if let Some(index) = candidate.body.iter().position(|retained| retained == block) {
                    return Body::reference(candidate.address, format!("/{index}"));
                }
            }
            if let Some(text) = encode_text(block, candidates) {
                return text;
            }
            if let Some(value) = block.get("value") {
                for candidate in candidates.iter().rev() {
                    for (index, source) in candidate.body.iter().enumerate() {
                        if let Some(path) = find_value(source, value, format!("/{index}")) {
                            let mut template = block.clone();
                            template["value"] = Value::Null;
                            return Body::Template {
                                template,
                                fields: BTreeMap::from([(
                                    "/value".into(),
                                    Body::reference(candidate.address, path),
                                )]),
                            };
                        }
                    }
                }
            }
            Body::Literal(block.clone())
        })
        .collect()
}

fn encode_text(block: &Value, candidates: &[&Candidate]) -> Option<Body> {
    // Protocol replies can join text from several retained assistant messages.
    // Arbitrary JSON tool data stays literal.
    let paths = text_paths(block);
    let mut template = block.clone();
    let mut fields = BTreeMap::new();
    for path in paths {
        if let Some(text) = block.pointer(&path).and_then(Value::as_str)
            && let Some(encoded) = text_parts(text, candidates)
        {
            *template.pointer_mut(&path).expect("known text path") = Value::Null;
            fields.insert(path, encoded);
        }
    }
    if !fields.is_empty() {
        return Some(Body::Template { template, fields });
    }
    None
}

fn text_paths(block: &Value) -> Vec<String> {
    match block["type"].as_str() {
        Some("text") => vec!["/value".into()],
        Some("reasoning") => block["value"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter(|(_, value)| value["type"] == "text")
            .map(|(index, _)| format!("/value/content/{index}/content/text"))
            .collect(),
        _ => Vec::new(),
    }
}

fn text_parts(text: &str, candidates: &[&Candidate]) -> Option<Body> {
    let mut rest = text;
    let mut parts = Vec::new();
    for candidate in candidates {
        for (index, block) in candidate.body.iter().enumerate() {
            for path in text_paths(block) {
                let Some(prefix) = block
                    .pointer(&path)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                if let Some(tail) = rest.strip_prefix(prefix) {
                    parts.push(Body::reference(
                        candidate.address,
                        format!("/{index}{path}"),
                    ));
                    rest = tail;
                }
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    if !rest.is_empty() {
        parts.push(Body::Literal(json!(rest)));
    }
    Some(Body::Join { join: parts })
}

fn find_value(source: &Value, wanted: &Value, path: String) -> Option<String> {
    if source == wanted {
        return Some(path);
    }
    match source {
        Value::Object(map) => map.iter().find_map(|(key, value)| {
            find_value(
                value,
                wanted,
                format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
            )
        }),
        Value::Array(values) => values
            .iter()
            .enumerate()
            .find_map(|(index, value)| find_value(value, wanted, format!("{path}/{index}"))),
        _ => None,
    }
}

fn metadata(before: &State, tx: &mut Transaction) -> Result<()> {
    let mut fields = metadata_changes(before, tx)?;
    account(before, tx, &mut fields);
    let implied_next = tx
        .records
        .iter()
        .filter_map(Record::item)
        .filter_map(|item| item.entry)
        .map(|id| id.0 + 1)
        .fold(before.next, u64::max);
    if implied_next != tx.state.next {
        fields.insert("next".into(), json!(tx.state.next));
    }
    let interrupted = if tx
        .records
        .iter()
        .any(|record| matches!(record, Record::Rewind { .. }))
    {
        None
    } else {
        before.interrupted.clone()
    };
    if interrupted != tx.state.interrupted {
        fields.insert("interrupted".into(), json!(tx.state.interrupted));
    }
    if !fields.is_empty() {
        tx.records.push(Record::Meta { fields });
    }
    Ok(())
}

fn metadata_changes(before: &State, tx: &Transaction) -> Result<BTreeMap<String, Value>> {
    let mut fields = BTreeMap::new();
    if let (Some(old), Some(new)) = (&before.metadata, &tx.state.metadata) {
        let old_json = serde_json::to_value(old)?;
        let new_json = serde_json::to_value(new)?;
        let has_context = tx.records.iter().any(|record| {
            record.item().is_some_and(|item| item.entry.is_some())
                || matches!(record, Record::Rewind { .. })
        });
        for key in [
            "workspace",
            "model",
            "name",
            "title",
            "created",
            "parent",
            "touched",
            "rounds",
            "prompt",
        ] {
            if has_context && matches!(key, "touched" | "rounds" | "prompt") {
                continue;
            }
            if old_json[key] != new_json[key] {
                fields.insert(key.into(), new_json[key].clone());
            }
        }
    }
    Ok(fields)
}

fn account(before: &State, tx: &mut Transaction, fields: &mut BTreeMap<String, Value>) {
    let (Some(old), Some(new)) = (&before.metadata, &tx.state.metadata) else {
        return;
    };
    if old.spent != new.spent {
        let delta = new.spent.since(&old.spent);
        let mut total = old.spent;
        total.add(&delta.usage, delta.cost);
        if total == new.spent
            && let Some(item) = tx.records.iter_mut().rev().find_map(|record| {
                if matches!(record, Record::Assistant(_)) {
                    record.item_mut()
                } else {
                    None
                }
            })
        {
            item.usage = Some(delta.usage);
            item.cost = Some(delta.cost);
        } else {
            fields.insert("spent".into(), json!(new.spent));
        }
    }
}
