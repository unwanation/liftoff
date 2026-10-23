// `fn_ts`: the contract function in TypeScript (FNC-01…06), of the world `liftoff:function/function` (`wit/function/`),
// built with componentize-js through `jco componentize` (`bun run build`,
// `just functions`). It answers exactly what `fn_rust`
// (`modules/testing/functions/rust/src/lib.rs`) answers:
//
// - `GET /`: `200`, `application/json`, `{"method", "path", "greeting",
//   "counter", "fetch"}` (`greeting` the `wasi:config` value `GREETING`,
//   `counter` `wasi:keyvalue/atomics.increment` of `counter` in the
//   bucket `default`, `fetch` the body of a `GET` of the `wasi:config`
//   value `FETCH_URL`; the warm-up with `X-Liftoff-Warmup` neither counts
//   nor fetches);
// - `POST /echo`: `200` with the request body and its `content-type`;
// - anything else: `404`.

import { get as configGet } from "wasi:config/store@0.2.0-rc.1";
import { handle as outgoingHandle } from "wasi:http/outgoing-handler@0.2.12";
import {
  Fields,
  IncomingBody,
  type IncomingRequest,
  OutgoingBody,
  OutgoingRequest,
  OutgoingResponse,
  ResponseOutparam,
} from "wasi:http/types@0.2.12";
import type { InputStream } from "wasi:io/streams@0.2.12";
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
      const visitor = request.headers().get("x-liftoff-warmup").length === 0;
      const fetchUrl = config("FETCH_URL");
      const answer = {
        method: "GET",
        path: target,
        greeting: config("GREETING"),
        counter: visitor ? counter() : null,
        fetch: visitor && fetchUrl !== null ? fetchText(fetchUrl) : null,
      };
      respond(
        responseOut,
        200,
        "application/json",
        encoder.encode(JSON.stringify(answer)),
      );
    } else if (method === "post" && path === "/echo") {
      const raw = request.headers().get("content-type")[0];
      const contentType =
        raw === undefined ? "application/octet-stream" : decoder.decode(raw);
      respond(responseOut, 200, contentType, requestBody(request));
    } else {
      respond(responseOut, 404, "text/plain", encoder.encode("not found"));
    }
  },
};

/** The `wasi:config` value `key`, or `null`. */
function config(key: string): string | null {
  try {
    return configGet(key) ?? null;
  } catch {
    return null;
  }
}

/** The counter after one more request, or `null`. */
function counter(): number | null {
  try {
    return Number(increment(open("default"), "counter", 1n));
  } catch {
    return null;
  }
}

/** The WIT error a binding threw: the payload of its error, or itself. */
function wasiError(thrown: unknown): unknown {
  if (thrown !== null && typeof thrown === "object" && "payload" in thrown) {
    return thrown.payload;
  }
  return thrown;
}

/** Reads `stream` to its end (a closed stream throws). */
function readAll(stream: InputStream): Uint8Array {
  const chunks: Uint8Array[] = [];
  let length = 0;
  for (;;) {
    let chunk: Uint8Array;
    try {
      chunk = stream.blockingRead(65536n);
    } catch {
      break;
    }
    chunks.push(chunk);
    length += chunk.length;
  }
  const read = new Uint8Array(length);
  let at = 0;
  for (const chunk of chunks) {
    read.set(chunk, at);
    at += chunk.length;
  }
  return read;
}

/** The body of `request`. */
function requestBody(request: IncomingRequest): Uint8Array {
  const body = request.consume();
  const stream = body.stream();
  const read = readAll(stream);
  stream[Symbol.dispose]();
  IncomingBody.finish(body);
  return read;
}

/** The body of `GET url` as a string, or `"error: <code>"`. */
function fetchText(url: string): string {
  try {
    return decoder.decode(get(url));
  } catch (thrown) {
    const code = wasiError(thrown);
    const tag =
      code !== null && typeof code === "object" && "tag" in code
        ? String(code.tag)
        : String(code);
    return `error: ${caseName(tag)}`;
  }
}

/**
 * The WIT case `tag` (`connection-refused`) as the other contract
 * functions name it (`ConnectionRefused`).
 */
function caseName(tag: string): string {
  return tag
    .split("-")
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1).toLowerCase())
    .join("");
}

/** `GET url` (`http://authority/path`) through the host. */
function get(url: string): Uint8Array {
  const separator = url.indexOf("://");
  if (separator < 0) {
    throw new Error("bad url");
  }
  const scheme = url.slice(0, separator);
  const rest = url.slice(separator + 3);
  const slash = rest.indexOf("/");
  const authority = slash < 0 ? rest : rest.slice(0, slash);
  const pathWithQuery = slash < 0 ? "/" : rest.slice(slash);
  const request = new OutgoingRequest(new Fields());
  request.setScheme(scheme === "https" ? { tag: "HTTPS" } : { tag: "HTTP" });
  request.setAuthority(authority);
  request.setPathWithQuery(pathWithQuery);
  const pending = outgoingHandle(request, undefined);
  pending.subscribe().block();
  const outcome = pending.get();
  if (outcome === undefined || outcome.tag === "err") {
    throw new Error("no response");
  }
  if (outcome.val.tag === "err") {
    throw outcome.val.val;
  }
  const response = outcome.val.val;
  const body = response.consume();
  const stream = body.stream();
  const read = readAll(stream);
  stream[Symbol.dispose]();
  IncomingBody.finish(body);
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
  for (let at = 0; at < body.length; at += 4096) {
    stream.blockingWriteAndFlush(body.subarray(at, at + 4096));
  }
  stream[Symbol.dispose]();
  OutgoingBody.finish(outgoing, undefined);
}
