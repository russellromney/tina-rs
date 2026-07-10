#[derive(Debug)]
enum Request {
    Read,
}

#[derive(Debug)]
struct Reply;

struct Service;

#[tina_runtime::isolate(request = Request, reply = Reply)]
impl Service {
    fn handle_request(
        &mut self,
        request: Request,
        call: tina::RequestCall<'_, Self>,
    ) -> tina::RequestEffect<Self> {
        match request {
            Request::Read => call.reply(Reply),
        }
    }
}

fn main() {}
