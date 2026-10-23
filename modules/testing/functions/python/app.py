"""`fn_python`: the contract function in Python (FNC-01…06), of the world `liftoff:function/function` (`wit/function/`),
built with componentize-py (`just functions`). It answers exactly what
`fn_rust` (`modules/testing/functions/rust/src/lib.rs`) answers:

- `GET /`: `200`, `application/json`, `{"method", "path", "greeting",
  "counter", "fetch"}` (`greeting` the `wasi:config` value `GREETING`,
  `counter` `wasi:keyvalue/atomics.increment` of `counter` in the bucket
  `default`, `fetch` the body of a `GET` of the `wasi:config` value
  `FETCH_URL`; the warm-up with `X-Liftoff-Warmup` neither counts nor
  fetches);
- `POST /echo`: `200` with the request body and its `content-type`;
- anything else: `404`.
"""

import json
from typing import Optional

from componentize_py_types import Err, Ok
from wit_world import exports
from wit_world.imports import atomics, outgoing_handler
from wit_world.imports import wasi_config_store as config
from wit_world.imports import wasi_keyvalue_store as keyvalue
from wit_world.imports.streams import InputStream
from wit_world.imports.types import (
    Fields,
    IncomingBody,
    IncomingRequest,
    Method_Get,
    Method_Post,
    OutgoingBody,
    OutgoingRequest,
    OutgoingResponse,
    ResponseOutparam,
    Scheme_Http,
    Scheme_Https,
)


class IncomingHandler(exports.IncomingHandler):
    """The component's export."""

    def handle(self, request: IncomingRequest, response_out: ResponseOutparam) -> None:
        target = request.path_with_query() or ""
        path = target.split("?", 1)[0]
        method = request.method()
        if isinstance(method, Method_Get) and path == "/":
            visitor = not request.headers().get("x-liftoff-warmup")
            fetch_url = config_value("FETCH_URL")
            answer = {
                "method": "GET",
                "path": target,
                "greeting": config_value("GREETING"),
                "counter": counter() if visitor else None,
                "fetch": fetch(fetch_url) if visitor and fetch_url is not None else None,
            }
            respond(response_out, 200, "application/json", json.dumps(answer).encode())
        elif isinstance(method, Method_Post) and path == "/echo":
            raw = request.headers().get("content-type")
            content_type = raw[0].decode(errors="replace") if raw else "application/octet-stream"
            respond(response_out, 200, content_type, request_body(request))
        else:
            respond(response_out, 404, "text/plain", b"not found")


def config_value(key: str) -> Optional[str]:
    """The `wasi:config` value `key`, or `None`."""
    try:
        return config.get(key)
    except Err:
        return None


def counter() -> Optional[int]:
    """The counter after one more request, or `None`."""
    try:
        return atomics.increment(keyvalue.open("default"), "counter", 1)
    except Err:
        return None


def read_all(stream: InputStream) -> bytes:
    """Reads `stream` to its end (a closed stream raises `Err`)."""
    chunks = []
    while True:
        try:
            chunks.append(stream.blocking_read(64 * 1024))
        except Err:
            return b"".join(chunks)


def request_body(request: IncomingRequest) -> bytes:
    """The body of `request`."""
    body = request.consume()
    with body.stream() as stream:
        read = read_all(stream)
    IncomingBody.finish(body)
    return read


def fetch(url: str) -> str:
    """The body of `GET url` as a string, or `"error: <code>"`."""
    try:
        return get(url).decode(errors="replace")
    except Err as error:
        return f"error: {type(error.value).__name__.removeprefix('ErrorCode_')}"
    except ValueError as error:
        return f"error: {error}"


def get(url: str) -> bytes:
    """`GET url` (`http://authority/path`) through the host."""
    scheme, separator, rest = url.partition("://")
    if not separator:
        raise ValueError("bad url")
    slash = rest.find("/")
    authority, path = (rest, "/") if slash < 0 else (rest[:slash], rest[slash:])
    request = OutgoingRequest(Fields())
    request.set_scheme(Scheme_Https() if scheme == "https" else Scheme_Http())
    request.set_authority(authority)
    request.set_path_with_query(path)
    pending = outgoing_handler.handle(request, None)
    pending.subscribe().block()
    outcome = pending.get()
    if outcome is None or not isinstance(outcome, Ok):
        raise ValueError("no response")
    if isinstance(outcome.value, Err):
        raise outcome.value
    response = outcome.value.value
    body = response.consume()
    with body.stream() as stream:
        read = read_all(stream)
    IncomingBody.finish(body)
    return read


def respond(response_out: ResponseOutparam, status: int, content_type: str, body: bytes) -> None:
    """Sends the response."""
    response = OutgoingResponse(Fields.from_list([("content-type", content_type.encode())]))
    response.set_status_code(status)
    outgoing = response.body()
    ResponseOutparam.set(response_out, Ok(response))
    with outgoing.write() as stream:
        for at in range(0, len(body), 4096):
            stream.blocking_write_and_flush(body[at : at + 4096])
    OutgoingBody.finish(outgoing, None)
