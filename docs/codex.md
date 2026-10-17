# Codex bridge

The browser calls `/api/codex/*` on Red. Red forwards to the optional Rust
`codex-wrapper` service on cat7; that service owns `codex app-server` and its
stdio connection. Red does not launch Codex, maintain agent state, or poll it
in the background. The wrapper can be stopped between tmux sessions without
affecting Red's `/health` or any other API scope.

## Configuration and startup

`CODEX_WRAPPER_URL` defaults to `http://10.1.1.3:3000`. It must be an HTTP(S)
origin, with no credentials, path prefix, query, or fragment. Changes require a
Red restart. Red connects directly, ignoring ambient HTTP proxy settings, and
never follows upstream redirects.

The wrapper's default bind is loopback. To make it reachable from brew7, start
it on cat7 in tmux with `CODEX_WRAPPER_BIND=10.1.1.3:3000 cargo run --release`
from the `codex-wrapper` checkout. The wrapper must be reachable from Red over
the LAN. No new credentials or authentication mechanism are introduced here;
keep the existing trusted network and frontend access controls.

## Availability

`GET /api/codex/status` makes one fresh request to the wrapper's
`/api/codex/v2/status` endpoint. Its two-second total deadline includes connect,
response headers, response body, and parsing. There is no cached result or background probe. The
JSON body is limited to 16 KiB. The response is always HTTP 200, with:

```json
{
  "status": "ready",
  "wrapperRunning": true,
  "appServerRunning": true,
  "workspaceRoot": "/home/op/w"
}
```

| Condition | status | wrapperRunning | appServerRunning | reason |
| --- | --- | --- | --- | --- |
| Wrapper and App Server ready | ready | true | true | omitted |
| Wrapper reachable, App Server stopped | unavailable | true | false | app_server_stopped |
| Connection refused or other transport failure | inactive | false | false | unreachable |
| Deadline exceeded | inactive | false | false | timeout |
| Invalid, oversized, contradictory, truncated, or non-success response | unavailable | true | false | bad_response |

`workspaceRoot` is null unless a valid wrapper status was received. A timeout
means availability could not be established; it does not prove the process has
stopped. A successful wrapper `/health` alone is not evidence App Server is ready.

## HTTP forwarding

Every other method/path under `/api/codex` (including non-GET requests to
`/api/codex/status`) forwards its original method, encoded path, query and body.
Red preserves upstream status codes (including 4xx, 5xx, and redirects), duplicate
end-to-end headers, and streams response bodies, including SSE. Request bodies
are streamed too. The connect deadline is two seconds; the combined request
upload and response-header wait is limited to 30 seconds. Once response headers
arrive, there is no Red body deadline, allowing long-running streams.

Connection-nominated headers and standard hop-by-hop headers are removed in both
directions. Red regenerates Host for the fixed upstream and does not forward
HTTP trailers. The target cannot be selected through a request path or query;
dot-segment traversal, backslashes and nested percent encoding are rejected.
Normal encoded slashes and encoded query values are retained.

Transport failure before headers produces HTTP 503 with the same structured
`inactive` body and `unreachable` or `timeout` reason. A transport failure after
headers terminates the stream; it cannot replace the already-sent status.
WebSocket/Upgrade requests return HTTP 501 JSON `unsupported_upgrade`; a
bidirectional WebSocket bridge is not implemented. This bridge does not add
thread, turn, approval, or agent APIs that the wrapper does not yet expose.

The Codex tests use ephemeral local mock HTTP servers; no running wrapper,
Redis, Kubernetes, or Codex account is required.
