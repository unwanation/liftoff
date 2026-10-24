# {{project}}

An HTTP function in Python: a WebAssembly component of the world in
`wit/world.wit` (`wasi:http/proxy@0.2` plus `wasi:config` and
`wasi:keyvalue`), built with
[componentize-py](https://github.com/bytecodealliance/componentize-py)
and run by liftoff.

| Request | Answer |
|---|---|
| `GET /` | `{"greeting", "visits"}`: the secret `GREETING` or a default, and a counter in the environment's key-value store |
| `POST /echo` | the request's body and `content-type` |
| anything else | `404` |

The handler is the class `IncomingHandler` of `app.py`; `wit/deps/`
holds the WASI WIT it imports. Pure Python only: packages with C
extensions (`numpy`, `pydantic-core`, `psycopg`) do not build.

## Develop

Needs [uv](https://docs.astral.sh/uv/) and the
[wasmtime](https://wasmtime.dev) CLI.

```sh
uv run componentize-py -d wit -w function componentize app -o dist/fn.wasm && wasmtime serve -Scli -Sconfig -Skeyvalue --addr 127.0.0.1:8080 dist/fn.wasm
lof expose 8080      # a private address for it on the network, through the server
lof secret pull --write   # the environment's secrets in .env.prod
```

The first line is the `[dev] command` of `liftoff.toml` on port 8080:
it builds the component and serves it on `http://localhost:8080`. It
passes no `wasi:config` values to `wasmtime serve` (they would go on its command line, where
`ps` shows them), so `GET /` answers the default greeting locally. Under `wasmtime serve` the
key-value store lives in memory and starts empty for every request, so
`visits` stays `1` there; on liftoff it is the environment's store and
keeps counting.

## Deploy

```sh
lof deploy
```

runs `[function] build` and uploads `dist/fn.wasm`; the CI workflow `lof
create` committed does the same on every push (its runner needs
uv). Secrets reach the
function through `wasi:config` (`lof secret set GREETING=…`).
