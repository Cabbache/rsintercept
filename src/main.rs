use anyhow::Result;
use fastwebsockets::upgrade::upgrade;
use fastwebsockets::{handshake, FragmentCollectorRead, OpCode, WebSocket, WebSocketError};
use hyper::header::{CONNECTION, UPGRADE};
use hyper::service::{make_service_fn, service_fn};
use hyper::{upgrade::Upgraded, Body, Client, Request, Response, Server};
use serde_json::Value;
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use prometheus::{register_counter_vec, CounterVec, Encoder, Opts, TextEncoder};
use warp::Filter;

struct Config {
	domain: String,
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
			&["path", "method", "jsonvalid"]
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

async fn connect_ws_upstream(req: Request<Body>) -> Result<WebSocket<Upgraded>> {
	let stream = TcpStream::connect("172.67.171.72:80").await?;

	let req = Request::builder()
		.method("GET")
		.uri(req.uri().path())
		.header("Host", "ws.ifelse.io")
		.header(UPGRADE, "websocket")
		.header(CONNECTION, "upgrade")
		.header(
			"Sec-WebSocket-Key",
			fastwebsockets::handshake::generate_key(),
		)
		.header("Sec-WebSocket-Version", "13")
		.body(Body::empty())?;

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

async fn handle(
	req: Request<Body>,
	config: Arc<Config>,
	metrics: Arc<Mutex<Metrics>>,
) -> Result<Response<Body>, Infallible> {
	let is_websocket = req.headers().contains_key("Sec-WebSocket-Key");
	{
		let metrics = metrics.lock().await;
		if is_websocket {
			metrics
				.websocket_sessions_total
				.with_label_values(&[req.uri().path()])
				.inc();
		} else {
			metrics
				.non_websocket_total
				.with_label_values(&[req.uri().path()])
				.inc();
		}
	}

	if is_websocket {
		server_upgrade(req, metrics)
			.await
			.or(Ok(Response::default()))
	} else {
		relay_request(req, &config.domain)
			.await
			.or(Ok(Response::default()))
	}
}

async fn relay_request(
	req: Request<Body>,
	target_endpoint: &str,
) -> Result<Response<Body>, hyper::Error> {
	let client = Client::new();
	let req_headers = req.headers().clone();

	let mut forwarded_req = Request::builder()
		.method(req.method())
		.uri(format!(
			"http://{}{}",
			target_endpoint,
			req.uri().path_and_query().map_or("", |x| x.as_str())
		))
		.body(req.into_body())
		.expect("request builder");

	*forwarded_req.headers_mut() = req_headers;
	client.request(forwarded_req).await
}

async fn server_upgrade(
	mut req: Request<Body>,
	metrics: Arc<Mutex<Metrics>>,
) -> Result<Response<Body>> {
	let (response, incoming_fut) = upgrade(&mut req)?;
	let req_uri = req.uri().clone();
	let mut outgoing_ws = connect_ws_upstream(req).await.unwrap();

	tokio::spawn(async move {
		let mut incoming_ws = incoming_fut.await.unwrap();

		incoming_ws.set_writev(false);
		incoming_ws.set_auto_close(true);
		incoming_ws.set_auto_pong(true);

		outgoing_ws.set_writev(false);
		outgoing_ws.set_auto_close(true);
		outgoing_ws.set_auto_pong(true);

		let (incoming_rx, mut incoming_tx) = incoming_ws.split(|ws| tokio::io::split(ws));
		let (outgoing_rx, mut outgoing_tx) = outgoing_ws.split(|ws| tokio::io::split(ws));
		let mut incoming_rx = FragmentCollectorRead::new(incoming_rx);
		let mut outgoing_rx = FragmentCollectorRead::new(outgoing_rx);

		//from client to upstream
		tokio::spawn(async move {
			while let Ok(frame) = incoming_rx
				.read_frame::<_, WebSocketError>(&mut move |_| async {
					unreachable!();
				})
				.await
			{
				println!("Received a frame from user");
				match frame.opcode {
					OpCode::Close => break,
					OpCode::Text | OpCode::Binary => {
						let json_result: Result<Value, anyhow::Error> =
							std::str::from_utf8(&frame.payload)
								.map_err(anyhow::Error::new)
								.and_then(|text| {
									serde_json::from_str(text).map_err(anyhow::Error::new)
								});

						let mut labels: [String; 3] = [
							req_uri.path().to_string(),
							"".to_string(),
							"false".to_string(),
						];
						match json_result {
							Ok(value) => {
								labels[1] = value["method"].as_str().unwrap_or("").to_string();
								labels[2] = "true".to_string();
							}
							Err(_) => {
								println!("not json");
							}
						}

						{
							let metrics = metrics.lock().await;
							metrics
								.websocket_messages_total
								.with_label_values(
									&labels.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
								)
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

		//from upstream to client
		tokio::spawn(async move {
			while let Ok(frame) = outgoing_rx
				.read_frame::<_, WebSocketError>(&mut move |_| async {
					unreachable!();
				})
				.await
			{
				println!("Received a response from upstream");
				match frame.opcode {
					OpCode::Close => break,
					OpCode::Text | OpCode::Binary => {
						println!("{:?}", std::str::from_utf8(&frame.payload));
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

	Ok(response)
}

#[tokio::main]
async fn main() {
	let config = Arc::new(Config {
		domain: "example.com".to_string(),
	});

	let metrics = Arc::new(Mutex::new(Metrics::new()));
	let prometheus_route = warp::path("metrics").map(move || {
		let encoder = TextEncoder::new();
		let metric_families = prometheus::gather();
		let mut buffer = Vec::new();
		encoder.encode(&metric_families, &mut buffer).unwrap();
		Response::builder()
			.header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
			.body(buffer)
			.unwrap()
	});

	let addr = SocketAddr::from(([0, 0, 0, 0], 8765));
	let make_service = make_service_fn(move |_conn| {
		let config_clone = config.clone();
		let metrics_clone = metrics.clone();
		async move {
			Ok::<_, Infallible>(service_fn(move |req| {
				handle(req, config_clone.clone(), Arc::clone(&metrics_clone))
			}))
		}
	});

	let server = Server::bind(&addr).serve(make_service);
	println!("Server listening on ws://0.0.0.0:8765");

	let warp_server = warp::serve(prometheus_route).run(([0, 0, 0, 0], 9091));
	println!("Prometheus exporter listening on ws://0.0.0.0:9091");
	tokio::join!(server, warp_server);
}
