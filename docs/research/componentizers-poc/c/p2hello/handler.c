#include "app.h"
#include <string.h>

// wasi:http/incoming-handler@0.2.0 export on wit-bindgen 0.62.0 C bindings.
void exports_wasi_http_incoming_handler_handle(exports_wasi_http_incoming_handler_own_incoming_request_t request,
                                               exports_wasi_http_incoming_handler_own_response_outparam_t response_out) {
  wasi_http_types_incoming_request_drop_own(request);
  wasi_http_types_own_fields_t headers = wasi_http_types_constructor_fields();
  wasi_http_types_own_outgoing_response_t resp = wasi_http_types_constructor_outgoing_response(headers);
  wasi_http_types_borrow_outgoing_response_t resp_b = wasi_http_types_borrow_outgoing_response(resp);
  wasi_http_types_method_outgoing_response_set_status_code(resp_b, 200);
  wasi_http_types_own_outgoing_body_t body;
  wasi_http_types_method_outgoing_response_body(resp_b, &body);
  wasi_http_types_borrow_outgoing_body_t body_b = wasi_http_types_borrow_outgoing_body(body);
  wasi_io_streams_own_output_stream_t out;
  wasi_http_types_method_outgoing_body_write(body_b, &out);
  wasi_io_streams_borrow_output_stream_t out_b = wasi_io_streams_borrow_output_stream(out);
  const char *msg = "hello from c\n";
  app_list_u8_t data = { (uint8_t *)msg, strlen(msg) };
  wasi_io_streams_stream_error_t err;
  wasi_io_streams_method_output_stream_blocking_write_and_flush(out_b, &data, &err);
  wasi_io_streams_output_stream_drop_own(out);
  wasi_http_types_static_outgoing_body_finish(body, NULL, NULL);
  wasi_http_types_result_own_outgoing_response_error_code_t result = { .is_err = false, .val = { .ok = resp } };
  wasi_http_types_static_response_outparam_set(response_out, &result);
}
