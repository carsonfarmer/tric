package export_wasi_http_incoming_handler

import (
	"fmt"
	config "wit_component/wasi_config_store"
	. "wit_component/wasi_http_types"
	kv "wit_component/wasi_keyvalue_store"

	. "go.bytecodealliance.org/pkg/wit/types"
)

func opt(o Option[string]) string {
	if o.IsSome() {
		return fmt.Sprintf("Some(%q)", o.Some())
	}
	return "None"
}

func Handle(request *IncomingRequest, responseOut *ResponseOutparam) {
	greeting := "ERR"
	if r := config.Get("greeting"); r.IsOk() {
		greeting = r.Ok().SomeOr("<none>")
	}
	seed, k := "ERR", "ERR"
	if b := kv.Open(""); b.IsOk() {
		bucket := b.Ok()
		if r := bucket.Get("seed"); r.IsOk() {
			o := r.Ok()
			if o.IsSome() {
				seed = fmt.Sprintf("Some(%q)", string(o.Some()))
			} else {
				seed = "None"
			}
		}
		bucket.Set("k", []byte("v1"))
		if r := bucket.Get("k"); r.IsOk() {
			o := r.Ok()
			if o.IsSome() {
				k = fmt.Sprintf("Some(%q)", string(o.Some()))
			} else {
				k = "None"
			}
		}
	}
	message := []byte(fmt.Sprintf("config.greeting=%s kv.seed=%s kv.k=%s\n", greeting, seed, k))

	response := MakeOutgoingResponse(MakeFields())
	body := response.Body()
	ResponseOutparamSet(responseOut, Ok[*OutgoingResponse, ErrorCode](response))
	if body.IsOk() {
		stream := body.Ok()
		if writeResult := stream.Write(); writeResult.IsOk() {
			writeResult.Ok().BlockingWriteAndFlush(message)
		}
	}
}
