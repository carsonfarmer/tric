#include "app.h"
#include <stdio.h>
#include <string.h>

// wasi:http/incoming-handler@0.2.0 + wasi:config/store@0.2.0-rc.1 + wasi:keyvalue/store@0.2.0-draft
static void respond(exports_wasi_http_incoming_handler_own_response_outparam_t response_out, const char *msg) {
  wasi_http_types_own_fields_t headers = wasi_http_types_constructor_fields();
  wasi_http_types_own_outgoing_response_t resp = wasi_http_types_constructor_outgoing_response(headers);
  wasi_http_types_borrow_outgoing_response_t resp_b = wasi_http_types_borrow_outgoing_response(resp);
  wasi_http_types_method_outgoing_response_set_status_code(resp_b, 200);
  wasi_http_types_own_outgoing_body_t body;
  wasi_http_types_method_outgoing_response_body(resp_b, &body);
  wasi_io_streams_own_output_stream_t out;
  wasi_http_types_method_outgoing_body_write(wasi_http_types_borrow_outgoing_body(body), &out);
  app_list_u8_t data = { (uint8_t *)msg, strlen(msg) };
  wasi_io_streams_stream_error_t err;
  wasi_io_streams_method_output_stream_blocking_write_and_flush(wasi_io_streams_borrow_output_stream(out), &data, &err);
  wasi_io_streams_output_stream_drop_own(out);
  wasi_http_types_static_outgoing_body_finish(body, NULL, NULL);
  wasi_http_types_result_own_outgoing_response_error_code_t result = { .is_err = false, .val = { .ok = resp } };
  wasi_http_types_static_response_outparam_set(response_out, &result);
}

static int fmt_opt(char *buf, size_t n, const char *name, app_option_list_u8_t *v) {
  if (!v->is_some) return snprintf(buf, n, " kv.%s=None", name);
  return snprintf(buf, n, " kv.%s=Some(\"%.*s\")", name, (int)v->val.len, (char *)v->val.ptr);
}

void exports_wasi_http_incoming_handler_handle(exports_wasi_http_incoming_handler_own_incoming_request_t request,
                                               exports_wasi_http_incoming_handler_own_response_outparam_t response_out) {
  wasi_http_types_incoming_request_drop_own(request);
  char msg[512]; int n = 0;

  app_string_t ckey = { (uint8_t *)"greeting", 8 };
  app_option_string_t cval; wasi_config_store_error_t cerr;
  if (wasi_config_store_get(&ckey, &cval, &cerr) && cval.is_some)
    n += snprintf(msg + n, sizeof msg - n, "config.greeting=%.*s", (int)cval.val.len, (char *)cval.val.ptr);
  else
    n += snprintf(msg + n, sizeof msg - n, "config.greeting=None");

  app_string_t id = { (uint8_t *)"", 0 };
  wasi_keyvalue_store_own_bucket_t bucket; wasi_keyvalue_store_error_t kerr;
  if (!wasi_keyvalue_store_open(&id, &bucket, &kerr)) { respond(response_out, "kv open failed\n"); return; }
  wasi_keyvalue_store_borrow_bucket_t b = wasi_keyvalue_store_borrow_bucket(bucket);

  app_option_list_u8_t v;
  app_string_t seed = { (uint8_t *)"seed", 4 };
  wasi_keyvalue_store_method_bucket_get(b, &seed, &v, &kerr);
  n += fmt_opt(msg + n, sizeof msg - n, "seed", &v);

  app_string_t k = { (uint8_t *)"k", 1 };
  app_list_u8_t val = { (uint8_t *)"v1", 2 };
  wasi_keyvalue_store_method_bucket_set(b, &k, &val, &kerr);
  wasi_keyvalue_store_method_bucket_get(b, &k, &v, &kerr);
  n += fmt_opt(msg + n, sizeof msg - n, "k", &v);
  n += snprintf(msg + n, sizeof msg - n, "\n");

  wasi_keyvalue_store_bucket_drop_own(bucket);
  respond(response_out, msg);
}
