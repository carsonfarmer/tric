package export_wasi_http_handler

import (
	"fmt"
	config "wit_component/wasi_config_store"
	. "wit_component/wasi_http_types"
	kv "wit_component/wasi_keyvalue_store"

	. "go.bytecodealliance.org/pkg/wit/types"
)

var _ = asyncTagPresent

func show(o Option[[]byte]) string {
	if o.IsSome() {
		return fmt.Sprintf("Some(%q)", string(o.Some()))
	}
	return "None"
}

func Handle(request *Request) Result[*Response, ErrorCode] {
	greeting := "ERR"
	if r := config.Get("greeting"); r.IsOk() {
		greeting = r.Ok().SomeOr("<none>")
	}
	seed, k := "ERR", "ERR"
	if b := kv.Open(""); b.IsOk() {
		bucket := b.Ok()
		if r := bucket.Get("seed"); r.IsOk() {
			seed = show(r.Ok())
		}
		bucket.Set("k", []byte("v1"))
		if r := bucket.Get("k"); r.IsOk() {
			k = show(r.Ok())
		}
	}
	return respond([]byte(fmt.Sprintf("config.greeting=%s kv.seed=%s kv.k=%s\n", greeting, seed, k)))
}

func respond(message []byte) Result[*Response, ErrorCode] {
	tx, rx := MakeStreamU8()
	go func() {
		defer tx.Drop()
		tx.WriteAll(message)
	}()
	response, send := ResponseNew(
		FieldsFromList([]Tuple2[string, []byte]{
			{F0: "content-type", F1: []byte("text/plain")},
		}).Ok(),
		Some(rx),
		trailersFuture(),
	)
	send.Drop()
	return Ok[*Response, ErrorCode](response)
}

func trailersFuture() *FutureReader[Result[Option[*Fields], ErrorCode]] {
	tx, rx := MakeFutureResultOptionFieldsErrorCode()
	go tx.Write(Ok[Option[*Fields], ErrorCode](None[*Fields]()))
	return rx
}
