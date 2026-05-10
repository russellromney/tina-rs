//! HTTP/1.1 over TCP or TLS. The enum keeps `TcpRead`/`TlsRead` (etc.)
//! distinct in the trace; callers match-dispatch on the variant and
//! call the runtime's `tcp_*` / `tls_*` helpers directly so the TLS
//! per-call deadline is only required where it applies.

use tina_runtime::{StreamId, TlsStreamId};

/// Per-stream transport rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HttpTransport {
    Tcp(StreamId),
    Tls(TlsStreamId),
}
