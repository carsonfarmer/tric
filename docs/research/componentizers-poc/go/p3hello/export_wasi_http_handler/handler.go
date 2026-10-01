package export_wasi_http_handler

import (
	. "wit_component/wasi_http_types"

	. "go.bytecodealliance.org/pkg/wit/types"
)

var _ = asyncTagPresent

func Handle(request *Request) Result[*Response, ErrorCode] {
	return respond([]byte("hello from go p3 (componentize-go)\n"))
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
