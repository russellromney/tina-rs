struct Driver;

tina::flow! {
    flow ReservedCaptureFlow for Driver {
        reply u32;

        step Done(req: u32) -> u32 {
            ::tina::reply_to(req, req)
        }
    }
}

fn main() {}
