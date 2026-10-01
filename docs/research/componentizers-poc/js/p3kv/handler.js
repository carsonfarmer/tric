import { Response, Fields } from "wasi:http/types@0.3.0";
import { get } from "wasi:config/store@0.2.0-rc.1";
import { open } from "wasi:keyvalue/store@0.2.0-draft";

const bytes = (s) => Uint8Array.from(s, (c) => c.charCodeAt(0));
const str = (v) => (v === undefined ? undefined : String.fromCharCode(...v));

export const handler = {
  async handle(_request) {
    const greeting = get("greeting") ?? "(unset)";
    const bucket = open("");
    const seed = str(bucket.get("seed"));
    bucket.set("k", bytes("v1"));
    const k = str(bucket.get("k"));
    const body = `config.greeting=${greeting} kv.seed=${JSON.stringify(seed)} kv.k=${JSON.stringify(k)}\n`;

    const { readable, writable } = wit.Stream(wit.Stream.U8);
    const trailers = wit.Future.from(
      Promise.resolve({ tag: "ok", val: undefined }),
      wit.Future.RESULT_OPTION_OTHER_ERROR_CODE,
    );
    const [response, _done] = Response.new(new Fields(), readable, trailers.readable);
    writable.writeAll(bytes(body)).then(() => writable.drop());
    return response;
  },
};
