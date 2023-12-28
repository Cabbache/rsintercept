use clap::Parser;
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
use hyper::header::HeaderValue;
use hyper::header::CONNECTION;
use hyper::header::HOST;
use hyper::header::UPGRADE;
use hyper::upgrade::Upgraded;
use hyper::{Request, Response};

use std::future::Future;

use fastwebsockets::upgrade::upgrade;
use fastwebsockets::{handshake, FragmentCollectorRead, OpCode, WebSocket, WebSocketError};

use prometheus::{register_counter_vec, CounterVec, Encoder, Opts, TextEncoder};

use anyhow::Result;

use std::sync::Arc;

use std::net::Ipv4Addr;

/// CLI upstream http/ws proxy
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
	/// ip address of upstream
	#[arg(short, long)]
	upstream_ip: Ipv4Addr,

	/// upstream port
	#[arg(short, long)]
	upstream_port: u16,

	/// Host http header to use for upstream, it is left untouched by default
	#[arg(short, long)]
	upstream_host: Option<String>,

	/// proxy listen interface
	#[arg(short, long, default_value_t = Ipv4Addr::new(127, 0, 0, 1))]
	listen_interface: Ipv4Addr,

	/// proxy listen port
	#[arg(short, long)]
	listen_port: u16,

	/// prometheus listen interface
	#[arg(short, long, default_value_t = Ipv4Addr::new(127, 0, 0, 1))]
	prometheus_listen_interface: Ipv4Addr,

	/// prometheus listen port
	#[arg(short, long, default_value_t = 9100)]
	prometheus_listen_port: u16,
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

async fn connect_ws_upstream(
	addr: &SocketAddr,
	host: Option<HeaderValue>,
) -> Result<WebSocket<TokioIo<Upgraded>>> {
	let stream = TcpStream::connect(addr).await?;

	let mut req = Request::builder().method("GET").uri("/");

	if let Some(value) = host {
		println!("{:?}", value);
		req = req.header(HOST, value);
	}

	let req = req
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
	let args = Args::parse();

	let in_addr: SocketAddr = (args.listen_interface, args.listen_port).into();
	let out_addr: SocketAddr = (args.upstream_ip, args.upstream_port).into();
	let prometheus_addr: SocketAddr = (
		args.prometheus_listen_interface,
		args.prometheus_listen_port,
	)
		.into();

	let override_host = args.upstream_host.is_some();
	let upstream_host = args.upstream_host.unwrap_or("".to_string());

	let listener = TcpListener::bind(in_addr).await?;

	let metrics = Arc::new(Mutex::new(Metrics::new()));
	let prometheus_route = warp::path("metrics").map(move || {
		let encoder = TextEncoder::new();
		let metric_families = prometheus::gather();
		let mut buffer = Vec::new();
		encoder.encode(&metric_families, &mut buffer).unwrap();
		String::from_utf8(buffer).unwrap()
	});

	println!("Listening on {}", in_addr);
	println!("Proxying to {}", out_addr);

	tokio::task::spawn(async move {
		println!("Prometheus exporter listening on {}", prometheus_addr);
		warp::serve(prometheus_route).run(prometheus_addr).await;
	});

	loop {
		let (stream, _) = listener.accept().await?;
		let io = TokioIo::new(stream);

		let metrics_clone = metrics.clone();
		let upstream_host = upstream_host.clone();

		let service = service_fn(move |mut req| {
			let upstream_host = upstream_host.clone();

			//TODO use the in built function
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
						//let hostValue = req.headers().get(HOST);
						//println!("{}", hostValue);
						let connect_host = match override_host {
							true => Some(HeaderValue::from_str(&upstream_host).unwrap()),
							false => req.headers().get(HOST).cloned(),
						};

						let outgoing_ws =
							connect_ws_upstream(&out_addr, connect_host).await.unwrap();

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
					let client_stream = TcpStream::connect(out_addr).await.unwrap();
					let io = TokioIo::new(client_stream);
					let (mut sender, conn) =
						hyper::client::conn::http1::handshake(io).await.unwrap();
					tokio::task::spawn(async move {
						if let Err(err) = conn.await {
							println!("Connection failed: {:?}", err);
						}
					});

					if override_host {
						req.headers_mut()
							.insert(HOST, HeaderValue::from_str(&upstream_host).unwrap());
					}
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
