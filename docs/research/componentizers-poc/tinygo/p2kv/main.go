package main

import (
	"fmt"

	config "example.com/kv/internal/wasi/config/store"
	incominghandler "example.com/kv/internal/wasi/http/incoming-handler"
	"example.com/kv/internal/wasi/http/types"
	kv "example.com/kv/internal/wasi/keyvalue/store"
	"go.bytecodealliance.org/cm"
)

func showBytes(o cm.Option[cm.List[uint8]]) string {
	if p := o.Some(); p != nil {
		return fmt.Sprintf("Some(%q)", string(p.Slice()))
	}
	return "None"
}

func init() {
	incominghandler.Exports.Handle = func(request types.IncomingRequest, responseOut types.ResponseOutparam) {
		greeting := "ERR"
		if r := config.Get("greeting"); r.IsOK() {
			v := r.OK()
			if p := v.Some(); p != nil {
				greeting = *p
			}
		}
		seed, k := "ERR", "ERR"
		if b := kv.Open(""); b.IsOK() {
			bucket := *b.OK()
			if r := bucket.Get("seed"); r.IsOK() {
				seed = showBytes(*r.OK())
			}
			bucket.Set("k", cm.ToList([]byte("v1")))
			if r := bucket.Get("k"); r.IsOK() {
				k = showBytes(*r.OK())
			}
		}
		msg := []byte(fmt.Sprintf("config.greeting=%s kv.seed=%s kv.k=%s\n", greeting, seed, k))

		response := types.NewOutgoingResponse(types.NewFields())
		bodyRes := response.Body()
		body := bodyRes.OK()
		types.ResponseOutparamSet(responseOut, cm.OK[cm.Result[types.ErrorCodeShape, types.OutgoingResponse, types.ErrorCode]](response))
		streamRes := body.Write()
		stream := streamRes.OK()
		stream.BlockingWriteAndFlush(cm.ToList(msg))
		stream.ResourceDrop()
		types.OutgoingBodyFinish(*body, cm.None[types.Trailers]())
	}
}

func main() {}
