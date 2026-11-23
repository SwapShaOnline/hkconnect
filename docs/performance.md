# Performance

Measured 2026-11-23 on a 2024 Hyundai Creta (Bluelink India, older non-CCS2
protocol) in India, over home broadband, on an Apple Silicon MacBook
(8 GB). Hyundai's servers dominate every number; expect variation by time of
day and network.

Reproduce the API numbers with `hkc bench` (read-only calls only; it never
wakes the car):

```sh
hkc bench --runs 5
```

## API calls (hkc bench, 5 runs each, 1 s apart)

| Operation | Median | p95 / max |
|---|---|---|
| Fresh login (4 requests) | 279 ms | — (1 run) |
| List vehicles | 86 ms | 111 ms |
| Status (cached) | 114 ms | 218 ms |
| Location | 72 ms | 77 ms |
| Capabilities | 106 ms | 148 ms |
| Maintenance | 96 ms | 116 ms |
| Trips: month | 98 ms | 138 ms |
| Trips: day | 333 ms | 387 ms |

The client keeps one HTTP/2 + TLS connection open and reuses the saved
session, so after the first call each read is a single request.

## Remote commands (the car's own response time)

| Command | Accepted → car confirmed |
|---|---|
| Lock | 30 s (one run, 2026-11-23) |

Commands are dominated by the car's cellular modem waking up and replying;
the client's own overhead is a few hundred milliseconds. Other commands have
not been measured on a real car yet.

## Local overhead

| Measurement | Result |
|---|---|
| `hkc --version` process start (release build) | 4 ms median, 21 ms max |
| `hkc` release binary size | ~10 MB |
| `hkc-mcp` start + MCP initialize (stdio) | 29 ms |
| `hkc-mcp` `tools/list` | 1.7 ms |
| `hkc-mcp` `get_status` (warm) | 95 ms median — same as a direct call: MCP adds no measurable latency |
| `hkc-mcp` memory (resident) | ~17 MB |
| `hkc-mcp` release binary size | ~16 MB |

## Comparison

For context, the Python reference library measured on the same account and
day: ~3.5 s for login (it opens new connections per step, including a ~1.8 s
device registration) and 0.3–0.6 s per read.
