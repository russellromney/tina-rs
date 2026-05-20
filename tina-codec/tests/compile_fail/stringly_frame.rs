//! The decoded frame is typed bytes, not a string. Matching a
//! `FrameDecision::Frame` payload against a `&str` literal must fail to
//! compile — codec adapter state stays typed, not stringly.

use tina_codec::{FrameDecision, LineFramer};

fn main() {
    let mut framer = LineFramer::new(64);
    framer.feed(b"hello\n");
    match framer.next_frame() {
        // `Frame` carries `Vec<u8>`, not `&str`: this arm is a type error.
        FrameDecision::Frame("hello") => {}
        _ => {}
    }
}
