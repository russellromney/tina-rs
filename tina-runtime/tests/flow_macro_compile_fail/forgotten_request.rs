#![deny(unused_variables)]

struct Driver;

tina::flow! {
    flow ForgottenRequestFlow for Driver {
        reply u32;

        step Done() -> u32 {
            match outcome {
                ::tina_runtime::CallOutcome::Replied(_) => ::tina::noop(),
                _ => ::tina::noop(),
            }
        }
    }
}

fn main() {}
