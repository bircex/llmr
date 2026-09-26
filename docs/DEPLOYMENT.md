# Running llmr

llmr runs only as its Docker image, `ghcr.io/bircex/llmr`. It needs two things from you: a
master key in its environment, and a volume for its database. Everything else (providers,
models, route sets) is set afterwards through the [management API](MANAGEMENT.md).

## The master key

Every stored credential is encrypted with `LLMR_MASTER_KEY`. Make one:

```sh
docker run --rm ghcr.io/bircex/llmr keygen
```

- **Keep a copy outside the host**, in whatever holds your other secrets. The database alone
  opens nothing; without the key, the stored credentials are gone and have to be entered
  again.
- **llmr refuses to start with a different key** than the one its database was created with,
  and says so, rather than starting with every provider broken.
- Never put the key on the same volume as the database: that would defeat the encryption.

## With Docker Compose

```sh
cp .env.example .env        # set LLMR_MASTER_KEY, and LLMR_TOKEN if you want one
docker compose up -d
docker compose logs -f llmr
```

[`docker-compose.yml`](../docker-compose.yml) runs the image read only, with
`no-new-privileges`, keeps the database in the named volume `llmr-data`, publishes the port on
loopback only, and creates a network named `llmr`. Another compose project reaches llmr at
`http://llmr:8080` by joining that network:

```yaml
services:
  app:
    environment:
      OPENAI_BASE_URL: http://llmr:8080/v1
      OPENAI_API_KEY: ${LLMR_TOKEN}
    networks: [llmr]

networks:
  llmr:
    external: true
```

## With `docker run`

```sh
docker run -d --name llmr --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v llmr-data:/var/lib/llmr \
  -e LLMR_MASTER_KEY="..." \
  -e LLMR_TOKEN="..." \
  --read-only \
  ghcr.io/bircex/llmr:latest
```

**Use a named volume.** The image's `/var/lib/llmr` belongs to its non root user, and a named
volume starts with that ownership. A bind mount (`-v /srv/llmr:/var/lib/llmr`) must be
writable by uid `65532`: `chown 65532:65532 /srv/llmr`.

## Environment

| Variable | Default | |
|---|---|---|
| `LLMR_MASTER_KEY` | required | Seals stored credentials. See above |
| `LLMR_TOKEN` | unset | Tokens callers must present, comma separated. Unset, nothing is checked |
| `LLMR_LISTEN` | `0.0.0.0:8080` | Address and port inside the container |
| `LLMR_DATA_DIR` | `/var/lib/llmr` | Where the database lives |
| `LLMR_MAX_BODY_MB` | `32` | Largest request body; images arrive inline |
| `RUST_LOG` | `info` | Log filter |
| `LLMR_LOG_FORMAT` | text | `json` for one object per line |

## Who can reach it

The management API can add providers, replace credentials and send traffic anywhere, so
whoever can reach the port can do all of that. Pick one:

- **A private network.** The panel and your apps share a Docker network or a VPC with llmr,
  and the port is not published beyond it. `LLMR_TOKEN` can stay unset.
- **A token.** Set `LLMR_TOKEN`; the panel and your apps present it. Rotate it by setting two
  comma separated tokens, moving callers to the new one, then removing the old one.

Either way, anything that crosses a network you do not control needs TLS in front: llmr
serves plain HTTP. With Caddy, which fetches a certificate on its own:

```
llm.example.com {
    reverse_proxy llmr:8080 {
        flush_interval -1
    }
}
```

`flush_interval -1` passes streamed chunks through as they arrive. llmr also sends
`x-accel-buffering: no`, which nginx honours, and a keep-alive comment every 15 seconds on a
quiet stream; set the proxy's read timeout above your longest expected reply.

## The image

- `linux/amd64` and `linux/arm64`.
- Tags: `X.Y.Z` and `X.Y` from releases, `latest` from `main`, `sha-<commit>` for every
  build. **Pin `X.Y.Z` in production**; `latest` moves with every merge.
- Distroless: no shell, no package manager, a non root user.
- `HEALTHCHECK` runs `llmr healthcheck`, which asks `/healthz` over loopback every 15 seconds.
- `docker stop` sends SIGTERM: llmr stops accepting connections and finishes requests in
  flight, streams included, before it exits.

## Backups and upgrades

The state is one SQLite file, `/var/lib/llmr/llmr.db`. Back it up with the container stopped,
or while it runs from a second container with SQLite's online backup:

```sh
docker run --rm -v llmr-data:/data -v "$PWD":/backup keinos/sqlite3 \
  sqlite3 /data/llmr.db ".backup /backup/llmr-$(date +%F).db"
```

A backup restores only with the master key it was made under.

Upgrading is pulling a newer tag and recreating the container on the same volume. The
database is migrated forward on start. An older llmr refuses to open a database a newer one
has written, rather than guessing at it; to go back, restore the backup taken before the
upgrade.

## Logs

One line per request, on stdout (shown here without the timestamp and target):

```
INFO answered model=default route=anthropic/claude-sonnet-5 attempts=1 fell_through=0 input_tokens=812 output_tokens=64 stop="end_turn"
WARN fell through model=default route=openai/gpt-5.1 why="rate limited, retry after 2000ms (attempt 1 of 2, waiting 2000ms)"
```

`LLMR_LOG_FORMAT=json` writes the same as one JSON object per line. Prompts, replies and
credentials are never logged.

At startup llmr logs every provider it could not build and every route that cannot be used,
then asks each usable route whether it is reachable, without a billable call. A `fell
through` warning on a request that still succeeded is worth alerting on: a provider degrading
while nothing fails.

`GET /manage/status` and `GET /manage/routes` are the same picture for a panel.
