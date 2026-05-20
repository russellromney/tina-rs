//! HTTP/2 frame encode/decode and standard frame builders.
//!
//! Internal to the `http2` module: shared between the server isolate and
//! the native client. Not re-exported on the public API.

use super::errors::Http2ProtocolError;

pub(super) const CLIENT_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
pub(super) const FRAME_HEADER_LEN: usize = 9;
pub(super) const DEFAULT_WINDOW: i32 = 65_535;
pub(super) const READ_CHUNK: usize = 16 * 1024;
pub(super) const WINDOW_CREDIT_FLUSH_THRESHOLD: u32 = 16 * 1024;

pub(super) const FLAG_ACK: u8 = 0x1;
pub(super) const FLAG_END_STREAM: u8 = 0x1;
pub(super) const FLAG_END_HEADERS: u8 = 0x4;
pub(super) const FLAG_PADDED: u8 = 0x8;
pub(super) const FLAG_PRIORITY: u8 = 0x20;

pub(super) const FRAME_DATA: u8 = 0x0;
pub(super) const FRAME_HEADERS: u8 = 0x1;
pub(super) const FRAME_PRIORITY: u8 = 0x2;
pub(super) const FRAME_RST_STREAM: u8 = 0x3;
pub(super) const FRAME_SETTINGS: u8 = 0x4;
pub(super) const FRAME_PUSH_PROMISE: u8 = 0x5;
pub(super) const FRAME_PING: u8 = 0x6;
pub(super) const FRAME_GOAWAY: u8 = 0x7;
pub(super) const FRAME_WINDOW_UPDATE: u8 = 0x8;
pub(super) const FRAME_CONTINUATION: u8 = 0x9;

pub(super) const PRIORITY_PAYLOAD_LEN: usize = 5;

#[derive(Debug, Clone)]
pub(super) struct Frame {
    pub(super) ty: u8,
    pub(super) flags: u8,
    pub(super) stream_id: u32,
    pub(super) payload: Vec<u8>,
}

impl Frame {
    pub(super) fn new(ty: u8, flags: u8, stream_id: u32, payload: Vec<u8>) -> Self {
        Self {
            ty,
            flags,
            stream_id,
            payload,
        }
    }

    pub(super) fn encode(&self) -> Vec<u8> {
        let len = self.payload.len();
        assert!(len <= 0x00ff_ffff, "HTTP/2 frame payload too large");
        let mut out = Vec::with_capacity(FRAME_HEADER_LEN + len);
        out.push(((len >> 16) & 0xff) as u8);
        out.push(((len >> 8) & 0xff) as u8);
        out.push((len & 0xff) as u8);
        out.push(self.ty);
        out.push(self.flags);
        let sid = self.stream_id & 0x7fff_ffff;
        out.extend_from_slice(&sid.to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

pub(super) fn try_decode_frame(
    buffer: &[u8],
    max_frame_size: usize,
) -> Result<Option<(Frame, usize)>, Http2ProtocolError> {
    if buffer.len() < FRAME_HEADER_LEN {
        return Ok(None);
    }
    let len = ((buffer[0] as usize) << 16) | ((buffer[1] as usize) << 8) | buffer[2] as usize;
    if len > max_frame_size {
        return Err(Http2ProtocolError::FrameTooLarge {
            len,
            max: max_frame_size,
        });
    }
    let total = FRAME_HEADER_LEN
        .checked_add(len)
        .ok_or(Http2ProtocolError::FrameTooLarge {
            len,
            max: max_frame_size,
        })?;
    if buffer.len() < total {
        return Ok(None);
    }
    let ty = buffer[3];
    let flags = buffer[4];
    let mut sid_bytes = [0_u8; 4];
    sid_bytes.copy_from_slice(&buffer[5..9]);
    let stream_id = u32::from_be_bytes(sid_bytes) & 0x7fff_ffff;
    let payload = buffer[9..total].to_vec();
    Ok(Some((
        Frame {
            ty,
            flags,
            stream_id,
            payload,
        },
        total,
    )))
}

pub(super) fn settings_frame(ack: bool) -> Frame {
    Frame::new(
        FRAME_SETTINGS,
        if ack { FLAG_ACK } else { 0 },
        0,
        Vec::new(),
    )
}

pub(super) fn rst_stream_frame(stream_id: u32, error: u32) -> Frame {
    Frame::new(FRAME_RST_STREAM, 0, stream_id, error.to_be_bytes().to_vec())
}

pub(super) fn goaway_frame(last_stream_id: u32, error: u32) -> Frame {
    let mut payload = Vec::with_capacity(8);
    payload.extend_from_slice(&(last_stream_id & 0x7fff_ffff).to_be_bytes());
    payload.extend_from_slice(&error.to_be_bytes());
    Frame::new(FRAME_GOAWAY, 0, 0, payload)
}

pub(super) fn window_update_frame(stream_id: u32, increment: u32) -> Frame {
    Frame::new(
        FRAME_WINDOW_UPDATE,
        0,
        stream_id,
        (increment & 0x7fff_ffff).to_be_bytes().to_vec(),
    )
}

pub(super) fn headers_frame(stream_id: u32, end_stream: bool, block: Vec<u8>) -> Frame {
    let flags = FLAG_END_HEADERS | if end_stream { FLAG_END_STREAM } else { 0 };
    Frame::new(FRAME_HEADERS, flags, stream_id, block)
}

pub(super) fn data_frame(stream_id: u32, end_stream: bool, data: Vec<u8>) -> Frame {
    Frame::new(
        FRAME_DATA,
        if end_stream { FLAG_END_STREAM } else { 0 },
        stream_id,
        data,
    )
}

pub(super) fn data_payload(frame: &Frame) -> Result<Vec<u8>, Http2ProtocolError> {
    if frame.flags & FLAG_PADDED == 0 {
        return Ok(frame.payload.clone());
    }
    let Some((&pad_len, rest)) = frame.payload.split_first() else {
        return Err(Http2ProtocolError::BadFrameLength);
    };
    let pad_len = usize::from(pad_len);
    if pad_len > rest.len() {
        return Err(Http2ProtocolError::BadFrameLength);
    }
    Ok(rest[..rest.len() - pad_len].to_vec())
}

pub(super) fn headers_payload(frame: &Frame) -> Result<&[u8], Http2ProtocolError> {
    let mut offset = 0usize;
    let mut pad_len = 0usize;
    if frame.flags & FLAG_PADDED != 0 {
        let Some((&pad, _)) = frame.payload.split_first() else {
            return Err(Http2ProtocolError::BadFrameLength);
        };
        pad_len = usize::from(pad);
        offset = 1;
    }
    if frame.flags & FLAG_PRIORITY != 0 {
        let next = offset
            .checked_add(5)
            .ok_or(Http2ProtocolError::BadFrameLength)?;
        if frame.payload.len() < next {
            return Err(Http2ProtocolError::BadFrameLength);
        }
        offset = next;
    }
    let available = frame
        .payload
        .len()
        .checked_sub(offset)
        .ok_or(Http2ProtocolError::BadFrameLength)?;
    if pad_len > available {
        return Err(Http2ProtocolError::BadFrameLength);
    }
    let end = frame.payload.len() - pad_len;
    Ok(&frame.payload[offset..end])
}

/// Apply a positive `WINDOW_UPDATE` increment to a signed 32-bit window,
/// returning the new value or [`Http2ProtocolError::WindowOverflow`] when
/// the result would exceed `i32::MAX`.
pub(super) fn add_window(current: i32, increment: u32) -> Result<i32, Http2ProtocolError> {
    let next = current as i64 + increment as i64;
    if next > i32::MAX as i64 {
        return Err(Http2ProtocolError::WindowOverflow);
    }
    Ok(next as i32)
}
