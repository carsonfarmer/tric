import componentize_py_async_support
import wit_world
from componentize_py_types import Ok
from wit_world import exports
from wit_world.imports import wasi_config_store as config, wasi_keyvalue_store as kv
from wit_world.imports.wasi_http_types import Fields, Response


async def write(tx, data):
    with tx:
        await tx.write_all(data)


class Handler(exports.Handler):
    async def handle(self, request):
        greeting = config.get("greeting")
        bucket = kv.open("")
        seed = bucket.get("seed")
        bucket.set("k", b"v1")
        k = bucket.get("k")
        text = f"config.greeting={greeting} kv.seed={seed!r} kv.k={k!r}\n"
        tx, rx = wit_world.byte_stream()
        componentize_py_async_support.spawn(write(tx, text.encode()))
        trailers = wit_world.result_option_wasi_http_types_fields_wasi_http_types_error_code_future(lambda: Ok(None))[1]
        return Response.new(Fields.from_list([("content-type", b"text/plain")]), rx, trailers)[0]
