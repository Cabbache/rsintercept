use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use warp::Filter;

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

use prometheus::{register_counter_vec, CounterVec, Encoder, Opts, TextEncoder};

use anyhow::Result;

use std::sync::Arc;

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

struct Metrics {
	non_websocket_total: CounterVec,
	websocket_messages_total: CounterVec,
	websocket_sessions_total: CounterVec,
}

impl Metrics {
	fn new() -> Metrics {
		let namespace = "rs_intercept";

		let non_websocket_total = register_counter_vec!(
			Opts::new(
				"non_websocket_total",
				"Total HTTP requests forwarded that were not websocket",
			)
			.namespace(namespace),
			&["path"]
		)
		.unwrap();

		let websocket_messages_total = register_counter_vec!(
			Opts::new("websocket_messages_total", "Total WebSocket messages").namespace(namespace),
			&["path"]
		)
		.unwrap();

		let websocket_sessions_total = register_counter_vec!(
			Opts::new("websocket_sessions_total", "Total WebSocket sessions").namespace(namespace),
			&["path"]
		)
		.unwrap();

		Metrics {
			non_websocket_total,
			websocket_messages_total,
			websocket_sessions_total,
		}
	}
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let in_addr: SocketAddr = ([127, 0, 0, 1], 80).into();
	let out_addr: SocketAddr = ([172, 67, 171, 72], 80).into();

	let out_addr_clone = out_addr.clone();

	let listener = TcpListener::bind(in_addr).await?;

	let metrics = Arc::new(Mutex::new(Metrics::new()));
	let prometheus_route = warp::path("metrics").map(move || {
		let encoder = TextEncoder::new();
		let metric_families = prometheus::gather();
		let mut buffer = Vec::new();
		encoder.encode(&metric_families, &mut buffer).unwrap();
		String::from_utf8(buffer).unwrap()
	});

	println!("Listening on http://{}", in_addr);
	println!("Proxying on http://{}", out_addr);

	tokio::task::spawn(async move {
		println!("Serving prometheus on 0.0.0.0:9091");
		warp::serve(prometheus_route)
			.run(([0, 0, 0, 0], 9091))
			.await;
	});

	loop {
		let (stream, _) = listener.accept().await?;
		let io = TokioIo::new(stream);

		let metrics_clone = metrics.clone();

		let service = service_fn(move |mut req| {
			let addr = format!("{}", out_addr_clone);

			let is_websocket = req.headers().contains_key("Sec-WebSocket-Key");

			let mtr = Arc::clone(&metrics_clone);

			async move {
				{
					let mtr_lock = mtr.lock().await;
					if is_websocket {
						mtr_lock
							.websocket_sessions_total
							.with_label_values(&[req.uri().path()])
							.inc();
					} else {
						mtr_lock
							.non_websocket_total
							.with_label_values(&[req.uri().path()])
							.inc();
					}
				}

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
										{
											let mtr_lock = mtr.lock().await;
											mtr_lock
												.websocket_messages_total
												.with_label_values(&[req.uri().path()])
												.inc();
										}
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

/*
example usage: echo '127.0.0.1 ws.ifelse.io' >> /etc/hosts
python -m websockets ws://ws.ifelse.io
*/
