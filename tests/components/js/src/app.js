// The test app, as app.rs, but for `/print` and `/fs`, and with no more than WASI under it. Every route answers 200, or
// the `status=N` of its query, after `sleep=MS` milliseconds if given. Under `/@<name>` a route is the same, with the
// name as the store of `/kv` unless the query names one.
//
// - `/echo`: the request, as `{"method", "uri", "headers": [[name, value], ..], "body"}`; under a name, and with an
//   unsafe method, kept in the name's key `echo` too.
// - `/fetch?method=M&async=1&url=U`: sends M (GET by default) to U, which is the rest of the query, not decoded, with
//   `Prefer: respond-async` given `async`. The reply is `<status> <body>`, or the case of the `ErrorCode`.
// - `/kv?…`: see kv.js.
// - `/env`: the environment and the arguments.
// - `/stream?n=N`: N lines, a second apart, in a body that streams.
// - `/hog?mb=N` holds N MiB; `/fields?n=N` holds N `fields`; `/loop` spins; anything else: `hello`.
import types from "wasi:http/types@0.3.0";
import client from "wasi:http/client@0.3.0";
import environment from "wasi:cli/environment@0.3.0";
import clock from "wasi:clocks/monotonic-clock@0.3.0";
import { keep, respond } from "./kv.js";
import { debug, decode, encode } from "./util.js";

const { Fields, Request, Response } = types;
const unform = (s) => decodeURIComponent(s.replace(/\+/g, " "));
const trailers = () => wit.Future(wit.Future.RESULT_OPTION_OTHER_ERROR_CODE);

export const handler = {
  async handle(request) {
    const [, target, query] = /^([^?]*)\??(.*)/.exec(request.getPathWithQuery() ?? "");
    const named = /^\/@([^/]*)(\/.*)?$/.exec(target);
    const [name, path] = named ? [named[1], named[2] ?? "/"] : [null, target];
    const [, params, url = ""] = /^(.*?)(?:url=(.*))?$/.exec(query); // the first `url=`, and all that follows it
    const q = new Map();
    for (const pair of params.split("&")) {
      const [k, v = ""] = pair.split(/=(.*)/);
      if (k) q.set(unform(k), unform(v));
    }
    if (q.has("sleep")) await clock.waitFor(Number(q.get("sleep")) * 1e6);
    const n = Number(q.get("mb") ?? q.get("n") ?? 1);
    if (path === "/stream") return answer(lines(n));
    let body;
    switch (path) {
      case "/echo": {
        const echo = await describe(request);
        body = JSON.stringify(echo);
        if (name !== null && !["GET", "HEAD", "OPTIONS", "TRACE"].includes(echo.method)) keep(name, "echo", body);
        break;
      }
      case "/fetch":
        body = await send(q.get("method") ?? "GET", q.has("async"), url);
        break;
      case "/kv":
        body = JSON.stringify(respond(q.get("store") ?? name ?? "", q));
        break;
      case "/env":
        body = JSON.stringify({
          env: Object.fromEntries(environment.getEnvironment()),
          args: environment.getArguments(),
        });
        break;
      case "/hog":
        body = `hogged ${new Uint8Array(n << 20).fill(1).length >> 20} MiB`;
        break;
      case "/fields":
        body = `held ${Array.from({ length: n }, () => Fields.fromList([])).length} fields`;
        break;
      case "/loop":
        for (;;);
      default:
        body = "hello";
    }
    return answer([body], Number(q.get("status")) || 200);
  },
};

/** `n` lines, a second apart. */
async function* lines(n) {
  for (let i = 0; i < n; i++) {
    if (i > 0) await clock.waitFor(1e9);
    yield `${i}\n`;
  }
}

/** A response of `status`, whose body is the text `chunks` yields, written as the host reads it. */
function answer(chunks, status = 200) {
  const stream = wit.Stream(wit.Stream.U8);
  const end = trailers();
  const [response, done] = Response.new(Fields.fromList([]), stream.readable, end.readable);
  done.drop();
  response.setStatusCode(status);
  (async () => {
    for await (const chunk of chunks) await stream.writable.writeAll(encode(chunk));
    stream.writable.drop();
    await end.writable.write({ tag: "ok", val: null });
  })();
  return response;
}

/** The body of `message`, as text, which `consume` (`Request.consumeBody` or `Response.consumeBody`) consumes. */
async function text(consume, message) {
  const done = wit.Future(wit.Future.RESULT_VOID_ERROR_CODE);
  const [stream] = consume(message, done.readable);
  const written = done.writable.write({ tag: "ok", val: null });
  const bytes = [];
  for await (const chunk of stream) for (const byte of chunk) bytes.push(byte);
  await written;
  return decode(bytes);
}

/** The request, for `/echo`. */
async function describe(request) {
  const method = request.getMethod();
  const scheme = request.getScheme();
  const uri = `${scheme ? (scheme.val ?? scheme.tag).toLowerCase() : ""}://${request.getAuthority() ?? ""}`;
  return {
    method: method.tag === "other" ? method.val : method.tag.toUpperCase(),
    uri: uri + (request.getPathWithQuery() ?? ""),
    headers: request.getHeaders().copyAll().map(([k, v]) => [k, decode(v)]),
    body: await text(Request.consumeBody, request),
  };
}

/** `<status> <body>` of the response to a request for `url`, or the case of its error. */
async function send(method, respondAsync, url) {
  const [, scheme = "http", authority, path = "/"] = /^(?:(\w+):\/\/)?([^/]*)(\/.*)?$/.exec(url);
  const headers = Fields.fromList(respondAsync ? [["prefer", encode("respond-async")]] : []);
  const end = trailers();
  const [request] = Request.new(headers, null, end.readable, null);
  request.setMethod({ tag: method.toLowerCase() });
  request.setScheme({ tag: scheme === "https" ? "HTTPS" : "HTTP" });
  request.setAuthority(authority);
  request.setPathWithQuery(path);
  end.writable.write({ tag: "ok", val: null }); // done when the host reads it, which is after `send`
  let response;
  try {
    response = await client.send(request);
  } catch (e) {
    return debug(e);
  }
  return `${response.getStatusCode()} ${await text(Response.consumeBody, response)}`;
}
