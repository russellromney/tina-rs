#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

//! Narrow Tokio/Tower bridge for Tina services.
//!
//! The bridge does not make Tina handlers async and does not add a hidden
//! executor queue. Tokio code sends one bounded request into a
//! [`BetelgeuseBackedRuntime`], then
//! waits for a oneshot response with an explicit timeout.

use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tina::{Address, Shard};
use tina_runtime::{
    BetelgeuseBackedRuntime, BetelgeuseBackedSendObservedError, BetelgeuseBackedTrySendError,
    CallError, MailboxFactory, SendRejectedReason,
};
use tokio::sync::oneshot;
use tower_service::Service;

/// Error returned by a Tokio-to-Tina bridge call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeError {
    /// The bounded Tina worker ingress queue is full.
    Full,
    /// Tina worker or responder was closed before a response arrived.
    Closed,
    /// The caller's explicit bridge timeout elapsed.
    Timeout,
}

impl From<BetelgeuseBackedTrySendError> for BridgeError {
    fn from(error: BetelgeuseBackedTrySendError) -> Self {
        match error {
            BetelgeuseBackedTrySendError::IngressFull => Self::Full,
            BetelgeuseBackedTrySendError::WorkerStopped => Self::Closed,
        }
    }
}

impl From<BetelgeuseBackedSendObservedError> for BridgeError {
    fn from(error: BetelgeuseBackedSendObservedError) -> Self {
        match error {
            BetelgeuseBackedSendObservedError::IngressFull
            | BetelgeuseBackedSendObservedError::MailboxFull => Self::Full,
            BetelgeuseBackedSendObservedError::MailboxClosed
            | BetelgeuseBackedSendObservedError::WorkerStopped => Self::Closed,
        }
    }
}

impl From<BridgeError> for CallError {
    fn from(error: BridgeError) -> Self {
        match error {
            BridgeError::Full => Self::TargetFull,
            BridgeError::Closed => Self::TargetClosed,
            BridgeError::Timeout => Self::Timeout,
        }
    }
}

impl From<BridgeError> for SendRejectedReason {
    fn from(error: BridgeError) -> Self {
        match error {
            BridgeError::Full => Self::Full,
            BridgeError::Closed | BridgeError::Timeout => Self::Closed,
        }
    }
}

/// Message payload sent from Tokio code into a Tina isolate.
///
/// A Tina handler receives this as its normal message, splits it into the user
/// request plus responder, and replies synchronously from the handler turn.
#[derive(Debug)]
pub struct BridgeRequest<M, R> {
    message: M,
    responder: BridgeResponder<R>,
}

impl<M, R> BridgeRequest<M, R> {
    /// Creates one bridge request around a user message and oneshot responder.
    pub fn new(message: M, sender: oneshot::Sender<R>) -> Self {
        Self {
            message,
            responder: BridgeResponder {
                sender: Some(sender),
            },
        }
    }

    /// Splits the bridge request into the user message and responder.
    pub fn into_parts(self) -> (M, BridgeResponder<R>) {
        (self.message, self.responder)
    }
}

/// One response handle owned by the Tina handler turn.
#[derive(Debug)]
pub struct BridgeResponder<R> {
    sender: Option<oneshot::Sender<R>>,
}

impl<R> BridgeResponder<R> {
    /// Sends the response back to the Tokio caller.
    ///
    /// Returns the response when the Tokio caller has already timed out or
    /// dropped the request.
    pub fn respond(mut self, response: R) -> Result<(), R> {
        self.sender
            .take()
            .expect("bridge responder is consumed once")
            .send(response)
    }
}

/// Cloneable bounded bridge handle for Tokio/Tower callers.
pub struct BridgeHandle<M, R, S, F, AR = ()>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    runtime: Arc<BetelgeuseBackedRuntime<S, F>>,
    address: Address<BridgeRequest<M, R>, AR>,
    timeout: Duration,
    marker: PhantomData<fn(M, R, AR)>,
}

impl<M, R, S, F, AR> Clone for BridgeHandle<M, R, S, F, AR>
where
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
{
    fn clone(&self) -> Self {
        Self {
            runtime: Arc::clone(&self.runtime),
            address: self.address,
            timeout: self.timeout,
            marker: PhantomData,
        }
    }
}

impl<M, R, S, F, AR> BridgeHandle<M, R, S, F, AR>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
{
    /// Builds a bridge handle to an already-registered Tina isolate.
    pub fn new(
        runtime: Arc<BetelgeuseBackedRuntime<S, F>>,
        address: Address<BridgeRequest<M, R>, AR>,
        timeout: Duration,
    ) -> Self {
        Self {
            runtime,
            address,
            timeout,
            marker: PhantomData,
        }
    }

    /// Sends one bounded request into Tina and waits for the handler response.
    pub async fn call(&self, message: M) -> Result<R, BridgeError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let (observed_tx, observed_rx) = oneshot::channel();
        self.runtime
            .try_send_and_observe_with(
                self.address,
                BridgeRequest::new(message, reply_tx),
                move |result| {
                    let _ = observed_tx.send(result);
                },
            )
            .map_err(BridgeError::from)?;

        tokio::time::timeout(self.timeout, async {
            observed_rx
                .await
                .map_err(|_| BridgeError::Closed)?
                .map_err(BridgeError::from)?;
            reply_rx.await.map_err(|_| BridgeError::Closed)
        })
        .await
        .map_err(|_| BridgeError::Timeout)?
    }
}

impl<M, R, S, F, AR> Service<M> for BridgeHandle<M, R, S, F, AR>
where
    M: Send + 'static,
    R: Send + 'static,
    S: Shard + Send + 'static,
    F: MailboxFactory + Send + 'static,
    AR: Send + 'static,
{
    type Response = R;
    type Error = BridgeError;
    type Future = Pin<Box<dyn Future<Output = Result<R, BridgeError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: M) -> Self::Future {
        let handle = self.clone();
        Box::pin(async move { BridgeHandle::call(&handle, request).await })
    }
}
