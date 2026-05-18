//! Negative fixture: split service events cannot be called as requests.

use std::time::Duration;

#[derive(Debug)]
enum Event {
    Filled,
}

#[derive(Debug)]
enum Request {
    Read,
}

fn main() {
    let raw: tina::Address<tina::ServiceMessage<Event, Request>, u32> =
        tina::Address::new_with_generation(
            tina::ShardId::new(0),
            tina::IsolateId::new(1),
            tina::AddressGeneration::new(0),
        );
    let requests = tina::ServiceRequestAddress::from_call_address(raw.callable());

    // Expected `Request`, found `Event`.
    let _ = tina_runtime::call_request(requests, Event::Filled, Duration::from_millis(1));
}
