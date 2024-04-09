# rsintercept

An upstream http proxy that supports websockets.


## Usage

```console
CLI upstream http/ws proxy

Usage: rsintercept [OPTIONS] --upstream-address <UPSTREAM_ADDRESS>

Options:
  -u, --upstream-address <UPSTREAM_ADDRESS>
          Upstream host address
  -l, --bind-address <BIND_ADDRESS>
          Bind address [default: 127.0.0.1:8080]
  -p, --prometheus-bind-address <PROMETHEUS_BIND_ADDRESS>
          Prometheus bind address [default: 127.0.0.1:9100]
      --trim <TRIM_LEVEL>
          Number of path segments to keep in metrics, 0 keeps all [default: 0]
      --http-host <HTTP_HOST>
          Host http header to use for upstream, it is left untouched by default
  -s, --ignore-status <IGNORE_STATUS>
          If specified, metrics will not include requests for which the response status is this code
      --tls-cert <TLS_CERT>
          tls certificate path
      --tls-pkey <TLS_PKEY>
          tls private key path
  -h, --help
          Print help
  -V, --version
          Print version
```

## Metrics

The proxy exposes prometheus metrics that include the number of websocket messages received by the client, labled by the request path. For example:

```console
# HELP rs_intercept_non_websocket_total Total HTTP requests forwarded that were not websocket
# TYPE rs_intercept_non_websocket_total counter
rs_intercept_non_websocket_total{path="/abc"} 2
rs_intercept_non_websocket_total{path="/def"} 1
rs_intercept_non_websocket_total{path="/hello"} 1
rs_intercept_non_websocket_total{path="/some-path"} 1
# HELP rs_intercept_websocket_messages_total Total WebSocket messages
# TYPE rs_intercept_websocket_messages_total counter
rs_intercept_websocket_messages_total{path="/"} 14
rs_intercept_websocket_messages_total{path="/abc"} 8
rs_intercept_websocket_messages_total{path="/abcdef"} 15
rs_intercept_websocket_messages_total{path="/some-path"} 19
# HELP rs_intercept_websocket_sessions_total Total WebSocket sessions
# TYPE rs_intercept_websocket_sessions_total counter
rs_intercept_websocket_sessions_total{path="/"} 1
rs_intercept_websocket_sessions_total{path="/abc"} 1
rs_intercept_websocket_sessions_total{path="/abcdef"} 2
rs_intercept_websocket_sessions_total{path="/some-path"} 1
```

## Example usage
```console
cargo run --release -- -u ws.ifelse.io:80 --http-host ws.ifelse.io
```
