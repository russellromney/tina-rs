//! Native gRPC client surface over [`Http2ClientConnection`].
//!
//! `GrpcClient` is a thin, stateless wrapper above an HTTP/2 client
//! connection isolate — it owns no queue and no runtime of its own. It
//! turns a typed request message into an [`Http2ClientMsg::Submit`] and
//! decodes the connection's [`Http2ClientOutcome`] into a typed gRPC
//! outcome where the gRPC status is first-class: a non-OK status is a
//! normal [`GrpcUnaryOutcome::Status`], never hidden inside a successful
//! HTTP response.
//!
//! This first form covers the **unary** path on cleartext h2c. The
//! request and response are each a single length-prefixed gRPC message
//! buffered under [`GrpcLimits`]. Server-streaming, client-streaming,
//! and bidi need the HTTP/2 client's streaming-body support (a separate
//! slice) and are not implemented here.

use http::{HeaderMap, HeaderValue, Method};
use prost::Message;
use tina::Address;

use crate::grpc::{GrpcError, GrpcLimits, GrpcStatus, GrpcStatusCode, decode_one_grpc_message};
use crate::grpc::{encode_grpc_message, grpc_status_from_header_map};
use crate::http2::{
    Http2ClientMsg, Http2ClientOutcome, Http2ClientReply, Http2ClientRequest, Http2Target,
};

/// A gRPC target: the HTTP/2 transport target plus gRPC-specific
/// defaults (message size caps). Not a string URL bag — the authority,
/// scheme, and TLS truth live in the typed [`Http2Target`].
#[derive(Debug, Clone)]
pub struct GrpcTarget {
    /// HTTP/2 connection target (h2c or h2/TLS).
    pub http2: Http2Target,
    /// gRPC message-size limits applied on encode and decode.
    pub limits: GrpcLimits,
}

impl GrpcTarget {
    /// h2c gRPC target for `authority` at `addr`.
    pub fn h2c(authority: impl Into<String>, addr: std::net::SocketAddr) -> Self {
        Self {
            http2: Http2Target::H2c {
                authority: authority.into(),
                addr,
            },
            limits: GrpcLimits::default(),
        }
    }

    /// Override the gRPC message-size limits.
    pub fn with_limits(mut self, limits: GrpcLimits) -> Self {
        self.limits = limits;
        self
    }
}

/// Typed outcome of a unary gRPC call.
///
/// The gRPC status is first-class: a non-OK status is
/// [`GrpcUnaryOutcome::Status`], distinct from an HTTP/2
/// [`GrpcUnaryOutcome::Transport`] failure and from a
/// [`GrpcUnaryOutcome::Malformed`] (non-gRPC / undecodable) response.
/// `Ok(Resp)` is produced only when the status is `OK` *and* the
/// response message decodes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrpcUnaryOutcome<Resp> {
    /// gRPC status `OK` with a decoded response message.
    Ok(Resp),
    /// gRPC returned a non-OK status. A normal gRPC outcome, not a
    /// transport failure.
    Status(GrpcStatus),
    /// The HTTP/2 transport failed before a gRPC status was received
    /// (connection closed, stream reset, protocol error, ALPN mismatch).
    Transport(Http2ClientOutcome),
    /// The response reached the gRPC layer but was not well-formed gRPC
    /// (missing `grpc-status`, non-200 HTTP status, bad content-type,
    /// undecodable or oversized message frame).
    Malformed(GrpcError),
}

/// Native gRPC client over one HTTP/2 client connection.
///
/// Holds the connection isolate address plus the gRPC limits. Stateless:
/// build a `Submit` with [`GrpcClient::unary_request`], issue it as a
/// Tina call against [`GrpcClient::connection`], then decode the reply
/// with [`GrpcClient::decode_unary`]. For host tests,
/// [`GrpcClient::unary_outcome_from_reply`] folds a whole
/// [`Http2ClientReply`] into the typed outcome in one step.
///
/// The unary helper takes exactly one `prost` message — a stream of
/// messages (an iterator) is not a `Message` and does not compile:
///
/// ```compile_fail
/// let client: tina_http::GrpcClient = unimplemented!();
/// let request_stream = std::iter::repeat(0u8); // a stream, not a Message
/// let _ = client.unary_request("/svc/Method", &request_stream);
/// ```
///
/// A unary outcome keeps the gRPC status first-class: you cannot treat
/// the outcome as the response message and silently drop a non-OK
/// status. Extracting the message requires matching `Ok(..)`:
///
/// ```compile_fail
/// let outcome: tina_http::GrpcUnaryOutcome<u64> = unimplemented!();
/// let _message: u64 = outcome; // does not compile: status is a separate arm
/// ```
#[derive(Debug, Clone)]
pub struct GrpcClient {
    connection: Address<Http2ClientMsg, Http2ClientReply>,
    limits: GrpcLimits,
}

impl GrpcClient {
    pub fn new(connection: Address<Http2ClientMsg, Http2ClientReply>, limits: GrpcLimits) -> Self {
        Self { connection, limits }
    }

    /// The underlying HTTP/2 client connection address. Callers issue the
    /// `Submit` produced by [`unary_request`](Self::unary_request) as a
    /// Tina call against this address.
    pub fn connection(&self) -> Address<Http2ClientMsg, Http2ClientReply> {
        self.connection
    }

    /// Build the [`Http2ClientMsg::Submit`] for a unary call to
    /// `full_method_path` (e.g. `"/pkg.Service/Method"`). The request
    /// message is encoded as one length-prefixed gRPC frame; the gRPC
    /// content-type and `te: trailers` headers are set.
    ///
    /// Returns [`GrpcError::EncodeTooLarge`] if the request exceeds the
    /// configured message cap, before anything reaches the wire.
    pub fn unary_request<Req: Message>(
        &self,
        full_method_path: &str,
        message: &Req,
    ) -> Result<Http2ClientMsg, GrpcError> {
        let body = encode_grpc_message(message, self.limits)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/grpc+proto"),
        );
        // gRPC requires the client to advertise trailer support.
        headers.insert(
            http::HeaderName::from_static("te"),
            HeaderValue::from_static("trailers"),
        );
        Ok(Http2ClientMsg::Submit(Http2ClientRequest {
            method: Method::POST,
            path: full_method_path.to_owned(),
            headers,
            body,
        }))
    }

    /// Decode a connection [`Http2ClientOutcome`] into a typed unary gRPC
    /// outcome. The gRPC status is read from the response trailers (or the
    /// headers, for a trailers-only response); a non-OK status is returned
    /// as [`GrpcUnaryOutcome::Status`], never collapsed into a success.
    pub fn decode_unary<Resp: Message + Default>(
        &self,
        outcome: Http2ClientOutcome,
    ) -> GrpcUnaryOutcome<Resp> {
        let response = match outcome {
            Http2ClientOutcome::Replied(response) => response,
            // Anything that is not a completed HTTP response is a
            // transport-level failure, not a gRPC status.
            transport => return GrpcUnaryOutcome::Transport(transport),
        };
        // gRPC-over-HTTP/2 requires HTTP status 200; anything else is a
        // transport-level protocol failure (RFC: "HTTP status").
        if response.status.as_u16() != 200 {
            return GrpcUnaryOutcome::Malformed(GrpcError::BadFrame);
        }
        // The status lives in trailers normally, or in headers for a
        // trailers-only response.
        let status = match grpc_status_from_header_map(&response.trailers)
            .or_else(|| grpc_status_from_header_map(&response.headers))
        {
            Some(status) => status,
            None => return GrpcUnaryOutcome::Malformed(GrpcError::MissingTrailers),
        };
        if status.code != GrpcStatusCode::Ok {
            return GrpcUnaryOutcome::Status(status);
        }
        // OK status: decode the single response message from the body.
        let mut cursor = 0;
        match decode_one_grpc_message::<Resp>(&response.body, &mut cursor, self.limits) {
            Ok(message) if cursor == response.body.len() => GrpcUnaryOutcome::Ok(message),
            Ok(_) => GrpcUnaryOutcome::Malformed(GrpcError::BadFrame),
            Err(error) => GrpcUnaryOutcome::Malformed(error),
        }
    }

    /// Fold a whole [`Http2ClientReply`] into a typed unary outcome.
    /// Convenience for host code that called the connection and got a
    /// `Reply` back. A non-`Outcome` reply (e.g. a `Report`) is
    /// `Malformed`.
    pub fn unary_outcome_from_reply<Resp: Message + Default>(
        &self,
        reply: Http2ClientReply,
    ) -> GrpcUnaryOutcome<Resp> {
        match reply {
            Http2ClientReply::Outcome { outcome, .. } => self.decode_unary::<Resp>(outcome),
            Http2ClientReply::Report(_) => GrpcUnaryOutcome::Malformed(GrpcError::BadFrame),
        }
    }
}
