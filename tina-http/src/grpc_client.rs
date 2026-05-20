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

use http::{HeaderMap, HeaderValue, Method, StatusCode};
use prost::Message;
use tina::{Address, Shard};

use crate::grpc::{
    GRPC_FRAME_HEADER_LEN, GrpcError, GrpcLimits, GrpcStatus, GrpcStatusCode,
    decode_one_grpc_message,
};
use crate::grpc::{encode_grpc_message, grpc_status_from_header_map};
use crate::http2::{
    Http2ClientConnection, Http2ClientLimits, Http2ClientMsg, Http2ClientOutcome, Http2ClientReply,
    Http2ClientRequest, Http2ClientRequestBody, Http2ClientStreamCall, Http2ResponseChunk,
    Http2Target,
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
        // gRPC paths are absolute: `/package.Service/Method`. A relative
        // path produces an invalid `:path` pseudo-header on the wire.
        debug_assert!(
            full_method_path.starts_with('/'),
            "gRPC method path must be absolute (start with '/'), got {full_method_path:?}",
        );
        let body = encode_grpc_message(message, self.limits)?;
        Ok(Http2ClientMsg::Submit(Http2ClientRequest {
            method: Method::POST,
            path: full_method_path.to_owned(),
            headers: grpc_headers(),
            body,
        }))
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
        let response = match outcome {
            Http2ClientOutcome::Replied(response) => response,
            // Anything that is not a completed HTTP response is a
            // transport-level failure, not a gRPC status.
            transport => return GrpcUnaryOutcome::Transport(transport),
        };
        // An explicit grpc-status wins regardless of HTTP status (gRPC
        // servers always send HTTP 200 and put status in trailers, or in
        // headers for a trailers-only response).
        let status = grpc_status_from_header_map(&response.trailers)
            .or_else(|| grpc_status_from_header_map(&response.headers));
        let status = match status {
            Some(status) => status,
            None => {
                return if response.status.as_u16() == 200 {
                    // A 200 gRPC response MUST carry a grpc-status.
                    GrpcUnaryOutcome::Malformed(GrpcError::MissingTrailers)
                } else {
                    // No grpc-status + non-200: a proxy/infra failure.
                    // Synthesize the status the gRPC spec prescribes.
                    GrpcUnaryOutcome::Status(http_status_to_grpc_status(response.status))
                };
            }
        };
        if status.code != GrpcStatusCode::Ok {
            return GrpcUnaryOutcome::Status(status);
        }
        // OK status: decode exactly one response message from the body.
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
        assert_grpc_path(full_method_path);
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
    ) -> Http2ClientMsg {
        assert_grpc_path(full_method_path);
        Http2ClientMsg::SubmitStreaming(crate::http2::Http2ClientStreamingRequest {
            method: Method::POST,
            path: full_method_path.to_owned(),
            headers: grpc_headers(),
            source,
        })
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
    ) -> Http2ClientMsg {
        assert_grpc_path(full_method_path);
        Http2ClientMsg::OpenStream(Http2ClientStreamCall {
            method: Method::POST,
            path: full_method_path.to_owned(),
            headers: grpc_headers(),
            body: Http2ClientRequestBody::Stream(source),
        })
    }

    /// Encode one message as a length-prefixed gRPC frame, for building a
    /// streaming request `source` (e.g. an `IterBodySource` over pre-framed
    /// messages). Honours the configured message cap.
    pub fn frame<Req: Message>(&self, message: &Req) -> Result<Vec<u8>, GrpcError> {
        encode_grpc_message(message, self.limits)
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
    /// yields a [`GrpcStreamItem::Status`] (from the trailers, defaulting
    /// to `Ok` when END_STREAM carries none — the head may have carried
    /// it); a transport teardown yields [`GrpcStreamItem::Transport`].
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
                let status = grpc_status_from_header_map(&trailers)
                    .unwrap_or_else(|| GrpcStatus::new(GrpcStatusCode::Ok));
                vec![GrpcStreamItem::Status(status)]
            }
            // Reset / Closed / ProtocolError — the stream died before a
            // gRPC status. Surface it as a transport item, not a status.
            other => vec![GrpcStreamItem::Transport(other)],
        }
    }
}

/// gRPC method paths are absolute (`/package.Service/Method`); a relative
/// path produces an invalid `:path` pseudo-header on the wire.
fn assert_grpc_path(full_method_path: &str) {
    debug_assert!(
        full_method_path.starts_with('/'),
        "gRPC method path must be absolute (start with '/'), got {full_method_path:?}",
    );
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
    /// message. A partial trailing frame stays buffered for the next
    /// `push`. Rejects compression and over-cap message lengths before
    /// allocating the message.
    pub fn push<Resp: Message + Default>(&mut self, bytes: &[u8]) -> Result<Vec<Resp>, GrpcError> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut cursor = 0;
        loop {
            let remaining = self.buf.len() - cursor;
            if remaining < GRPC_FRAME_HEADER_LEN {
                break;
            }
            if self.buf[cursor] != 0 {
                return Err(GrpcError::CompressedUnsupported);
            }
            let len = u32::from_be_bytes([
                self.buf[cursor + 1],
                self.buf[cursor + 2],
                self.buf[cursor + 3],
                self.buf[cursor + 4],
            ]) as usize;
            if len > self.limits.max_message_bytes {
                return Err(GrpcError::MessageTooLarge {
                    len,
                    max: self.limits.max_message_bytes,
                });
            }
            if remaining < GRPC_FRAME_HEADER_LEN + len {
                // The frame is not all here yet — wait for more bytes.
                break;
            }
            let mut frame_cursor = cursor;
            let message =
                decode_one_grpc_message::<Resp>(&self.buf, &mut frame_cursor, self.limits)?;
            cursor = frame_cursor;
            out.push(message);
        }
        self.buf.drain(..cursor);
        Ok(out)
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
}
