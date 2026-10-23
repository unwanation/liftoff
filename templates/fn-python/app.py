"""The function of {{project}}.

- `GET /`: `200`, JSON `{"greeting", "visits"}`: the secret `GREETING`
  (read through `wasi:config`) or a default, and how many times the page
  was asked for (a counter in `wasi:keyvalue`);
- `POST /echo`: `200` with the request's body and `content-type`;
- anything else: `404`.
"""

import json
from typing import Optional

from componentize_py_types import Err, Ok
from wit_world import exports
from wit_world.imports import atomics
from wit_world.imports import wasi_config_store as config
from wit_world.imports import wasi_keyvalue_store as keyvalue
from wit_world.imports.types import (
    Fields,
    IncomingBody,
    IncomingRequest,
    Method_Get,
    Method_Post,
    OutgoingBody,
    OutgoingResponse,
    ResponseOutparam,
)

DEFAULT_GREETING = "Hello from {{project}}"


class IncomingHandler(exports.IncomingHandler):
    """The component's export."""

    def handle(self, request: IncomingRequest, response_out: ResponseOutparam) -> None:
        target = request.path_with_query() or ""
        path = target.split("?", 1)[0]
        method = request.method()
        if isinstance(method, Method_Get) and path == "/":
            # liftoff warms a new deploy up with one `GET /` that carries
            # this header: it is no visit.
            visit = not request.headers().get("x-liftoff-warmup")
            answer = {"greeting": greeting(), "visits": visits() if visit else None}
            respond(response_out, 200, "application/json", json.dumps(answer).encode())
        elif isinstance(method, Method_Post) and path == "/echo":
            raw = request.headers().get("content-type")
            content_type = raw[0].decode(errors="replace") if raw else "application/octet-stream"
            respond(response_out, 200, content_type, request_body(request))
        else:
            respond(response_out, 404, "text/plain", b"not found\n")


def greeting() -> str:
    """The secret `GREETING`, or the default."""
    try:
        return config.get("GREETING") or DEFAULT_GREETING
    except Err:
        return DEFAULT_GREETING


def visits() -> Optional[int]:
    """The counter `visits` after one more visit, or `None` when the
    key-value store refuses. The bucket `""` is the environment's one on
    liftoff and the in-memory one of `wasmtime serve`."""
    try:
        return atomics.increment(keyvalue.open(""), "visits", 1)
    except Err:
        return None


def request_body(request: IncomingRequest) -> bytes:
    """The body of `request`."""
    body = request.consume()
    chunks = []
    with body.stream() as stream:
        while True:
            try:
                chunks.append(stream.blocking_read(64 * 1024))
            except Err:
                # The stream ends with `closed`.
                break
    IncomingBody.finish(body)
    return b"".join(chunks)


def respond(response_out: ResponseOutparam, status: int, content_type: str, body: bytes) -> None:
    """Sends the response."""
    response = OutgoingResponse(Fields.from_list([("content-type", content_type.encode())]))
    response.set_status_code(status)
    outgoing = response.body()
    ResponseOutparam.set(response_out, Ok(response))
    with outgoing.write() as stream:
        # One write takes at most 4096 bytes.
        for at in range(0, len(body), 4096):
            stream.blocking_write_and_flush(body[at : at + 4096])
    OutgoingBody.finish(outgoing, None)
