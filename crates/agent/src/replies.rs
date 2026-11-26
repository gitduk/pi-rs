//! Project accepted user responses into native history, including cancellation.

use crate::AgentError;
use crate::event::Reporter;
use crate::seams::Input;
use crate::session::{Drawn, Session};
use llm::message::{ToolCall, ToolResult};
use std::collections::HashMap;
use tool::{Ctx, ToolOutput};

fn bound(mut result: ToolResult, ctx: &Ctx) -> ToolResult {
    let output = ToolOutput {
        content: std::mem::take(&mut result.content),
        screen: None,
        spent: Default::default(),
        warned: false,
    };
    result.content = tool::output::bound(output, ctx).content;
    result
}

pub(crate) fn reconcile(
    calls: &[ToolCall],
    results: Vec<(ToolResult, Drawn)>,
    resolved: Vec<ToolResult>,
    ctx: &Ctx,
) -> Result<Vec<(ToolResult, Drawn)>, AgentError> {
    if resolved.is_empty() {
        return Ok(results);
    }
    let mut ready: HashMap<_, _> = results
        .into_iter()
        .map(|pair| (pair.0.call.clone(), pair))
        .collect();
    for mut result in resolved {
        let call = calls
            .iter()
            .find(|call| call.id == result.call && call.name == result.name)
            .ok_or_else(|| {
                AgentError::Interaction("resolved user result has no matching native call".into())
            })?;
        let previous = ready.remove(&call.id);
        let (mut drawn, suffix) = match previous {
            Some((native, drawn)) if !native.is_error && !result.is_error => {
                let count = result.content.len();
                if native.content.get(..count) == Some(&result.content[..]) {
                    ready.insert(call.id.clone(), (native, drawn));
                    continue;
                }
                // Capability hooks append context after the answer. Keep those
                // blocks when its answer prefix needs a fresh projection.
                (drawn, native.content.into_iter().skip(count).collect())
            }
            Some((_, drawn)) => (drawn, Vec::new()),
            None => (Drawn::default(), Vec::new()),
        };
        drawn.screen = None;
        drawn.warned = false;
        if result.is_error {
            drawn.file = None;
        }
        result = bound(result, ctx);
        result.content.extend(suffix);
        ready.insert(call.id.clone(), (result, drawn));
    }
    Ok(calls
        .iter()
        .filter_map(|call| ready.remove(&call.id))
        .collect())
}

pub(crate) async fn settle(
    session: &mut Session,
    ctx: &Ctx,
    reporter: &Reporter,
    input: &dyn Input,
    status: message::Status,
) -> Result<(), AgentError> {
    let mut resolved: HashMap<_, _> = input
        .settle_calls(status)
        .map_err(AgentError::Interaction)?
        .into_iter()
        .map(|result| (result.call.clone(), result))
        .collect();
    if resolved.is_empty() {
        return Ok(());
    }
    let mut results = Vec::new();
    for (id, name) in session.open_calls() {
        if let Some(result) = resolved.remove(&id) {
            if result.name != name {
                return Err(AgentError::Interaction(
                    "resolved user result tool name does not match".into(),
                ));
            }
            results.push((bound(result, ctx), Drawn::default()));
        }
    }
    commit(session, reporter, input, results).await
}

/// Persist history and responses together before reporting completed results.
pub(crate) async fn commit(
    session: &mut Session,
    reporter: &Reporter,
    input: &dyn Input,
    results: Vec<(ToolResult, Drawn)>,
) -> Result<(), AgentError> {
    for event in retain(session, input, results).await? {
        reporter.report(event).await;
    }
    Ok(())
}

pub(crate) async fn retain(
    session: &mut Session,
    input: &dyn Input,
    results: Vec<(ToolResult, Drawn)>,
) -> Result<Vec<crate::Event>, AgentError> {
    if results.is_empty() {
        return Ok(Vec::new());
    }
    let ids = session.push_drawn(results);
    let entries = session.entries_for(&ids);
    for &entry in &entries {
        if let crate::session::Entry::Tool { result, .. } = entry {
            input
                .tool_response(result)
                .map_err(AgentError::Interaction)?;
        }
    }
    input
        .checkpoint(session)
        .await
        .map_err(AgentError::Interaction)?;
    let mut events = Vec::new();
    for &entry in &entries {
        if let crate::session::Entry::Tool {
            result,
            screen,
            warned,
            took: Some(took),
            ..
        } = entry
        {
            events.push(crate::Event::ToolEnd {
                id: result.call.clone(),
                name: result.name.clone(),
                outcome: crate::Outcome::of(result.is_error, *warned),
                screen: tool::screen_of(screen.as_deref(), &result.content),
                took: *took,
            });
        }
    }
    events.push(crate::Event::Committed {
        entries: entries.into_iter().cloned().collect(),
    });
    Ok(events)
}
