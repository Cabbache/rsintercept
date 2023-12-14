use fastwebsockets::upgrade::upgrade;
use fastwebsockets::{OpCode, FragmentCollectorRead, WebSocket, WebSocketError, handshake};
use hyper::{Body, Client, Request, Response, Server, upgrade::Upgraded};
use hyper::header::{UPGRADE,CONNECTION};
use hyper::service::{make_service_fn, service_fn};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpStream;
use std::future::Future;
use anyhow::Result;

struct Config {
	domain: String,
}

async fn connect() -> Result<WebSocket<Upgraded>> {
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
    .body(Body::empty())?;

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

async fn handle(req: Request<Body>, config: Arc<Config>) -> Result<Response<Body>, Infallible> {
    if is_websocket_request(&req) {
        server_upgrade(req).await.or(Ok(Response::default()))
    } else {
        relay_request(req, &config.domain).await.or(Ok(Response::default()))
    }
}

fn is_websocket_request(req: &Request<Body>) -> bool {
	req.headers().contains_key("Sec-WebSocket-Key")
}

async fn relay_request(req: Request<Body>, target_endpoint: &str) -> Result<Response<Body>, hyper::Error> {
	let client = Client::new();
	let req_headers = req.headers().clone();

	let mut forwarded_req = Request::builder()
		.method(req.method())
		.uri(format!("http://{}{}", target_endpoint, req.uri().path_and_query().map_or("", |x| x.as_str())))
		.body(req.into_body())
		.expect("request builder");

	*forwarded_req.headers_mut() = req_headers;
	client.request(forwarded_req).await
}

async fn server_upgrade(mut req: Request<Body>) -> Result<Response<Body>> {
	let (response, incoming_fut) = upgrade(&mut req)?;

	let mut outgoing_ws = connect().await.unwrap();

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

		tokio::spawn(async move {
			while let Ok(frame) = incoming_rx.read_frame::<_, WebSocketError>(
			&mut move |_| async {
        unreachable!();
      }).await {
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
			while let Ok(frame) = outgoing_rx.read_frame::<_, WebSocketError>(
			&mut move |_| async {
        unreachable!();
      }).await {
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

	Ok(response)
}

#[tokio::main]
async fn main() {
	let config = Arc::new(Config {
		domain: "example.com".to_string(),
	});

	let addr = SocketAddr::from(([0, 0, 0, 0], 8765));
	let make_service = make_service_fn(move |_conn| {
		let config_clone = config.clone();
		async move {
			Ok::<_, Infallible>(service_fn(move |req| handle(req, config_clone.clone())))
		}
	});

	let server = Server::bind(&addr).serve(make_service);
	println!("Server listening on ws://0.0.0.0:8765");

	if let Err(e) = server.await {
		eprintln!("Server error: {}", e);
	}
}
