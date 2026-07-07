struct Driver;

tina::flow! {
    flow DuplicateStepFlow for Driver {
        reply u32;

        step Done() -> u32 {
            ::tina::reply_to(req, 1)
        }

        step Done() -> u32 {
            ::tina::reply_to(req, 2)
        }
    }
}

fn main() {}
