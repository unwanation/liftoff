// The function of {{project}}.
//
// - `GET /`: `200`, JSON `{"greeting", "visits"}`: the secret `GREETING`
//   (read through `wasi:config`) or a default, and how many times the
//   page was asked for (a counter in `wasi:keyvalue`);
// - `POST /echo`: `200` with the request's body and `content-type`;
// - anything else: `404`.

import { get as configGet } from "wasi:config/store@0.2.0-rc.1";
import {
  Fields,
  IncomingBody,
  type IncomingRequest,
  OutgoingBody,
  OutgoingResponse,
  ResponseOutparam,
} from "wasi:http/types@0.2.12";
import { increment } from "wasi:keyvalue/atomics@0.2.0-draft";
import { open } from "wasi:keyvalue/store@0.2.0-draft";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

export const incomingHandler = {
  handle(request: IncomingRequest, responseOut: ResponseOutparam) {
    const target = request.pathWithQuery() ?? "";
    const path = target.split("?", 1)[0];
    const method = request.method().tag;
    if (method === "get" && path === "/") {
      // liftoff warms a new deploy up with one `GET /` that carries this
      // header: it is no visit.
      const visit = request.headers().get("x-liftoff-warmup").length === 0;
      const answer = {
        greeting: greeting(),
        visits: visit ? visits() : null,
      };
      respond(responseOut, 200, "application/json", encoder.encode(JSON.stringify(answer)));
    } else if (method === "post" && path === "/echo") {
      const raw = request.headers().get("content-type")[0];
      const contentType = raw === undefined ? "application/octet-stream" : decoder.decode(raw);
      respond(responseOut, 200, contentType, requestBody(request));
    } else {
      respond(responseOut, 404, "text/plain", encoder.encode("not found\n"));
    }
  },
};

/** The secret `GREETING`, or the default. */
function greeting(): string {
  try {
    return configGet("GREETING") ?? "Hello from {{project}}";
  } catch {
    return "Hello from {{project}}";
  }
}

/**
 * The counter `visits` after one more visit, or `null` when the key-value
 * store refuses. The bucket `""` is the environment's one on liftoff and
 * the in-memory one of `wasmtime serve`.
 */
function visits(): number | null {
  try {
    return Number(increment(open(""), "visits", 1n));
  } catch {
    return null;
  }
}

/** The body of `request`. */
function requestBody(request: IncomingRequest): Uint8Array {
  const body = request.consume();
  const stream = body.stream();
  const chunks: Uint8Array[] = [];
  for (;;) {
    try {
      chunks.push(stream.blockingRead(65536n));
    } catch {
      // The stream ends with `closed`.
      break;
    }
  }
  stream[Symbol.dispose]();
  IncomingBody.finish(body);
  const read = new Uint8Array(chunks.reduce((length, chunk) => length + chunk.length, 0));
  let at = 0;
  for (const chunk of chunks) {
    read.set(chunk, at);
    at += chunk.length;
  }
  return read;
}

/** Sends the response. */
function respond(
  responseOut: ResponseOutparam,
  status: number,
  contentType: string,
  body: Uint8Array,
) {
  const response = new OutgoingResponse(
    Fields.fromList([["content-type", encoder.encode(contentType)]]),
  );
  response.setStatusCode(status);
  const outgoing = response.body();
  ResponseOutparam.set(responseOut, { tag: "ok", val: response });
  const stream = outgoing.write();
  // One write takes at most 4096 bytes.
  for (let at = 0; at < body.length; at += 4096) {
    stream.blockingWriteAndFlush(body.subarray(at, at + 4096));
  }
  stream[Symbol.dispose]();
  OutgoingBody.finish(outgoing, undefined);
}
