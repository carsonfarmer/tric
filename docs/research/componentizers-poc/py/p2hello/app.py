from componentize_py_types import Ok
from wit_world import exports
from wit_world.imports.types import Fields, OutgoingResponse, OutgoingBody, ResponseOutparam


class IncomingHandler(exports.IncomingHandler):
    def handle(self, request, response_out):
        resp = OutgoingResponse(Fields.from_list([("content-type", b"text/plain")]))
        body = resp.body()
        ResponseOutparam.set(response_out, Ok(resp))
        with body.write() as stream:
            stream.blocking_write_and_flush(b"hello from python p2\n")
        OutgoingBody.finish(body, None)
