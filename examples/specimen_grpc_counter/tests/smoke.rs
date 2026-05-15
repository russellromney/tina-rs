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
    let mut stream = client
        .server_streaming(
            Request::new(specimen_grpc_counter::CounterRequest { delta: 40 }),
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
    assert_eq!((first.value, second.value), (41, 42));
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

    client.ready().await.expect("tonic ready");
    let mut stream = client
        .streaming(
            Request::new(iter([
                specimen_grpc_counter::CounterRequest { delta: 3 },
                specimen_grpc_counter::CounterRequest { delta: 4 },
            ])),
            PathAndQuery::from_static("/specimen.Counter/Chat"),
            ProstCodec::default(),
        )
        .await
        .expect("tonic streaming")
        .into_inner();
    let first: specimen_grpc_counter::CounterReply = stream
        .message()
        .await
        .expect("streaming first")
        .expect("streaming first item");
    let second: specimen_grpc_counter::CounterReply = stream
        .message()
        .await
        .expect("streaming second")
        .expect("streaming second item");
    assert_eq!((first.value, second.value), (3, 4));
    assert!(
        stream.message().await.expect("streaming eof").is_none(),
        "streaming RPC must end cleanly"
    );

    let addr = server.addr;
    let first = async move {
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect first tonic h2c");
        let mut client = Grpc::new(channel);
        client.ready().await.expect("first tonic ready");
        let mut stream: tonic::Streaming<specimen_grpc_counter::CounterReply> = client
            .streaming(
                Request::new(iter([specimen_grpc_counter::CounterRequest { delta: 11 }])),
                PathAndQuery::from_static("/specimen.Counter/Chat"),
                ProstCodec::default(),
            )
            .await
            .expect("first tonic streaming")
            .into_inner();
        stream
            .message()
            .await
            .expect("first streaming message")
            .expect("first streaming item")
            .value
    };
    let second = async move {
        let channel = Endpoint::from_shared(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect second tonic h2c");
        let mut client = Grpc::new(channel);
        client.ready().await.expect("second tonic ready");
        let mut stream: tonic::Streaming<specimen_grpc_counter::CounterReply> = client
            .streaming(
                Request::new(iter([specimen_grpc_counter::CounterRequest { delta: 12 }])),
                PathAndQuery::from_static("/specimen.Counter/Chat"),
                ProstCodec::default(),
            )
            .await
            .expect("second tonic streaming")
            .into_inner();
        stream
            .message()
            .await
            .expect("second streaming message")
            .expect("second streaming item")
            .value
    };
    assert_eq!(tokio::join!(first, second), (11, 12));

    server.shutdown().expect("shutdown specimen server");
}
