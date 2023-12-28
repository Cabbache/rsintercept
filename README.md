# rsintercept

An upstream http proxy that supports websockets.


## Usage

```console
CLI upstream http/ws proxy

Usage: rsintercept [OPTIONS] --upstream-ip <UPSTREAM_IP> --upstream-port <UPSTREAM_PORT> --listen-port <LISTEN_PORT>

Options:
  -u, --upstream-ip <UPSTREAM_IP>
          ip address of upstream
  -e, --upstream-port <UPSTREAM_PORT>
          upstream port
  -h, --upstream-host <UPSTREAM_HOST>
          Host http header to use for upstream, it is left untouched by default
  -l, --listen-interface <LISTEN_INTERFACE>
          proxy listen interface [default: 127.0.0.1]
  -p, --listen-port <LISTEN_PORT>
          proxy listen port
  -k, --prometheus-listen-interface <PROMETHEUS_LISTEN_INTERFACE>
          prometheus listen interface [default: 127.0.0.1]
  -m, --prometheus-listen-port <PROMETHEUS_LISTEN_PORT>
          prometheus listen port [default: 9100]
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
