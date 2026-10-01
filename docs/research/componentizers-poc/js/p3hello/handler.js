import { Response, Fields } from "wasi:http/types@0.3.0";

export const handler = {
  async handle(_request) {
    const { readable, writable } = wit.Stream(wit.Stream.U8);
    const trailers = wit.Future.from(
      Promise.resolve({ tag: "ok", val: undefined }),
      wit.Future.RESULT_OPTION_OTHER_ERROR_CODE,
    );
    const [response, _done] = Response.new(new Fields(), readable, trailers.readable);
    writable
      .writeAll(Uint8Array.from("hello from js p3 (qjs)\n", (c) => c.charCodeAt(0)))
      .then(() => writable.drop());
    return response;
  },
};
