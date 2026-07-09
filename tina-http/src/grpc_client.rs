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
//! Beyond **unary** (one buffered request and response message), this
//! covers the streaming call shapes on top of the HTTP/2 client's
//! streaming bodies:
//!
//! - **server-streaming** ([`server_streaming_request`]): one buffered
//!   request, a pulled response. Feed each response chunk to a
//!   [`GrpcStreamDecoder`] and fold it with
//!   [`decode_stream_chunk`] into [`GrpcStreamItem`]s ending in one
//!   `Status`.
//! - **client-streaming** ([`client_streaming_request`]): a streamed
//!   request body (a `source` of gRPC-framed messages, built with
//!   [`frame`]) and a single buffered response — decode with
//!   [`decode_unary`] like a unary call.
//! - **bidi** ([`bidi_request`]): a streamed request body and a pulled
//!   response, so the two directions progress independently.
//!
//! The gRPC status stays first-class on every shape: a non-OK status is
//! a `Status` item / outcome, never hidden in a successful HTTP response.
//!
//! [`server_streaming_request`]: GrpcClient::server_streaming_request
//! [`client_streaming_request`]: GrpcClient::client_streaming_request
//! [`bidi_request`]: GrpcClient::bidi_request
//! [`decode_stream_chunk`]: GrpcClient::decode_stream_chunk
//! [`decode_unary`]: GrpcClient::decode_unary
//! [`frame`]: GrpcClient::frame

use std::sync::Arc;

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use prost::Message;
use tina::{Address, Shard};

use crate::grpc::{
    GRPC_FRAME_HEADER_LEN, GrpcError, GrpcLimits, GrpcStatus, GrpcStatusCode,
    decode_one_grpc_message,
};
use crate::grpc::{
    encode_grpc_message, encode_grpc_message_into, grpc_status_from_compact,
    grpc_status_from_header_map,
};
use crate::http2::{
    Http2ClientConnection, Http2ClientGrpcUnaryRequest, Http2ClientLimits, Http2ClientMsg,
    Http2ClientOutcome, Http2ClientReply, Http2ClientRequestBody, Http2ClientStreamCall,
    Http2ResponseChunk, Http2Target,
};
use crate::streaming::{ResponseChunkMsg, ResponseChunkReply};

/// The gRPC request/response content-type and the mandatory `te:
/// trailers` advertisement, shared by every gRPC call shape.
fn grpc_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc+proto"),
    );
    headers.insert(
        http::HeaderName::from_static("te"),
        HeaderValue::from_static("trailers"),
    );
    headers
}

/// Maps a non-200 HTTP status with no `grpc-status` into a synthesized
/// gRPC status, per `grpc/doc/http-grpc-status-mapping.md`. Used only
/// when an HTTP/2 intermediary fails the request without speaking gRPC.
fn http_status_to_grpc_status(status: StatusCode) -> GrpcStatus {
    let code = match status.as_u16() {
        400 => GrpcStatusCode::Internal,
        401 => GrpcStatusCode::Unauthenticated,
        403 => GrpcStatusCode::PermissionDenied,
        404 => GrpcStatusCode::Unimplemented,
        429 | 502 | 503 | 504 => GrpcStatusCode::Unavailable,
        _ => GrpcStatusCode::Unknown,
    };
    GrpcStatus::with_message(code, format!("HTTP status {}", status.as_u16()))
}

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

    /// Build the HTTP/2 client connection isolate for this target. Register
    /// it with the runtime, send it `Http2ClientMsg::Begin`, then wrap the
    /// returned address with [`GrpcClient::new`] using
    /// [`GrpcTarget::limits`]:
    ///
    /// ```ignore
    /// let conn = runtime.register_with_capacity(target.http2_connection::<S>(), 32)?;
    /// runtime.try_send(conn, Http2ClientMsg::Begin)?;
    /// let client = GrpcClient::new(conn, target.limits());
    /// ```
    pub fn http2_connection<S: Shard + 'static>(&self) -> Http2ClientConnection<S> {
        Http2ClientConnection::new(self.http2.clone(), Http2ClientLimits::default())
    }

    /// The gRPC message-size limits for this target.
    pub fn limits(&self) -> GrpcLimits {
        self.limits
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

/// Reusable builder for one gRPC unary method path.
///
/// This validates and stores the method path once. Each request still
/// encodes the protobuf message into a fresh gRPC frame, but it skips the
/// per-call path allocation and public HTTP header map construction.
#[derive(Debug, Clone)]
pub struct GrpcUnaryTemplate {
    path: Arc<str>,
    limits: GrpcLimits,
}

impl GrpcUnaryTemplate {
    pub fn request<Req: Message>(&self, message: &Req) -> Result<Http2ClientMsg, GrpcError> {
        let body = encode_grpc_message(message, self.limits)?;
        Ok(Http2ClientMsg::SubmitGrpcUnary(
            Http2ClientGrpcUnaryRequest::owned(Arc::clone(&self.path), body),
        ))
    }

    /// Pre-encode one request message for hot repeated unary calls with an
    /// identical payload. This is useful for health checks, perf probes, and
    /// fixed command messages; dynamic requests should use [`request`](Self::request).
    pub fn preframed<Req: Message>(&self, message: &Req) -> Result<GrpcPreframedUnary, GrpcError> {
        let body = encode_grpc_message(message, self.limits)?;
        Ok(GrpcPreframedUnary {
            path: Arc::clone(&self.path),
            body: Arc::from(body.into_boxed_slice()),
        })
    }
}

/// A reusable already-framed gRPC unary request body.
#[derive(Debug, Clone)]
pub struct GrpcPreframedUnary {
    path: Arc<str>,
    body: Arc<[u8]>,
}

impl GrpcPreframedUnary {
    pub fn request(&self) -> Http2ClientMsg {
        Http2ClientMsg::SubmitGrpcUnary(Http2ClientGrpcUnaryRequest::shared(
            Arc::clone(&self.path),
            Arc::clone(&self.body),
        ))
    }
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

    /// Build the [`Http2ClientMsg::SubmitGrpcUnary`] for a unary call to
    /// `full_method_path` (e.g. `"/pkg.Service/Method"`). The request
    /// message is encoded as one length-prefixed gRPC frame; the HTTP/2
    /// connection emits the fixed gRPC headers directly.
    ///
    /// Returns [`GrpcError::EncodeTooLarge`] if the request exceeds the
    /// configured message cap, before anything reaches the wire.
    pub fn unary_request<Req: Message>(
        &self,
        full_method_path: &str,
        message: &Req,
    ) -> Result<Http2ClientMsg, GrpcError> {
        self.unary_template(full_method_path)?.request(message)
    }

    /// Reuse the validated method path for repeated unary calls.
    pub fn unary_template(&self, full_method_path: &str) -> Result<GrpcUnaryTemplate, GrpcError> {
        validate_grpc_path(full_method_path)?;
        Ok(GrpcUnaryTemplate {
            path: Arc::from(full_method_path),
            limits: self.limits,
        })
    }

    /// Decode a connection [`Http2ClientOutcome`] into a typed unary gRPC
    /// outcome. An explicit `grpc-status` (from trailers, or headers for a
    /// trailers-only response) is authoritative and a non-OK value is
    /// returned as [`GrpcUnaryOutcome::Status`], never collapsed into a
    /// success. When no `grpc-status` is present, a non-200 HTTP status is
    /// synthesized into a gRPC status per the gRPC HTTP-status mapping
    /// (e.g. 404 → `Unimplemented`), so an infra/proxy failure still
    /// surfaces as a typed status rather than a generic "malformed".
    pub fn decode_unary<Resp: Message + Default>(
        &self,
        outcome: Http2ClientOutcome,
    ) -> GrpcUnaryOutcome<Resp> {
        match outcome {
            Http2ClientOutcome::Replied(response) => {
                // An explicit grpc-status wins regardless of HTTP status (gRPC
                // servers always send HTTP 200 and put status in trailers, or in
                // headers for a trailers-only response).
                let status = grpc_status_from_header_map(&response.trailers)
                    .or_else(|| grpc_status_from_header_map(&response.headers));
                self.finish_unary(response.status, status, &response.body)
            }
            // The compact gRPC-unary path: the HTTP/2 client already parsed the
            // status facts, so there is no `HeaderMap` to scan.
            Http2ClientOutcome::GrpcUnaryReplied {
                status,
                grpc_status,
                grpc_message,
                body,
            } => {
                let parsed =
                    grpc_status.map(|code| grpc_status_from_compact(code, grpc_message.as_deref()));
                self.finish_unary(status, parsed, &body)
            }
            // Anything that is not a completed HTTP response is a
            // transport-level failure, not a gRPC status.
            transport => GrpcUnaryOutcome::Transport(transport),
        }
    }

    /// Fold an HTTP status, an optional explicit gRPC status, and the response
    /// body into a typed unary outcome. Shared by the public-header and compact
    /// receive paths so both report identical status truth.
    fn finish_unary<Resp: Message + Default>(
        &self,
        http_status: StatusCode,
        grpc_status: Option<GrpcStatus>,
        body: &[u8],
    ) -> GrpcUnaryOutcome<Resp> {
        let status = match grpc_status {
            Some(status) => status,
            None => {
                return if http_status.as_u16() == 200 {
                    // A 200 gRPC response MUST carry a grpc-status.
                    GrpcUnaryOutcome::Malformed(GrpcError::MissingTrailers)
                } else {
                    // No grpc-status + non-200: a proxy/infra failure.
                    // Synthesize the status the gRPC spec prescribes.
                    GrpcUnaryOutcome::Status(http_status_to_grpc_status(http_status))
                };
            }
        };
        if status.code != GrpcStatusCode::Ok {
            return GrpcUnaryOutcome::Status(status);
        }
        // OK status: decode exactly one response message from the body.
        let mut cursor = 0;
        match decode_one_grpc_message::<Resp>(body, &mut cursor, self.limits) {
            Ok(message) if cursor == body.len() => GrpcUnaryOutcome::Ok(message),
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
            Http2ClientReply::Report(_) | Http2ClientReply::ResponseChunk { .. } => {
                GrpcUnaryOutcome::Malformed(GrpcError::BadFrame)
            }
        }
    }

    /// Build the [`Http2ClientMsg::OpenStream`] for a **server-streaming**
    /// call: one buffered request message, a pulled response. Issue it as
    /// a Tina call; the reply is an
    /// [`Http2ClientOutcome::ResponseStreaming`] head (check
    /// [`stream_head_status`](Self::stream_head_status) on its headers for
    /// a trailers-only error), then pull the body with
    /// [`Http2ClientMsg::ResponseNext`] and feed each chunk to
    /// [`decode_stream_chunk`](Self::decode_stream_chunk).
    pub fn server_streaming_request<Req: Message>(
        &self,
        full_method_path: &str,
        message: &Req,
    ) -> Result<Http2ClientMsg, GrpcError> {
        validate_grpc_path(full_method_path)?;
        let body = encode_grpc_message(message, self.limits)?;
        Ok(Http2ClientMsg::OpenStream(Http2ClientStreamCall {
            method: Method::POST,
            path: full_method_path.to_owned(),
            headers: grpc_headers(),
            body: Http2ClientRequestBody::Buffered(body),
        }))
    }

    /// Build the [`Http2ClientMsg::SubmitStreaming`] for a
    /// **client-streaming** call: a streamed request body (the `source`
    /// yields gRPC-framed messages — see [`frame`](Self::frame)) and a
    /// single buffered response message + status. Decode the reply with
    /// [`decode_unary`](Self::decode_unary), exactly like a unary call.
    pub fn client_streaming_request(
        &self,
        full_method_path: &str,
        source: Address<ResponseChunkMsg, ResponseChunkReply>,
    ) -> Result<Http2ClientMsg, GrpcError> {
        validate_grpc_path(full_method_path)?;
        Ok(Http2ClientMsg::SubmitStreaming(
            crate::http2::Http2ClientStreamingRequest {
                method: Method::POST,
                path: full_method_path.to_owned(),
                headers: grpc_headers(),
                source,
            },
        ))
    }

    /// Build the [`Http2ClientMsg::OpenStream`] for a **bidi** call: a
    /// streamed request body and a pulled response. The request `source`
    /// yields gRPC-framed messages; the response is pulled and decoded
    /// just like server-streaming, so the two directions progress
    /// independently.
    pub fn bidi_request(
        &self,
        full_method_path: &str,
        source: Address<ResponseChunkMsg, ResponseChunkReply>,
    ) -> Result<Http2ClientMsg, GrpcError> {
        validate_grpc_path(full_method_path)?;
        Ok(Http2ClientMsg::OpenStream(Http2ClientStreamCall {
            method: Method::POST,
            path: full_method_path.to_owned(),
            headers: grpc_headers(),
            body: Http2ClientRequestBody::Stream(source),
        }))
    }

    /// Encode one message as a length-prefixed gRPC frame, for building a
    /// streaming request `source` (e.g. an `IterBodySource` over pre-framed
    /// messages). Honours the configured message cap.
    pub fn frame<Req: Message>(&self, message: &Req) -> Result<Vec<u8>, GrpcError> {
        encode_grpc_message(message, self.limits)
    }

    /// Append one length-prefixed gRPC frame onto a caller-owned buffer — the
    /// reusable form of [`frame`](Self::frame). A caller building a
    /// client-streaming body from several messages can pack them into one
    /// buffer (a valid concatenated gRPC body) instead of allocating a fresh
    /// `Vec` per message. The configured message cap is enforced before any
    /// bytes are written, so an over-cap message fails with
    /// [`GrpcError::EncodeTooLarge`] and leaves `out` untouched.
    pub fn frame_into<Req: Message>(
        &self,
        out: &mut Vec<u8>,
        message: &Req,
    ) -> Result<(), GrpcError> {
        encode_grpc_message_into(out, message, self.limits)
    }

    /// Read a final gRPC status from a streamed response **head's** header
    /// block. A trailers-only error response (END_STREAM on the HEADERS
    /// frame) carries `grpc-status` here, not in the `End` trailers.
    pub fn stream_head_status(&self, headers: &HeaderMap) -> Option<GrpcStatus> {
        grpc_status_from_header_map(headers)
    }

    /// Fold one streamed-response [`Http2ResponseChunk`] into typed gRPC
    /// stream items, draining any newly complete messages from `decoder`.
    /// `Data` yields zero or more [`GrpcStreamItem::Message`]s; `End`
    /// yields a [`GrpcStreamItem::Status`] from `grpc-status` trailers,
    /// or [`GrpcStreamItem::Malformed`] when the trailers omit it; a
    /// transport teardown yields [`GrpcStreamItem::Transport`].
    pub fn decode_stream_chunk<Resp: Message + Default>(
        &self,
        decoder: &mut GrpcStreamDecoder,
        chunk: Http2ResponseChunk,
    ) -> Vec<GrpcStreamItem<Resp>> {
        match chunk {
            Http2ResponseChunk::Data(bytes) => match decoder.push::<Resp>(&bytes) {
                Ok(messages) => messages.into_iter().map(GrpcStreamItem::Message).collect(),
                Err(error) => vec![GrpcStreamItem::Malformed(error)],
            },
            Http2ResponseChunk::End { trailers } => {
                // `End` carries no body bytes: the HTTP/2 client delivers a
                // final DATA frame's payload as a separate `Data` chunk
                // (already fed to the decoder above) *before* surfacing
                // `End`. So there is nothing to `push` here — only the
                // final status. A partial frame still buffered at END_STREAM
                // is therefore a truncated message, not a clean end.
                if let Err(error) = decoder.finish() {
                    return vec![GrpcStreamItem::Malformed(error)];
                }
                match grpc_status_from_header_map(&trailers) {
                    Some(status) => vec![GrpcStreamItem::Status(status)],
                    None => vec![GrpcStreamItem::Malformed(GrpcError::MissingTrailers)],
                }
            }
            // Reset / Closed / ProtocolError — the stream died before a
            // gRPC status. Surface it as a transport item, not a status.
            other => vec![GrpcStreamItem::Transport(other)],
        }
    }
}

/// gRPC method paths are absolute (`/package.Service/Method`); a relative
/// path produces an invalid `:path` pseudo-header on the wire.
fn validate_grpc_path(full_method_path: &str) -> Result<(), GrpcError> {
    if full_method_path.starts_with('/') {
        Ok(())
    } else {
        Err(GrpcError::InvalidPath(full_method_path.to_owned()))
    }
}

/// Reassembles length-prefixed gRPC messages from a streamed HTTP/2
/// response body. A single response DATA chunk may carry several
/// messages, one message, or a fragment that spans chunks — this decoder
/// buffers across [`push`](Self::push) calls and yields only the messages
/// that are now complete.
#[derive(Debug, Clone)]
pub struct GrpcStreamDecoder {
    buf: Vec<u8>,
    limits: GrpcLimits,
}

impl GrpcStreamDecoder {
    /// New decoder honouring `limits.max_message_bytes` per message.
    pub fn new(limits: GrpcLimits) -> Self {
        Self {
            buf: Vec::new(),
            limits,
        }
    }

    /// Feed received body bytes, draining every complete length-prefixed
    /// message into a fresh `Vec`. A partial trailing frame stays buffered
    /// for the next `push`. Rejects compression and over-cap message lengths
    /// before allocating the message.
    ///
    /// This is a convenience wrapper over [`push_into`](Self::push_into) for
    /// callers that do not reuse output storage. Streaming loops that pull
    /// many chunks should call `push_into` with one reused `Vec` instead, to
    /// avoid a fresh output allocation per chunk.
    pub fn push<Resp: Message + Default>(&mut self, bytes: &[u8]) -> Result<Vec<Resp>, GrpcError> {
        let mut out = Vec::new();
        self.push_into(bytes, &mut out)?;
        Ok(out)
    }

    /// Feed received body bytes, appending every newly complete message to
    /// `out`. The caller owns `out` and may reuse it across many chunks, so
    /// a steady stream pays no per-chunk output `Vec` allocation.
    ///
    /// Caps and compression are still enforced before any message is
    /// allocated. On error the bytes consumed so far are dropped from the
    /// internal buffer and any messages decoded before the error stay in
    /// `out`; a gRPC stream error is terminal, so those earlier messages are
    /// valid and the error is final. ([`push`](Self::push) preserves the
    /// all-or-nothing shape by discarding its private `Vec` on error.)
    pub fn push_into<Resp: Message + Default>(
        &mut self,
        bytes: &[u8],
        out: &mut Vec<Resp>,
    ) -> Result<(), GrpcError> {
        self.buf.extend_from_slice(bytes);
        let mut cursor = 0;
        let result = loop {
            let remaining = self.buf.len() - cursor;
            if remaining < GRPC_FRAME_HEADER_LEN {
                break Ok(());
            }
            if self.buf[cursor] != 0 {
                break Err(GrpcError::CompressedUnsupported);
            }
            let len = u32::from_be_bytes([
                self.buf[cursor + 1],
                self.buf[cursor + 2],
                self.buf[cursor + 3],
                self.buf[cursor + 4],
            ]) as usize;
            if len > self.limits.max_message_bytes {
                break Err(GrpcError::MessageTooLarge {
                    len,
                    max: self.limits.max_message_bytes,
                });
            }
            if remaining < GRPC_FRAME_HEADER_LEN + len {
                // The frame is not all here yet — wait for more bytes.
                break Ok(());
            }
            let mut frame_cursor = cursor;
            match decode_one_grpc_message::<Resp>(&self.buf, &mut frame_cursor, self.limits) {
                Ok(message) => {
                    cursor = frame_cursor;
                    out.push(message);
                }
                Err(error) => break Err(error),
            }
        };
        self.buf.drain(..cursor);
        result
    }

    /// Assert the stream ended on a frame boundary. Leftover buffered
    /// bytes mean the final message was truncated.
    pub fn finish(&self) -> Result<(), GrpcError> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(GrpcError::BadFrame)
        }
    }
}

/// One typed item produced by folding a streamed-response chunk through
/// [`GrpcClient::decode_stream_chunk`]. The gRPC status is first-class:
/// a server-streaming response ends with exactly one
/// [`GrpcStreamItem::Status`], never a silent stop.
///
/// A stream item is not the response message — you cannot treat it as the
/// decoded `Resp` and silently drop the terminal `Status`/`Transport`
/// arms. Extracting a message requires matching `Message(..)`:
///
/// ```compile_fail
/// let item: tina_http::GrpcStreamItem<u64> = unimplemented!();
/// let _message: u64 = item; // does not compile: status is a separate arm
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrpcStreamItem<Resp> {
    /// One decoded response message.
    Message(Resp),
    /// The final gRPC status (from `End` trailers). Terminal.
    Status(GrpcStatus),
    /// The stream died before a gRPC status (reset / closed / protocol
    /// error). Terminal; not a gRPC status.
    Transport(Http2ResponseChunk),
    /// A response frame was not well-formed gRPC (compression, oversize,
    /// truncated, or undecodable). Terminal.
    Malformed(GrpcError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Http2ClientResponse;
    use http::StatusCode;
    use tina::{Address, AddressGeneration, IsolateId, ShardId};

    #[derive(Clone, PartialEq, prost::Message)]
    struct Empty {}

    #[derive(Clone, PartialEq, prost::Message)]
    struct Reply {
        #[prost(uint64, tag = "1")]
        value: u64,
    }

    fn framed(value: u64) -> Vec<u8> {
        encode_grpc_message(&Reply { value }, GrpcLimits::default()).expect("frame reply")
    }

    #[test]
    fn push_into_reuses_one_output_buffer_across_chunks() {
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let mut out: Vec<Reply> = Vec::new();

        // First chunk: two complete messages at once.
        let mut chunk = framed(1);
        chunk.extend_from_slice(&framed(2));
        decoder.push_into(&chunk, &mut out).expect("push two");
        assert_eq!(out, vec![Reply { value: 1 }, Reply { value: 2 }]);

        // Drain (keeps capacity) and reuse the same buffer for the next chunk.
        out.clear();
        let cap_after_first = out.capacity();
        decoder.push_into(&framed(3), &mut out).expect("push one");
        assert_eq!(out, vec![Reply { value: 3 }]);
        // No reallocation was needed for the second, smaller chunk.
        assert!(out.capacity() >= cap_after_first);
    }

    #[test]
    fn push_into_buffers_partial_frame_across_chunks() {
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let mut out: Vec<Reply> = Vec::new();

        let whole = framed(7);
        let split = whole.len() - 2;
        decoder.push_into(&whole[..split], &mut out).expect("first");
        assert!(out.is_empty(), "partial frame must not yield a message yet");
        decoder.push_into(&whole[split..], &mut out).expect("rest");
        assert_eq!(out, vec![Reply { value: 7 }]);
    }

    #[test]
    fn push_into_rejects_compressed_frame() {
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let mut out: Vec<Reply> = Vec::new();
        let mut chunk = framed(1);
        chunk[0] = 1; // compression flag set
        assert!(matches!(
            decoder.push_into(&chunk, &mut out),
            Err(GrpcError::CompressedUnsupported)
        ));
    }

    #[test]
    fn push_into_rejects_over_cap_before_decoding() {
        let limits = GrpcLimits {
            max_message_bytes: 4,
            ..GrpcLimits::default()
        };
        let mut decoder = GrpcStreamDecoder::new(limits);
        let mut out: Vec<Reply> = Vec::new();
        // Only the 5-byte frame header is present, declaring a len over the cap.
        // The decoder must reject on the length alone, before any message body
        // is read or allocated.
        let header = [0u8, 0, 0, 0, 64];
        assert!(matches!(
            decoder.push_into(&header, &mut out),
            Err(GrpcError::MessageTooLarge { len: 64, max: 4 })
        ));
        assert!(out.is_empty());
    }

    #[test]
    fn compact_grpc_unary_outcome_decodes_ok_message() {
        let client = dummy_client();
        let body = framed(99);
        let out: GrpcUnaryOutcome<Reply> =
            client.decode_unary(Http2ClientOutcome::GrpcUnaryReplied {
                status: StatusCode::OK,
                grpc_status: Some(0),
                grpc_message: None,
                body,
            });
        assert_eq!(out, GrpcUnaryOutcome::Ok(Reply { value: 99 }));
    }

    #[test]
    fn compact_grpc_unary_outcome_surfaces_non_ok_status_with_message() {
        let client = dummy_client();
        // grpc-message arrives percent-encoded on the wire; the compact path
        // must decode it the same way the header-map path does.
        let out: GrpcUnaryOutcome<Reply> =
            client.decode_unary(Http2ClientOutcome::GrpcUnaryReplied {
                status: StatusCode::OK,
                grpc_status: Some(GrpcStatusCode::PermissionDenied.as_u16()),
                grpc_message: Some("no%20access".to_owned()),
                body: Vec::new(),
            });
        match out {
            GrpcUnaryOutcome::Status(status) => {
                assert_eq!(status.code, GrpcStatusCode::PermissionDenied);
                assert_eq!(status.message.as_deref(), Some("no access"));
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[test]
    fn compact_grpc_unary_outcome_missing_status_on_200_is_malformed() {
        let client = dummy_client();
        let out: GrpcUnaryOutcome<Reply> =
            client.decode_unary(Http2ClientOutcome::GrpcUnaryReplied {
                status: StatusCode::OK,
                grpc_status: None,
                grpc_message: None,
                body: Vec::new(),
            });
        assert!(matches!(
            out,
            GrpcUnaryOutcome::Malformed(GrpcError::MissingTrailers)
        ));
    }

    #[test]
    fn compact_grpc_unary_outcome_non_200_without_status_synthesizes() {
        let client = dummy_client();
        let out: GrpcUnaryOutcome<Reply> =
            client.decode_unary(Http2ClientOutcome::GrpcUnaryReplied {
                status: StatusCode::NOT_FOUND,
                grpc_status: None,
                grpc_message: None,
                body: Vec::new(),
            });
        match out {
            GrpcUnaryOutcome::Status(status) => {
                assert_eq!(status.code, GrpcStatusCode::Unimplemented);
            }
            other => panic!("expected synthesized Status, got {other:?}"),
        }
    }

    #[test]
    fn frame_into_packs_multiple_messages_into_one_buffer() {
        let client = dummy_client();
        // Frame three dynamic messages into one reused buffer — a valid
        // concatenated gRPC body, not three separate Vecs.
        let mut body = Vec::new();
        for value in [10u64, 20, 30] {
            client
                .frame_into(&mut body, &Reply { value })
                .expect("frame into shared buffer");
        }
        // The body decodes back to exactly those three messages.
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let mut out: Vec<Reply> = Vec::new();
        decoder
            .push_into(&body, &mut out)
            .expect("decode packed body");
        assert_eq!(
            out,
            vec![
                Reply { value: 10 },
                Reply { value: 20 },
                Reply { value: 30 }
            ]
        );
    }

    #[test]
    fn frame_into_into_empty_buffer_matches_frame() {
        let client = dummy_client();
        let mut buf = Vec::new();
        client
            .frame_into(&mut buf, &Reply { value: 7 })
            .expect("frame_into");
        let direct = client.frame(&Reply { value: 7 }).expect("frame");
        assert_eq!(buf, direct, "reusable framing matches the one-Vec form");
    }

    #[test]
    fn frame_into_rejects_over_cap_before_writing() {
        let limits = GrpcLimits {
            max_message_bytes: 4,
            ..GrpcLimits::default()
        };
        let client = GrpcClient::new(dummy_client().connection(), limits);
        // Pre-seed the buffer so we can prove nothing was appended on failure.
        let mut buf = vec![0xAAu8; 3];
        let err = client
            .frame_into(&mut buf, &Reply { value: u64::MAX })
            .expect_err("over-cap message must fail before framing");
        assert!(matches!(err, GrpcError::EncodeTooLarge { max: 4, .. }));
        assert_eq!(buf, vec![0xAA, 0xAA, 0xAA], "buffer untouched on over-cap");
    }

    #[test]
    fn finish_rejects_truncated_trailing_frame() {
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let mut out: Vec<Reply> = Vec::new();
        let whole = framed(9);
        // Feed all but the last byte: a frame is buffered but incomplete.
        decoder
            .push_into(&whole[..whole.len() - 1], &mut out)
            .expect("partial");
        assert!(out.is_empty());
        assert!(matches!(decoder.finish(), Err(GrpcError::BadFrame)));
    }

    #[test]
    fn stream_end_without_grpc_status_is_malformed() {
        let client = dummy_client();
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let items: Vec<GrpcStreamItem<Reply>> = client.decode_stream_chunk(
            &mut decoder,
            Http2ResponseChunk::End {
                trailers: HeaderMap::new(),
            },
        );

        assert_eq!(
            items,
            vec![GrpcStreamItem::Malformed(GrpcError::MissingTrailers)]
        );
    }

    // -- Wire-negative coverage at the user's stream-decode surface --------
    //
    // A streaming caller folds each pulled `Http2ResponseChunk` through
    // `decode_stream_chunk` and matches the returned `GrpcStreamItem`s
    // (Message / Status / Transport / Malformed) — exactly what
    // `collect_grpc_stream` does in the live tests. These cases feed the
    // malformed / edge chunks a hostile or dying peer can put on the wire and
    // assert the user-visible typed item, never a panic.

    fn empty_framed() -> Vec<u8> {
        // A well-formed frame carrying a zero-length message body.
        encode_grpc_message(&Empty {}, GrpcLimits::default()).expect("frame empty")
    }

    #[test]
    fn stream_nonzero_compression_flag_is_malformed_item() {
        let client = dummy_client();
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let mut chunk = framed(5);
        chunk[0] = 1; // reserved compression flag set — identity only supported
        let items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(chunk));
        assert_eq!(
            items,
            vec![GrpcStreamItem::Malformed(GrpcError::CompressedUnsupported)],
            "a compressed response frame must surface as a typed Malformed item",
        );
    }

    #[test]
    fn stream_over_cap_length_is_malformed_item() {
        let limits = GrpcLimits {
            max_message_bytes: 4,
            ..GrpcLimits::default()
        };
        let client = GrpcClient::new(dummy_client().connection(), limits);
        let mut decoder = GrpcStreamDecoder::new(limits);
        // Frame header alone declares a body length over the cap. The decoder
        // must reject on the declared length before buffering/allocating.
        let header = vec![0u8, 0, 0, 0, 64];
        let items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(header));
        assert_eq!(
            items,
            vec![GrpcStreamItem::Malformed(GrpcError::MessageTooLarge {
                len: 64,
                max: 4
            })],
            "an oversized declared length must surface as a typed Malformed item",
        );
    }

    #[test]
    fn stream_truncated_frame_at_end_is_malformed_item() {
        let client = dummy_client();
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let whole = framed(9);
        // Deliver all but the final body byte, then END_STREAM: a frame is
        // still buffered, so the stream ended mid-message.
        let partial = whole[..whole.len() - 1].to_vec();
        let data_items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(partial));
        assert!(
            data_items.is_empty(),
            "a partial frame must not yield a message yet"
        );
        let end_items: Vec<GrpcStreamItem<Reply>> = client.decode_stream_chunk(
            &mut decoder,
            Http2ResponseChunk::End {
                trailers: HeaderMap::new(),
            },
        );
        assert_eq!(
            end_items,
            vec![GrpcStreamItem::Malformed(GrpcError::BadFrame)],
            "a truncated trailing frame at END_STREAM must surface as Malformed, not a clean Status",
        );
    }

    #[test]
    fn stream_closed_mid_stream_is_transport_item_not_status() {
        let client = dummy_client();
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        // One clean message, then the connection dies before END_STREAM.
        let msg_items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(framed(1)));
        assert_eq!(msg_items, vec![GrpcStreamItem::Message(Reply { value: 1 })]);
        let closed_items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Closed);
        assert_eq!(
            closed_items,
            vec![GrpcStreamItem::Transport(Http2ResponseChunk::Closed)],
            "a connection closed before a gRPC status must surface as Transport, not a fabricated status",
        );
    }

    #[test]
    fn stream_multiple_concatenated_messages_yield_all_in_order() {
        let client = dummy_client();
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        // Three complete frames packed into one DATA chunk.
        let mut chunk = framed(1);
        chunk.extend_from_slice(&framed(2));
        chunk.extend_from_slice(&framed(3));
        let items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(chunk));
        assert_eq!(
            items,
            vec![
                GrpcStreamItem::Message(Reply { value: 1 }),
                GrpcStreamItem::Message(Reply { value: 2 }),
                GrpcStreamItem::Message(Reply { value: 3 }),
            ],
            "a chunk carrying several messages must yield each as its own Message item, in order",
        );
    }

    #[test]
    fn stream_zero_length_message_decodes_to_default() {
        let client = dummy_client();
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let items: Vec<GrpcStreamItem<Empty>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(empty_framed()));
        assert_eq!(
            items,
            vec![GrpcStreamItem::Message(Empty {})],
            "a valid zero-length message frame must decode to a default message, not be dropped",
        );
    }

    #[test]
    fn stream_truncated_header_buffers_then_completes() {
        let client = dummy_client();
        let mut decoder = GrpcStreamDecoder::new(GrpcLimits::default());
        let whole = framed(7);
        // First deliver only 3 bytes: less than the 5-byte frame header. The
        // decoder must buffer and yield nothing, never mis-read the length.
        let head_items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(whole[..3].to_vec()));
        assert!(
            head_items.is_empty(),
            "a truncated frame header must buffer, not yield or panic"
        );
        // The rest arrives and completes exactly one message.
        let rest_items: Vec<GrpcStreamItem<Reply>> =
            client.decode_stream_chunk(&mut decoder, Http2ResponseChunk::Data(whole[3..].to_vec()));
        assert_eq!(
            rest_items,
            vec![GrpcStreamItem::Message(Reply { value: 7 })]
        );
    }

    fn dummy_client() -> GrpcClient {
        let connection = Address::new_with_generation(
            ShardId::new(1),
            IsolateId::new(1),
            AddressGeneration::new(0),
        );
        GrpcClient::new(connection, GrpcLimits::default())
    }

    fn response(
        status: u16,
        headers: HeaderMap,
        trailers: HeaderMap,
        body: Vec<u8>,
    ) -> Http2ClientOutcome {
        Http2ClientOutcome::Replied(Http2ClientResponse {
            status: StatusCode::from_u16(status).unwrap(),
            headers,
            body,
            trailers,
        })
    }

    fn grpc_status_headers(code: u16) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            http::HeaderName::from_static("grpc-status"),
            HeaderValue::from_str(&code.to_string()).unwrap(),
        );
        h
    }

    #[test]
    fn non_replied_outcome_is_transport() {
        let client = dummy_client();
        let out: GrpcUnaryOutcome<Empty> = client.decode_unary(Http2ClientOutcome::Closed);
        assert!(matches!(
            out,
            GrpcUnaryOutcome::Transport(Http2ClientOutcome::Closed)
        ));
    }

    #[test]
    fn http_200_without_grpc_status_is_malformed_missing_trailers() {
        let client = dummy_client();
        let out: GrpcUnaryOutcome<Empty> = client.decode_unary(response(
            200,
            HeaderMap::new(),
            HeaderMap::new(),
            Vec::new(),
        ));
        assert!(matches!(
            out,
            GrpcUnaryOutcome::Malformed(GrpcError::MissingTrailers)
        ));
    }

    #[test]
    fn non_200_without_grpc_status_synthesizes_a_typed_status() {
        let client = dummy_client();
        // 404 → Unimplemented per the gRPC HTTP-status mapping.
        let out: GrpcUnaryOutcome<Empty> = client.decode_unary(response(
            404,
            HeaderMap::new(),
            HeaderMap::new(),
            Vec::new(),
        ));
        match out {
            GrpcUnaryOutcome::Status(status) => {
                assert_eq!(status.code, GrpcStatusCode::Unimplemented);
            }
            other => panic!("expected synthesized Status, got {other:?}"),
        }
    }

    #[test]
    fn explicit_grpc_status_wins_over_http_status() {
        let client = dummy_client();
        // grpc-status in trailers is authoritative even if HTTP were odd.
        let out: GrpcUnaryOutcome<Empty> = client.decode_unary(response(
            200,
            HeaderMap::new(),
            grpc_status_headers(GrpcStatusCode::PermissionDenied.as_u16()),
            Vec::new(),
        ));
        match out {
            GrpcUnaryOutcome::Status(status) => {
                assert_eq!(status.code, GrpcStatusCode::PermissionDenied);
            }
            other => panic!("expected Status(PermissionDenied), got {other:?}"),
        }
    }

    #[test]
    fn trailers_only_status_in_headers_is_read() {
        let client = dummy_client();
        // Trailers-only response: grpc-status lands in headers.
        let out: GrpcUnaryOutcome<Empty> = client.decode_unary(response(
            200,
            grpc_status_headers(GrpcStatusCode::NotFound.as_u16()),
            HeaderMap::new(),
            Vec::new(),
        ));
        assert!(matches!(
            out,
            GrpcUnaryOutcome::Status(GrpcStatus {
                code: GrpcStatusCode::NotFound,
                ..
            })
        ));
    }

    #[test]
    fn http_status_mapping_table_is_spec_shaped() {
        let cases = [
            (400, GrpcStatusCode::Internal),
            (401, GrpcStatusCode::Unauthenticated),
            (403, GrpcStatusCode::PermissionDenied),
            (404, GrpcStatusCode::Unimplemented),
            (429, GrpcStatusCode::Unavailable),
            (502, GrpcStatusCode::Unavailable),
            (503, GrpcStatusCode::Unavailable),
            (504, GrpcStatusCode::Unavailable),
            (418, GrpcStatusCode::Unknown),
        ];
        for (http, expected) in cases {
            let got = http_status_to_grpc_status(StatusCode::from_u16(http).unwrap()).code;
            assert_eq!(got, expected, "HTTP {http}");
        }
    }

    #[test]
    fn grpc_client_rejects_relative_method_paths_before_wire() {
        let client = dummy_client();
        let err = client
            .unary_request("pkg.Service/Method", &Empty {})
            .expect_err("relative unary path is invalid");
        assert!(matches!(err, GrpcError::InvalidPath(path) if path == "pkg.Service/Method"));

        let source = Address::new_with_generation(
            ShardId::new(1),
            IsolateId::new(2),
            AddressGeneration::new(0),
        );
        let err = client
            .client_streaming_request("pkg.Service/Upload", source)
            .expect_err("relative client-streaming path is invalid");
        assert!(matches!(err, GrpcError::InvalidPath(path) if path == "pkg.Service/Upload"));

        let source = Address::new_with_generation(
            ShardId::new(1),
            IsolateId::new(3),
            AddressGeneration::new(0),
        );
        let err = client
            .bidi_request("pkg.Service/Chat", source)
            .expect_err("relative bidi path is invalid");
        assert!(matches!(err, GrpcError::InvalidPath(path) if path == "pkg.Service/Chat"));
    }

    #[test]
    fn unary_request_uses_compact_grpc_http2_submit() {
        let client = dummy_client();
        let msg = client
            .unary_request("/pkg.Service/Method", &Empty {})
            .expect("valid unary request");

        match msg {
            Http2ClientMsg::SubmitGrpcUnary(req) => {
                assert_eq!(req.path(), "/pkg.Service/Method");
                assert_eq!(req.body_len(), GRPC_FRAME_HEADER_LEN);
                assert!(!req.body_is_shared());
            }
            other => panic!("expected compact gRPC submit, got {other:?}"),
        }
    }

    #[test]
    fn unary_template_reuses_validated_path() {
        let client = dummy_client();
        let template = client
            .unary_template("/pkg.Service/Method")
            .expect("valid template");

        let first = template.request(&Empty {}).expect("first request");
        let second = template.request(&Empty {}).expect("second request");
        match (first, second) {
            (Http2ClientMsg::SubmitGrpcUnary(first), Http2ClientMsg::SubmitGrpcUnary(second)) => {
                assert!(Arc::ptr_eq(first.path_arc(), second.path_arc()));
                assert!(!first.body_is_shared());
                assert!(!second.body_is_shared());
            }
            other => panic!("expected compact gRPC submits, got {other:?}"),
        }
    }

    #[test]
    fn preframed_unary_reuses_path_and_body() {
        let client = dummy_client();
        let template = client
            .unary_template("/pkg.Service/Method")
            .expect("valid template");
        let preframed = template.preframed(&Empty {}).expect("preframe");

        let first = preframed.request();
        let second = preframed.request();
        match (first, second) {
            (Http2ClientMsg::SubmitGrpcUnary(first), Http2ClientMsg::SubmitGrpcUnary(second)) => {
                assert!(Arc::ptr_eq(first.path_arc(), second.path_arc()));
                assert_eq!(first.body_len(), GRPC_FRAME_HEADER_LEN);
                assert_eq!(second.body_len(), GRPC_FRAME_HEADER_LEN);
                assert!(first.body_is_shared());
                assert!(second.body_is_shared());
            }
            other => panic!("expected preframed gRPC submits, got {other:?}"),
        }
    }
}
