# {{project}}

An HTTP function in TypeScript: a WebAssembly component of the world in
`wit/world.wit` (`wasi:http/proxy@0.2` plus `wasi:config` and
`wasi:keyvalue`), built with
[componentize-js](https://github.com/bytecodealliance/ComponentizeJS)
through `jco componentize` and run by liftoff.

| Request | Answer |
|---|---|
| `GET /` | `{"greeting", "visits"}`: the secret `GREETING` or a default, and a counter in the environment's key-value store |
| `POST /echo` | the request's body and `content-type` |
| anything else | `404` |

The handler is `src/index.ts`; it exports `incomingHandler` and speaks
`wasi:http/types` directly (the `addEventListener("fetch", …)` style does
not build with componentize-js 0.23). `wit/deps/` holds the WASI WIT it
imports.

## Develop

Needs [Bun](https://bun.sh) and the [wasmtime](https://wasmtime.dev) CLI.

```sh
bun install && bun run build && wasmtime serve -Scli -Sconfig -Skeyvalue --addr 127.0.0.1:8080 dist/fn.wasm
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
Bun). Secrets reach the
function through `wasi:config` (`lof secret set GREETING=…`).
