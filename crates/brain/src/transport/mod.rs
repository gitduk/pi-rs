use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::catalog::ModelSpec;
use crate::error::Result;
use crate::request::Request;
use crate::stream::StreamEvent;

pub mod anthropic;
pub mod openai;

/// One exchange, from the request going out to the response coming back.
///
/// Both wires go through this rather than calling a set of logging helpers at
/// the right moments themselves. Sharing the words but not the control flow is
/// how the two came to share a hole: `send().await?` returns on a refused
/// connection, a DNS failure or a TLS failure without either wire noticing, so
/// the journal showed a request and then nothing — the one class of failure a
/// journal is most needed for. Owning the flow makes that unrecordable.
pub(crate) async fn exchange(
    wire: &'static str,
    url: String,
    spec: &ModelSpec,
    req: &Request,
    body: &serde_json::Value,
    call: reqwest::RequestBuilder,
) -> crate::Result<reqwest::Response> {
    tracing::debug!(
        target: "pi::wire",
        wire,
        // Some hosts take their credential as a query parameter. The path is
        // what identifies the endpoint; the rest is not worth the risk.
        url = url.split('?').next().unwrap_or(&url),
        model = %spec.wire_id,
        messages = req.messages.len(),
        tools = req.tools.len(),
        effort = ?req.effort,
        "request"
    );
    // The body runs to hundreds of kilobytes and is the one thing a 400 is
    // actually about, so it rides one level below everything else.
    tracing::trace!(target: "pi::wire", wire, body = %body, "request body");

    let began = std::time::Instant::now();
    let resp = match call.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::error!(
                target: "pi::wire",
                wire,
                took_ms = began.elapsed().as_millis() as u64,
                // As the trait object, so the journal unwinds the source chain.
                // A reqwest error's own line says only "error sending request";
                // `Connection refused` is two links down, and it is the answer.
                error = &e as &dyn std::error::Error,
                "unreachable"
            );
            return Err(e.into());
        }
    };

    let status = resp.status().as_u16();
    let took_ms = began.elapsed().as_millis() as u64;
    if resp.status().is_success() {
        tracing::info!(target: "pi::wire", wire, status, took_ms, "response");
        return Ok(resp);
    }

    // The refusal text in full where the level allows: providers put the real
    // reason in it, and a status alone never says which of a dozen things
    // went wrong.
    let body = resp.text().await.unwrap_or_default();
    tracing::warn!(target: "pi::wire", wire, status, took_ms, detail = %body, "refused");
    Err(crate::BrainError::Api {
        transport: wire,
        status,
        body,
    })
}

/// One wire protocol. Implementations branch on `spec.wire`'s compat record and
/// never on the model id: identity is resolved once, into the catalog.
#[async_trait]
pub trait Transport: Send + Sync {
    fn name(&self) -> &'static str;

    async fn stream(
        &self,
        spec: &ModelSpec,
        req: &Request,
    ) -> Result<BoxStream<'static, Result<StreamEvent>>>;
}
