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

Needs [Bun](https://bun.sh) and the [wasmtime](https://wasmtime.dev) CLI
for `lof dev`.

```sh
lof dev              # build, then serve on http://localhost:<PORT>
bun run build        # build dist/fn.wasm only
```

`lof dev` runs the `[dev] command` of `liftoff.toml` with `PORT` and the
project's secrets in its environment. The command passes no `wasi:config`
values to `wasmtime serve` (they would go on its command line, where
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
