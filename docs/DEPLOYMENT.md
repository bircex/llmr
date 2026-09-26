# Running llmr

llmr is one process with one configuration file. There is no database and no state on disk:
what it remembers (which routes are failing) lives in memory and is rebuilt after a restart.

## With Docker Compose

```sh
cp llmr.example.toml llmr.toml     # the routes
cp .env.example .env               # LLMR_API_KEYS and the provider keys
docker compose up -d
docker compose logs -f llmr
```

[`docker-compose.yml`](../docker-compose.yml) runs the image read only, with
`no-new-privileges`, publishes port 8080, and creates a network named `llmr`. Another compose
project reaches the gateway at `http://llmr:8080/v1` by joining that network:

```yaml
services:
  app:
    environment:
      OPENAI_BASE_URL: http://llmr:8080/v1
      OPENAI_API_KEY: ${LLMR_KEY}
    networks: [llmr]

networks:
  llmr:
    external: true
```

When only other containers call it, drop the `ports:` mapping so it is not reachable from
outside the host at all.

## With `docker run`

```sh
docker run -d --name llmr --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v "$PWD/llmr.toml:/etc/llmr/llmr.toml:ro" \
  --env-file .env \
  --read-only \
  ghcr.io/bircex/llmr:latest
```

## The image

- `ghcr.io/bircex/llmr`, for `linux/amd64` and `linux/arm64`.
- Tags: `X.Y.Z` and `X.Y` from release tags, `latest` from `main`, `sha-<commit>` for every
  build. **Pin `X.Y.Z` in production**; `latest` moves with every merge.
- Distroless, no shell, runs as a non root user. The configuration is read from
  `/etc/llmr/llmr.toml`; the image carries `llmr.example.toml` there, which will not start
  without vendor keys, so mount your own.
- `HEALTHCHECK` runs `llmr healthcheck`, which asks `/healthz` over loopback every 15
  seconds.
- `docker stop` sends SIGTERM: the gateway stops accepting connections and finishes the
  requests in flight, streams included, before it exits.

Other commands the image runs:

```sh
docker run --rm ghcr.io/bircex/llmr:latest --version
docker run --rm -v "$PWD/llmr.toml:/etc/llmr/llmr.toml:ro" --env-file .env ghcr.io/bircex/llmr:latest check
```

## Without Docker

Every release has binaries for Linux (x86_64, arm64) and macOS (arm64) on the
[releases page](https://github.com/bircex/llmr/releases), each with a `.sha256` beside it.

```sh
tar -xzf llmr-v0.3.0-x86_64-unknown-linux-gnu.tar.gz
sha256sum -c llmr-v0.3.0-x86_64-unknown-linux-gnu.tar.gz.sha256
./llmr-v0.3.0-x86_64-unknown-linux-gnu/llmr --config llmr.toml
```

A systemd unit, with the keys in an environment file only root can read:

```ini
[Unit]
Description=llmr gateway
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/llmr serve --config /etc/llmr/llmr.toml
EnvironmentFile=/etc/llmr/llmr.env
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

Or from source: `cargo run --release --features server -- --config llmr.toml`.

## In front of it: TLS

The gateway serves plain HTTP and does not terminate TLS. Client keys travel in a header, so
anything that crosses a network you do not control needs TLS in front. With Caddy, which
fetches a certificate on its own:

```
llm.example.com {
    reverse_proxy llmr:8080 {
        flush_interval -1
    }
}
```

`flush_interval -1` passes streamed chunks through as they arrive. The gateway also sends
`x-accel-buffering: no`, which nginx honours, and a keep-alive comment every 15 seconds on a
quiet stream; still, set the proxy's read timeout above your longest expected reply
(`proxy_read_timeout` in nginx).

## Changing the configuration

The file is read at startup. After editing it, run `llmr check`, then restart the gateway.
A restart finishes requests in flight first, and forgets which routes were resting.

## Keys

**Client keys** live in `LLMR_API_KEYS`, comma separated. To rotate one: add the new key,
restart, move clients to it, remove the old key, restart.

**Provider keys** are read once at startup from the variables the configuration names. A
missing or empty one stops startup with a message naming the variable. A key the vendor
rejects is reported by the startup preflight, and the route is rested rather than tried
first in every request.

## Logs

One line per request, on stdout (shown here without the timestamp and target):

```
INFO answered model=default route=anthropic/claude-sonnet-5 attempts=1 fell_through=0 input_tokens=812 output_tokens=64 stop="end_turn"
WARN fell through model=default route=openai/gpt-5.1 why="rate limited, retry after 2000ms (attempt 1 of 2, waiting 2000ms)"
```

`LLMR_LOG_FORMAT=json` writes the same as one JSON object per line, for a log collector.
`RUST_LOG` sets the filter (`info` by default).

Prompts, replies and keys are never logged. What is worth alerting on is a `fell through`
warning on a request that still succeeded: a provider degrading while nothing fails.

## What to watch

- `GET /llmr/routes` lists routes a breaker is currently skipping and for how long.
- `llmr check` in a scheduled job catches a key that expired or a model a vendor retired,
  without spending anything.
- Startup logs name every route that can never be chosen (`the provider does not know this
  model`); fix those before anything else.
