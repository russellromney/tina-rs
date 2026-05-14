#[test]
fn specimen_grpc_counter_smoke() {
    let value = specimen_grpc_counter::run_smoke().expect("specimen smoke");
    assert_eq!(value, 7);
}

#[tokio::test]
async fn specimen_grpc_counter_tonic_h2c_interop() {
    use http::uri::PathAndQuery;
    use tokio_stream::iter;
    use tonic::Request;
    use tonic::client::Grpc;
    use tonic::codec::ProstCodec;
    use tonic::transport::Endpoint;

    let server = specimen_grpc_counter::start_server().expect("start specimen server");
    let channel = Endpoint::from_shared(format!("http://{}", server.addr))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect tonic h2c");
    let mut client = Grpc::new(channel);

    client.ready().await.expect("tonic ready");
    let response: specimen_grpc_counter::CounterReply = client
        .unary(
            Request::new(specimen_grpc_counter::CounterRequest { delta: 5 }),
            PathAndQuery::from_static("/specimen.Counter/Increment"),
            ProstCodec::default(),
        )
        .await
        .expect("tonic unary")
        .into_inner();
    assert_eq!(response.value, 5);

    client.ready().await.expect("tonic ready");
    let response: specimen_grpc_counter::BlobReply = client
        .unary(
            Request::new(specimen_grpc_counter::CounterRequest { delta: 70_000 }),
            PathAndQuery::from_static("/specimen.Counter/BigBlob"),
            ProstCodec::default(),
        )
        .await
        .expect("tonic large unary")
        .into_inner();
    assert_eq!(response.bytes.len(), 70_000);
    assert_eq!(response.bytes.first(), Some(&7));

    client.ready().await.expect("tonic ready");
    let mut stream = client
        .server_streaming(
            Request::new(specimen_grpc_counter::CounterRequest { delta: 5 }),
            PathAndQuery::from_static("/specimen.Counter/Watch"),
            ProstCodec::default(),
        )
        .await
        .expect("tonic server streaming")
        .into_inner();
    let first: specimen_grpc_counter::CounterReply = stream
        .message()
        .await
        .expect("first stream message")
        .expect("first stream item");
    let second: specimen_grpc_counter::CounterReply = stream
        .message()
        .await
        .expect("second stream message")
        .expect("second stream item");
    assert_eq!((first.value, second.value), (6, 7));
    assert!(
        stream.message().await.expect("stream eof").is_none(),
        "server stream must end cleanly"
    );

    client.ready().await.expect("tonic ready");
    let response: specimen_grpc_counter::CounterReply = client
        .client_streaming(
            Request::new(iter([
                specimen_grpc_counter::CounterRequest { delta: 10 },
                specimen_grpc_counter::CounterRequest { delta: 32 },
            ])),
            PathAndQuery::from_static("/specimen.Counter/Sum"),
            ProstCodec::default(),
        )
        .await
        .expect("tonic client streaming")
        .into_inner();
    assert_eq!(response.value, 42);

    server.shutdown().expect("shutdown specimen server");
}
