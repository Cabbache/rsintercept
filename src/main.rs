use clap::Parser;
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use std::net::ToSocketAddrs;

use warp::Filter;

use http_body_util::combinators::BoxBody;
use http_body_util::Empty;
use http_body_util::BodyExt;
use hyper::body::Bytes;
use hyper::header::HeaderValue;
use hyper::header::HOST;
use hyper::upgrade::Upgraded;
use hyper::StatusCode;
use hyper::body::Incoming;
use hyper::{Request, Response};

use std::future::Future;

use fastwebsockets::upgrade::{upgrade, is_upgrade_request};
use fastwebsockets::{handshake, FragmentCollectorRead, OpCode, WebSocket, WebSocketError};

use prometheus::{register_counter_vec, register_counter, CounterVec, Counter, Encoder, Opts, TextEncoder};

use anyhow::Result;

use std::sync::Arc;

/// CLI upstream http/ws proxy
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
	/// Upstream host address
	#[arg(short = 'h', long)]
	upstream_address: String,

	/// Bind address
	#[arg(short = 'l', long, default_value_t = String::from("127.0.0.1:8080"))]
	bind_address: String,

	/// Prometheus bind address
	#[arg(short = 'p', long, default_value_t = String::from("127.0.0.1:9100"))]
	prometheus_bind_address: String,

	/// Host http header to use for upstream, it is left untouched by default
	#[arg(long)]
	http_host: Option<String>,
}

struct Metrics {
	non_websocket: CounterVec,
	websocket_messages: CounterVec,
	websocket_sessions: CounterVec,

	non_websocket_fail: Counter,
	websocket_fail: Counter,
}

impl Metrics {
	fn new() -> Metrics {
		let namespace = "rs_intercept";

		let non_websocket = register_counter_vec!(
			Opts::new(
				"non_websocket",
				"Total HTTP requests forwarded that were not websocket",
			)
			.namespace(namespace),
			&["path"]
		)
		.unwrap();

		let websocket_messages = register_counter_vec!(
			Opts::new("websocket_messages", "Total WebSocket messages").namespace(namespace),
			&["path"]
		)
		.unwrap();

		let websocket_sessions = register_counter_vec!(
			Opts::new("websocket_sessions", "Total WebSocket sessions").namespace(namespace),
			&["path"]
		)
		.unwrap();

		let websocket_fail = register_counter!(
			Opts::new("websocket_fail", "total websocket requests that could not be upgraded").namespace(namespace)
		)
		.unwrap();

		let non_websocket_fail = register_counter!(
			Opts::new("non_websocket_fail", "total non websocket requests that upstream could not handle").namespace(namespace)
		)
		.unwrap();

		Metrics {
			non_websocket,
			websocket_messages,
			websocket_sessions,

			websocket_fail,
			non_websocket_fail,
		}
	}
}

async fn connect_ws_upstream(
	addr: &SocketAddr,
	req: Request<Incoming>,
) -> Result<WebSocket<TokioIo<Upgraded>>> {
	let stream = TcpStream::connect(addr).await?;
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args = Args::parse();

	let override_host = args.http_host.is_some();
	let http_host = args.http_host.unwrap_or("".to_string());

	let listener = TcpListener::bind(args.bind_address.clone()).await?;

	let out_addr = args
		.upstream_address.
		to_socket_addrs()
		.expect("Unable to parse upstream address")
		.next()
		.expect("Unable to parse upstream address");

	let metrics = Arc::new(Mutex::new(Metrics::new()));
	let prometheus_route = warp::path("metrics").map(move || {
		let encoder = TextEncoder::new();
		let metric_families = prometheus::gather();
		let mut buffer = Vec::new();
		encoder.encode(&metric_families, &mut buffer).unwrap();
		String::from_utf8(buffer).unwrap()
	});

	println!("Listening on {}", args.bind_address);
	println!("Proxying to {}", args.upstream_address);

	let prom_addr: SocketAddr = args
		.prometheus_bind_address
		.parse()
		.expect("Invalid bind address for prometheus");

	tokio::task::spawn(async move {
		println!("Prometheus exporter listening on {}", args.prometheus_bind_address.clone());
		warp::serve(prometheus_route).run(prom_addr).await;
	});

	loop {
		let (stream, _) = listener.accept().await?;
		let io = TokioIo::new(stream);

		let metrics_clone = metrics.clone();
		let http_host = http_host.clone();

		let service = service_fn(move |mut req| {
			let http_host = http_host.clone();
			let mtr = Arc::clone(&metrics_clone);

			let is_websocket = is_upgrade_request(&req);

			async move {
				{
					let mtr_lock = mtr.lock().await;
					match is_websocket {
						true => &mtr_lock.websocket_sessions,
						false => &mtr_lock.non_websocket,
					}.with_label_values(&[req.uri().path()]).inc();
				}

				if override_host {
					req.headers_mut()
						.insert(HOST, HeaderValue::from_str(&http_host).unwrap());
				}

				let handle_error = |e: anyhow::Error| {
					eprintln!("{}", e);
					Response::builder()
						.status(StatusCode::SERVICE_UNAVAILABLE)
						.body(empty())
						.unwrap()
				};

				if is_websocket {
					let (response, incoming_fut) = upgrade(&mut req).unwrap();
					let resp: Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> =
						Ok(response.map(|r| r.map_err(|e| match e {}).boxed()));

					let req_path = String::from(req.uri().path());

					let outgoing_ws = match connect_ws_upstream(&out_addr, req).await {
						Ok(ws) => ws,
						Err(e) => return Ok(handle_error(e.into()))
					};

					tokio::task::spawn(async move {
						let incoming_ws = incoming_fut.await.unwrap();

						let (incoming_rx, mut incoming_tx) =
							incoming_ws.split(|ws| tokio::io::split(ws));
						let (outgoing_rx, mut outgoing_tx) =
							outgoing_ws.split(|ws| tokio::io::split(ws));
						let mut incoming_rx = FragmentCollectorRead::new(incoming_rx);
						let mut outgoing_rx = FragmentCollectorRead::new(outgoing_rx);

						tokio::spawn(async move {
							while let Ok(frame) = incoming_rx
								.read_frame::<_, WebSocketError>(&mut move |_| async {
									println!("Connection closed");
									Ok::<(), WebSocketError>(())
								})
								.await
							{
								let decoded_payload = std::str::from_utf8(&frame.payload);
								let decoded_payload = match decoded_payload {
									Ok(p) => p,
									Err(_) => return,
								};
								println!("↑ {}", decoded_payload);
								match frame.opcode {
									OpCode::Close => break,
									OpCode::Text | OpCode::Binary => {
										{
											let mtr_lock = mtr.lock().await;
											mtr_lock
												.websocket_messages
												.with_label_values(&[&req_path])
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
									println!("Connection closed");
									Ok::<(), WebSocketError>(())
								})
								.await
							{
								let decoded_payload = std::str::from_utf8(&frame.payload);
								let decoded_payload = match decoded_payload {
									Ok(p) => p,
									Err(_) => return,
								};
								println!("↓ {}", decoded_payload);
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
					let client_stream = match TcpStream::connect(out_addr).await {
						Ok(stream) => stream,
						Err(e) => return Ok(handle_error(e.into()))
					};

					let io = TokioIo::new(client_stream);
					let (mut sender, conn) =
						hyper::client::conn::http1::handshake(io).await.unwrap();
					tokio::task::spawn(async move {
						if let Err(err) = conn.await {
							eprintln!("Connection failed: {:?}", err);
						}
					});

					let upstream_response = match sender.send_request(req).await {
						Ok(resp) => resp,
						Err(e) => return Ok(handle_error(e.into()))
					};
				
					let response: Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> =
						Ok(upstream_response.map(|p| p.boxed()));
					response
				}
			}
		});

		tokio::task::spawn(async move {
			if let Err(err) = http1::Builder::new()
				.serve_connection(io, service)
				.with_upgrades()
				.await
			{
				eprintln!("Failed to serve the connection: {:?}", err);
			}
		});
	}
}

//https://github.com/hyperium/hyper/blob/a9fa893f18c6409abae2e1dcbba0f4487df54d4f/examples/http_proxy.rs#L117
fn empty() -> BoxBody<Bytes, hyper::Error> {
	Empty::<Bytes>::new()
		.map_err(|never| match never {})
		.boxed()
}
