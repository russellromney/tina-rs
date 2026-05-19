//! Negative fixture: private internal split events cannot be constructed
//! outside their owner module.

mod cache {
    #[derive(Debug)]
    enum Event {
        FillDone,
    }

    #[derive(Debug)]
    pub enum Request {
        Read,
    }

    pub struct Service;

    #[tina_runtime::isolate(event = Event, request = Request, reply = ())]
    impl Service {
        fn handle_event(
            &mut self,
            _event: Event,
            _ctx: &mut tina::Context<'_, tina::SingleShard, Self::Reply>,
        ) -> tina::Effect<Self> {
            tina::noop()
        }

        fn handle_request(
            &mut self,
            _request: Request,
            call: tina::CallContext<'_, Self>,
        ) -> tina::Effect<Self> {
            call.reply(())
        }
    }

    pub fn event_address(
        raw: tina::Address<tina::ServiceMessage<Event, Request>, ()>,
    ) -> tina::ServiceEventAddress<Event, Request> {
        tina::ServiceEventAddress::from_send_address(raw.send_only())
    }
}

fn main() {
    let _event = cache::Event::FillDone;
}
