//! The `Framer` trait is sealed: external code cannot implement it, so
//! the only framers are the crate's own `LineFramer` /
//! `LengthDelimitedFramer`. This must fail to compile.

use tina_codec::{FrameDecision, Framer};

struct MyFramer;

impl Framer for MyFramer {
    type Frame = Vec<u8>;
    type Malformed = ();

    fn feed(&mut self, _bytes: &[u8]) {}

    fn next_frame(&mut self) -> FrameDecision<Self::Frame, Self::Malformed> {
        FrameDecision::NeedMore
    }
}

fn main() {}
