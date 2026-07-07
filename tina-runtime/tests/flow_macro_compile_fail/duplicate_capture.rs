struct Driver;

tina::flow! {
    flow DuplicateCaptureFlow for Driver {
        reply u32;

        step Done(value: u32, value: u32) -> u32 {
            ::tina::reply_to(req, value)
        }
    }
}

fn main() {}
