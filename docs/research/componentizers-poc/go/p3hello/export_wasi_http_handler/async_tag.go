//go:build componentizego_async

package export_wasi_http_handler

// componentize-go passes the `componentizego_async` build tag to `go build`
// when the target world uses async features, as this example's
// `wasip3-example` world does.  The reference to this constant in handler.go
// will fail to compile if the tag does not reach the Go compiler.
const asyncTagPresent = true
