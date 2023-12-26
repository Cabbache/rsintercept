use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::header::CONNECTION;
use hyper::header::UPGRADE;
use hyper::upgrade::Upgraded;
use hyper::{Request, Response};

use std::future::Future;

use fastwebsockets::upgrade::upgrade;
use fastwebsockets::{handshake, FragmentCollectorRead, OpCode, WebSocket, WebSocketError};

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let in_addr: SocketAddr = ([127, 0, 0, 1], 80).into();
	let out_addr: SocketAddr = ([172, 67, 171, 72], 80).into();

	let out_addr_clone = out_addr.clone();

	let listener = TcpListener::bind(in_addr).await?;

	println!("Listening on http://{}", in_addr);
	println!("Proxying on http://{}", out_addr);

	loop {
		let (stream, _) = listener.accept().await?;
		let io = TokioIo::new(stream);

		let service = service_fn(move |mut req| {
			let addr = format!("{}", out_addr_clone);

			let is_websocket = req.headers().contains_key("Sec-WebSocket-Key");

			async move {
				if is_websocket {
					let (response, incoming_fut) = upgrade(&mut req).unwrap();
					let resp: Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> =
						Ok(response.map(|r| r.map_err(|e| match e {}).boxed()));

					tokio::task::spawn(async move {
						let incoming_ws = incoming_fut.await.unwrap();
						let outgoing_ws = connect().await.unwrap();

						let (incoming_rx, mut incoming_tx) =
							incoming_ws.split(|ws| tokio::io::split(ws));
						let (outgoing_rx, mut outgoing_tx) =
							outgoing_ws.split(|ws| tokio::io::split(ws));
						let mut incoming_rx = FragmentCollectorRead::new(incoming_rx);
						let mut outgoing_rx = FragmentCollectorRead::new(outgoing_rx);

						tokio::spawn(async move {
							while let Ok(frame) = incoming_rx
								.read_frame::<_, WebSocketError>(&mut move |_| async {
									unreachable!();
								})
								.await
							{
								println!("Received a frame (user)");
								println!("{:?}", std::str::from_utf8(&frame.payload));
								match frame.opcode {
									OpCode::Close => break,
									OpCode::Text | OpCode::Binary => {
										if let Err(e) = outgoing_tx.write_frame(frame).await {
											eprintln!("Error sending frame: {}", e);
											break;
										}
									}
									_ => {}
								}
							}
						});

						tokio::spawn(async move {
							while let Ok(frame) = outgoing_rx
								.read_frame::<_, WebSocketError>(&mut move |_| async {
									unreachable!();
								})
								.await
							{
								println!("Received a frame (upstream)");
								println!("{:?}", std::str::from_utf8(&frame.payload));
								match frame.opcode {
									OpCode::Close => break,
									OpCode::Text | OpCode::Binary => {
										if let Err(e) = incoming_tx.write_frame(frame).await {
											eprintln!("Error sending frame: {}", e);
											break;
										}
									}
									_ => {}
								}
							}
						});
					});

					resp
				} else {
					let client_stream = TcpStream::connect(addr).await.unwrap();
					let io = TokioIo::new(client_stream);
					let (mut sender, conn) =
						hyper::client::conn::http1::handshake(io).await.unwrap();
					tokio::task::spawn(async move {
						if let Err(err) = conn.await {
							println!("Connection failed: {:?}", err);
						}
					});

					let qq = sender.send_request(req).await.unwrap();
					println!("Served http request");
					let resp: Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> =
						Ok(qq.map(|p| p.boxed()));
					resp
				}
			}
		});

		tokio::task::spawn(async move {
			if let Err(err) = http1::Builder::new()
				.serve_connection(io, service)
				.with_upgrades()
				.await
			{
				println!("Failed to serve the connection: {:?}", err);
			}
		});
	}
}

async fn connect() -> Result<WebSocket<TokioIo<Upgraded>>> {
	let stream = TcpStream::connect("172.67.171.72:80").await?;

	let req = Request::builder()
		.method("GET")
		.uri("/")
		.header("Host", "ws.ifelse.io")
		.header(UPGRADE, "websocket")
		.header(CONNECTION, "upgrade")
		.header(
			"Sec-WebSocket-Key",
			fastwebsockets::handshake::generate_key(),
		)
		.header("Sec-WebSocket-Version", "13")
		.body(Empty::<Bytes>::new())?;

	println!("ok");
	let (ws, _) = handshake::client(&SpawnExecutor, req, stream).await?;
	Ok(ws)
}

struct SpawnExecutor;
impl<Fut> hyper::rt::Executor<Fut> for SpawnExecutor
where
	Fut: Future + Send + 'static,
	Fut::Output: Send + 'static,
{
	fn execute(&self, fut: Fut) {
		tokio::task::spawn(fut);
	}
}

/*
example usage: echo '127.0.0.1 ws.ifelse.io >> /etc/hosts'
python -m websockets ws://ws.ifelse.io
*/
