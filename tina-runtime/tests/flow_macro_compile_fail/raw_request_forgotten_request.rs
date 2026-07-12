#![deny(unused_variables)]

struct Driver;

tina::flow! {
    flow ForgottenRawRequestFlow for Driver {
        reply u32;

        step Woke() -> raw request tina_runtime::SleepReply {
            match outcome {
                Ok(()) | Err(_) => tina::noop(),
            }
        }
    }
}

fn main() {}
