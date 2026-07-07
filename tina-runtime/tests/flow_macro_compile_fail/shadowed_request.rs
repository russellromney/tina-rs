struct Driver;

tina::flow! {
    flow ShadowedRequestFlow for Driver {
        reply u32;

        step Done() -> u32 {
            let _ = (|req: u32| req)(1);
            match outcome {
                ::tina_runtime::CallOutcome::Replied(_) => ::tina::noop(),
                _ => ::tina::noop(),
            }
        }
    }
}

fn main() {}
