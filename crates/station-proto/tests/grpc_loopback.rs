// SPDX-License-Identifier: MIT
//! The generated tonic client and server of `StationGateway` work end to end
//! (plaintext loopback; TLS is the hub's and stationd's business).

use std::pin::Pin;

use station_proto::v1::station_gateway_client::StationGatewayClient;
use station_proto::v1::station_gateway_server::{StationGateway, StationGatewayServer};
use station_proto::v1::{self, hub_message, station_message::Body};
use station_proto::validate::{Session, Verdict, negotiate};
use tokio_stream::{Stream, StreamExt, wrappers::TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};

struct Gateway;

type HubStream = Pin<Box<dyn Stream<Item = Result<v1::HubMessage, Status>> + Send>>;

#[tonic::async_trait]
impl StationGateway for Gateway {
    type ConnectStream = HubStream;

    async fn connect(
        &self,
        request: Request<Streaming<v1::StationMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let mut inbound = request.into_inner();
        let first = inbound
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("no Hello"))?;
        let now = std::time::Instant::now();
        let mut session = Session::new(now);
        let reply = match (session.check(&first, now), &first.body) {
            (Verdict::Accept, Some(Body::Hello(hello))) => {
                hub_message::Body::Welcome(v1::Welcome {
                    protocol_version: negotiate(hello).ok(),
                    connection_id: "c1".into(),
                    status_interval_ms: 10_000,
                    max_message_bytes: 65_536,
                    hub_time_unix_ms: 0,
                })
            }
            (verdict, _) => hub_message::Body::Goodbye(v1::Goodbye {
                reason: match verdict {
                    Verdict::Violation(k) => k.goodbye_reason() as i32,
                    _ => v1::goodbye::Reason::ProtocolViolation as i32,
                },
                ..Default::default()
            }),
        };
        let out = tokio_stream::once(Ok(v1::HubMessage { body: Some(reply) }));
        Ok(Response::new(Box::pin(out)))
    }
}

async fn exchange(first: v1::StationMessage) -> hub_message::Body {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(StationGatewayServer::new(Gateway))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut client = StationGatewayClient::new(channel);
    let response = client
        .connect(tokio_stream::iter(vec![first]))
        .await
        .expect("rpc");
    let mut inbound = response.into_inner();
    let msg = inbound.next().await.expect("one message").expect("ok");
    msg.body.expect("body")
}

#[tokio::test]
async fn hello_gets_welcome() {
    let hello = v1::StationMessage {
        body: Some(Body::Hello(v1::Hello {
            protocol_version: Some(station_proto::PROTOCOL_VERSION),
            daemon_version: "0.1.0".into(),
            backend: Some(v1::BackendInfo {
                kind: v1::BackendKind::Ka9qRadio as i32,
                version: String::new(),
            }),
            platform: "linux-x86_64".into(),
            boot_id: 7,
        })),
    };
    let hub_message::Body::Welcome(w) = exchange(hello).await else {
        panic!("expected Welcome");
    };
    assert_eq!(w.protocol_version, Some(station_proto::PROTOCOL_VERSION));
}

#[tokio::test]
async fn malformed_first_message_gets_protocol_violation() {
    let audio = v1::StationMessage {
        body: Some(Body::Audio(v1::AudioFrame::default())),
    };
    let hub_message::Body::Goodbye(g) = exchange(audio).await else {
        panic!("expected Goodbye");
    };
    assert_eq!(g.reason, v1::goodbye::Reason::ProtocolViolation as i32);
}
