use clap::Parser;
use hyper::service::service_fn;
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_rustls::TlsAcceptor;

use std::net::ToSocketAddrs;
use std::time::{SystemTime, UNIX_EPOCH};
use chrono::prelude::*;
use std::{fs, io};

use warp::Filter;

use http_body_util::combinators::BoxBody;
use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::body::Incoming;
use hyper::header::HeaderValue;
use hyper::header::HOST;
use hyper::upgrade::Upgraded;
use hyper::StatusCode;
use hyper::{Request, Response};
use hyper_util::server::conn::auto::Builder;

use rustls::ServerConfig;

use pki_types::{CertificateDer, PrivateKeyDer};

use std::future::Future;

use fastwebsockets::upgrade::{is_upgrade_request, upgrade};
use fastwebsockets::{handshake, FragmentCollectorRead, Frame, OpCode, WebSocket, WebSocketError};

use prometheus::{
	register_counter, register_counter_vec, Counter, CounterVec, Encoder, Opts, TextEncoder,
};

use anyhow::Result;

use std::sync::Arc;

macro_rules! writelog {
	($($arg:tt)*) => {
		write_log(format!($($arg)*))
	};
}

/// CLI upstream http/ws proxy
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
	/// Upstream host address
	#[arg(short = 'u', long)]
	upstream_address: String,

	/// Bind address
	#[arg(short = 'l', long, default_value_t = String::from("127.0.0.1:8080"))]
	bind_address: String,

	/// Prometheus bind address
	#[arg(short = 'p', long, default_value_t = String::from("127.0.0.1:9100"))]
	prometheus_bind_address: String,

	/// Number of path segments to keep in metrics, 0 keeps all
	#[arg(long = "trim", default_value_t = 0)]
	trim_level: u16,

	/// Host http header to use for upstream, it is left untouched by default
	#[arg(long)]
	http_host: Option<String>,

	/// If specified, metrics will not include requests for which the response status is this code
	#[arg(short = 's', long)]
	ignore_status: Option<u16>,

	/// tls certificate path
	#[arg(long)]
	tls_cert: Option<String>,

	/// tls private key path
	#[arg(long)]
	tls_pkey: Option<String>,

	/// print websocket messages to stdout
	#[arg(short = 'v', long)]
	verbose: bool,

	/// print connecting IP to stdout
	#[arg(short = 'j', long)]
	print_ip: bool,
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

		let websocket_fail = register_counter!(Opts::new(
			"websocket_fail",
			"total websocket requests that could not be upgraded"
		)
		.namespace(namespace))
		.unwrap();

		let non_websocket_fail = register_counter!(Opts::new(
			"non_websocket_fail",
			"total non websocket requests that upstream could not handle"
		)
		.namespace(namespace))
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
	let trim_level: u16 = args.trim_level;
	let verbose = args.verbose;
	let show_ip = args.print_ip;

	assert!(
		args.tls_cert.is_some() == args.tls_pkey.is_some(),
		"tls cert and key should be either both provided or neither provided"
	);

	let mut tls_acceptor: Option<TlsAcceptor> = None;
	if args.tls_cert.is_some() {
		let tls_certs = load_certs(&args.tls_cert.unwrap())?;
		let tls_pkey = load_private_key(&args.tls_pkey.unwrap())?;

		let mut server_config = ServerConfig::builder()
			.with_no_client_auth()
			.with_single_cert(tls_certs, tls_pkey)
			.map_err(|e| error(e.to_string()))?;

		//server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec(), b"http/1.0".to_vec()];
		server_config.alpn_protocols = vec![b"http/1.1".to_vec(), b"http/1.0".to_vec()];
		tls_acceptor = Some(TlsAcceptor::from(Arc::new(server_config)));
	}

	let listener = TcpListener::bind(args.bind_address.clone()).await?;

	let ignore_status = args.ignore_status;

	let out_addr = args
		.upstream_address
		.to_socket_addrs()
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

	writelog!("Listening on {}", args.bind_address);
	writelog!("Proxying to {}", args.upstream_address);

	let prom_addr: SocketAddr = args
		.prometheus_bind_address
		.parse()
		.expect("Invalid bind address for prometheus");

	tokio::task::spawn(async move {
		writelog!(
			"Prometheus exporter listening on {}",
			args.prometheus_bind_address.clone()
		);
		warp::serve(prometheus_route).run(prom_addr).await;
	});

	loop {
		let (tcp_stream, _) = listener.accept().await?;

		if show_ip {
			match tcp_stream.peer_addr() {
				Ok(addr) => writelog!("Received connection from {}", addr),
				Err(msg) => writelog!("Could not show ip: {}", msg)
			}
		}

		let metrics_clone = metrics.clone();
		let http_host = http_host.clone();

		let service = service_fn(move |mut req| {
			let http_host = http_host.clone();
			let mtr = Arc::clone(&metrics_clone);

			let is_websocket = is_upgrade_request(&req);

			async move {
				if override_host {
					req.headers_mut()
						.insert(HOST, HeaderValue::from_str(&http_host).unwrap());
				}

				let handle_error = |e: anyhow::Error| {
					writelog!("{}", e);
					Response::builder()
						.status(StatusCode::SERVICE_UNAVAILABLE)
						.body(empty())
						.unwrap()
				};

				let req_path = String::from(req.uri().path());
				let req_path = match trim_level {
					0 => req_path,
					_ => format!(
						"/{}",
						req_path
							.split('/')
							.filter(|s| !s.is_empty())
							.collect::<Vec<&str>>()
							.into_iter()
							.take(trim_level.into())
							.collect::<Vec<&str>>()
							.join("/")
					),
				};

				if is_websocket {
					let (response, incoming_fut) = upgrade(&mut req).unwrap();
					let resp: Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> =
						Ok(response.map(|r| r.map_err(|e| match e {}).boxed()));

					let outgoing_ws = match connect_ws_upstream(&out_addr, req).await {
						Ok(ws) => ws,
						Err(e) => {
							mtr.lock().await.websocket_fail.inc();
							return Ok(handle_error(e.into()));
						}
					};

					tokio::task::spawn(async move {
						let incoming_ws = match incoming_fut.await {
							Ok(ws) => ws,
							Err(e) => {
								writelog!("error on ws: {}", e);
								return;
							}
						};

						mtr.lock()
							.await
							.websocket_sessions
							.with_label_values(&[&req_path])
							.inc();

						let (incoming_rx, incoming_tx) =
							incoming_ws.split(|ws| tokio::io::split(ws));
						let (outgoing_rx, outgoing_tx) =
							outgoing_ws.split(|ws| tokio::io::split(ws));
						let mut incoming_rx = FragmentCollectorRead::new(incoming_rx);
						let mut outgoing_rx = FragmentCollectorRead::new(outgoing_rx);

						let incoming_tx = Arc::new(Mutex::new(incoming_tx));
						let outgoing_tx = Arc::new(Mutex::new(outgoing_tx));

						let incoming_tx_clone1 = incoming_tx.clone();
						let outgoing_tx_clone1 = outgoing_tx.clone();

						let incoming_tx_clone2 = incoming_tx.clone();
						let outgoing_tx_clone2 = outgoing_tx.clone();

						tokio::spawn(async move {
							while let Ok(frame) = incoming_rx
								.read_frame::<_, WebSocketError>(&mut move |smth| async move {
									writelog!("err, from client: {:?}", smth.opcode);
									match smth.opcode {
										OpCode::Close => Err::<(), WebSocketError>(
											WebSocketError::ConnectionClosed,
										),
										_ => Ok::<(), WebSocketError>(()),
									}
								})
								.await
							{
								let decoded_payload = std::str::from_utf8(&frame.payload);
								let decoded_payload = match decoded_payload {
									Ok(p) => p,
									Err(_) => return,
								};
								if verbose {
									writelog!("↑ {}", decoded_payload);
								}
								match frame.opcode {
									OpCode::Close => break,
									OpCode::Text | OpCode::Binary => {
										mtr.lock()
											.await
											.websocket_messages
											.with_label_values(&[&req_path])
											.inc();
										if let Err(e) =
											outgoing_tx_clone1.lock().await.write_frame(frame).await
										{
											writelog!("Error sending frame: {}", e);
											break;
										}
									}
									_ => {}
								}
							}
							outgoing_tx_clone1
								.lock()
								.await
								.write_frame(Frame::close(1000, b""))
								.await
								.unwrap();
							incoming_tx_clone2
								.lock()
								.await
								.write_frame(Frame::close(1000, b""))
								.await
								.unwrap();
							writelog!("Closed client connection");
						});

						tokio::spawn(async move {
							while let Ok(frame) = outgoing_rx
								.read_frame::<_, WebSocketError>(&mut move |smth| async move {
									writelog!("err, from server {:?}", smth.opcode);
									match smth.opcode {
										OpCode::Close => Err::<(), WebSocketError>(
											WebSocketError::ConnectionClosed,
										),
										_ => Ok::<(), WebSocketError>(()),
									}
								})
								.await
							{
								let decoded_payload = std::str::from_utf8(&frame.payload);
								let decoded_payload = match decoded_payload {
									Ok(p) => p,
									Err(_) => return,
								};
								if verbose {
									writelog!("↓ {}", decoded_payload);
								}
								match frame.opcode {
									OpCode::Close => break,
									OpCode::Text | OpCode::Binary => {
										if let Err(e) =
											incoming_tx_clone1.lock().await.write_frame(frame).await
										{
											writelog!("Error sending frame: {}", e);
											break;
										}
									}
									_ => {}
								}
							}
							let _ = outgoing_tx_clone2
								.lock()
								.await
								.write_frame(Frame::close(1000, b""))
								.await;
							let _ = incoming_tx_clone1
								.lock()
								.await
								.write_frame(Frame::close(1000, b""))
								.await;
							writelog!("Closed server connection");
						});
					});

					resp
				} else {
					let client_stream = match TcpStream::connect(out_addr).await {
						Ok(stream) => stream,
						Err(e) => {
							mtr.lock().await.non_websocket_fail.inc();
							return Ok(handle_error(e.into()));
						}
					};

					let io = TokioIo::new(client_stream);
					let (mut sender, conn) =
						hyper::client::conn::http1::handshake(io).await.unwrap();
					tokio::task::spawn(async move {
						if let Err(err) = conn.await {
							writelog!("Connection failed: {:?}", err);
						}
					});

					let upstream_response = match sender.send_request(req).await {
						Ok(resp) => resp,
						Err(e) => {
							mtr.lock().await.non_websocket_fail.inc();
							return Ok(handle_error(e.into()));
						}
					};

					match ignore_status {
						Some(status) if status == upstream_response.status() => {}
						_ => {
							mtr.lock()
								.await
								.non_websocket
								.with_label_values(&[&req_path])
								.inc();
						}
					}

					let response: Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> =
						Ok(upstream_response.map(|p| p.boxed()));
					response
				}
			}
		});

		let tls_acceptor = tls_acceptor.clone();
		match tls_acceptor {
			Some(acceptor) => tokio::task::spawn(async move {
				let stream = match acceptor.accept(tcp_stream).await {
					Ok(stream) => stream,
					Err(e) => {
						writelog!("error on tls: {}", e);
						return;
					}
				};
				if let Err(err) = Builder::new(TokioExecutor::new())
					.serve_connection_with_upgrades(TokioIo::new(stream), service)
					.await
				{
					writelog!("Failed to serve the connection: {:?}", err);
				}
			}),
			_ => tokio::task::spawn(async move {
				if let Err(err) = Builder::new(TokioExecutor::new())
					.serve_connection_with_upgrades(TokioIo::new(tcp_stream), service)
					.await
				{
					writelog!("Failed to serve the connection: {:?}", err);
				}
			}),
		};
	}
}

//https://github.com/hyperium/hyper/blob/a9fa893f18c6409abae2e1dcbba0f4487df54d4f/examples/http_proxy.rs#L117
fn empty() -> BoxBody<Bytes, hyper::Error> {
	Empty::<Bytes>::new()
		.map_err(|never| match never {})
		.boxed()
}

//https://github.com/rustls/hyper-rustls/blob/4030f86a95d7c3d2ebe87c7da86b5a8bde2857a1/examples/server.rs#L114
fn load_certs(filename: &str) -> io::Result<Vec<CertificateDer<'static>>> {
	// Open certificate file.
	let certfile = fs::File::open(filename)
		.map_err(|e| error(format!("failed to open {}: {}", filename, e)))?;
	let mut reader = io::BufReader::new(certfile);

	// Load and return certificate.
	rustls_pemfile::certs(&mut reader).collect()
}

// Load private key from file.
fn load_private_key(filename: &str) -> io::Result<PrivateKeyDer<'static>> {
	// Open keyfile.
	let keyfile = fs::File::open(filename)
		.map_err(|e| error(format!("failed to open {}: {}", filename, e)))?;
	let mut reader = io::BufReader::new(keyfile);

	// Load and return a single private key.
	rustls_pemfile::private_key(&mut reader).map(|key| key.unwrap())
}

fn error(err: String) -> io::Error {
	io::Error::new(io::ErrorKind::Other, err)
}

fn write_log(msg: String) {
	let start = SystemTime::now();
	let timestamp = start
		.duration_since(UNIX_EPOCH)
		.expect("Time went backwards")
		.as_secs();
	let datetime = DateTime::from_timestamp(timestamp as i64, 0).unwrap();
	let newdate = datetime.format("%Y-%m-%d %H:%M:%S");
	println!("[{}]: {}", newdate, msg);
}
