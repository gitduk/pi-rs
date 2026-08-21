use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::catalog::ModelSpec;
use crate::error::Result;
use crate::request::Request;
use crate::stream::StreamEvent;

pub mod anthropic;
pub mod openai;

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
