package main

import (
	incominghandler "example.com/hello/internal/wasi/http/incoming-handler"
	"example.com/hello/internal/wasi/http/types"
	"go.bytecodealliance.org/cm"
)

func init() {
	incominghandler.Exports.Handle = func(request types.IncomingRequest, responseOut types.ResponseOutparam) {
		response := types.NewOutgoingResponse(types.NewFields())
		bodyRes := response.Body()
		body := bodyRes.OK()
		types.ResponseOutparamSet(responseOut, cm.OK[cm.Result[types.ErrorCodeShape, types.OutgoingResponse, types.ErrorCode]](response))
		streamRes := body.Write()
		stream := streamRes.OK()
		msg := []byte("hello from tinygo p2\n")
		stream.BlockingWriteAndFlush(cm.ToList(msg))
		stream.ResourceDrop()
		types.OutgoingBodyFinish(*body, cm.None[types.Trailers]())
	}
}

func main() {}
