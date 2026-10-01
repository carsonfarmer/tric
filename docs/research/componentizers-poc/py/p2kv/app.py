from componentize_py_types import Ok
from wit_world import exports
from wit_world.imports import wasi_config_store as config, wasi_keyvalue_store as kv
from wit_world.imports.types import Fields, OutgoingResponse, OutgoingBody, ResponseOutparam


class IncomingHandler(exports.IncomingHandler):
    def handle(self, request, response_out):
        greeting = config.get("greeting")
        bucket = kv.open("")
        seed = bucket.get("seed")
        bucket.set("k", b"v1")
        k = bucket.get("k")
        text = f"config.greeting={greeting} kv.seed={seed!r} kv.k={k!r}\n"
        resp = OutgoingResponse(Fields.from_list([("content-type", b"text/plain")]))
        body = resp.body()
        ResponseOutparam.set(response_out, Ok(resp))
        with body.write() as stream:
            stream.blocking_write_and_flush(text.encode())
        OutgoingBody.finish(body, None)
