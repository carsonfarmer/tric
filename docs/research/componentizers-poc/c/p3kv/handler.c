#include "app.h"
#include <string.h>

// wasi:http/handler@0.3.0 export (async, callback-style) on wit-bindgen 0.62.0 C bindings.
#include <stdio.h>
static char MSG[512]; static size_t MSG_LEN;
static int fmt_opt(char *buf, size_t n, const char *name, app_option_list_u8_t *v) {
  if (!v->is_some) return snprintf(buf, n, " kv.%s=None", name);
  return snprintf(buf, n, " kv.%s=Some(\"%.*s\")", name, (int)v->val.len, (char *)v->val.ptr);
}
static void compute(void) {
  int n = 0;
  app_string_t ckey = { (uint8_t *)"greeting", 8 };
  app_option_string_t cval; wasi_config_store_error_t cerr;
  if (wasi_config_store_get(&ckey, &cval, &cerr) && cval.is_some)
    n += snprintf(MSG + n, sizeof MSG - n, "config.greeting=%.*s", (int)cval.val.len, (char *)cval.val.ptr);
  else
    n += snprintf(MSG + n, sizeof MSG - n, "config.greeting=None");
  app_string_t id = { (uint8_t *)"", 0 };
  wasi_keyvalue_store_own_bucket_t bucket; wasi_keyvalue_store_error_t kerr;
  if (!wasi_keyvalue_store_open(&id, &bucket, &kerr)) { MSG_LEN = snprintf(MSG, sizeof MSG, "kv open failed\n"); return; }
  wasi_keyvalue_store_borrow_bucket_t b = wasi_keyvalue_store_borrow_bucket(bucket);
  app_option_list_u8_t v;
  app_string_t seed = { (uint8_t *)"seed", 4 };
  wasi_keyvalue_store_method_bucket_get(b, &seed, &v, &kerr);
  n += fmt_opt(MSG + n, sizeof MSG - n, "seed", &v);
  app_string_t k = { (uint8_t *)"k", 1 };
  app_list_u8_t val = { (uint8_t *)"v1", 2 };
  wasi_keyvalue_store_method_bucket_set(b, &k, &val, &kerr);
  wasi_keyvalue_store_method_bucket_get(b, &k, &v, &kerr);
  n += fmt_opt(MSG + n, sizeof MSG - n, "k", &v);
  n += snprintf(MSG + n, sizeof MSG - n, "\n");
  MSG_LEN = n;
  wasi_keyvalue_store_bucket_drop_own(bucket);
}
static wasi_http_types_stream_u8_writer_t body_writer;
static wasi_http_types_future_result_option_own_trailers_error_code_writer_t trailers_writer;
static app_waitable_set_t wset;

static void finish(void) {
  wasi_http_types_stream_u8_drop_writable(body_writer);
  wasi_http_types_result_option_own_trailers_error_code_t t = { .is_err = false, .val = { .ok = { .is_some = false } } };
  wasi_http_types_future_result_option_own_trailers_error_code_write(trailers_writer, &t);
  wasi_http_types_future_result_option_own_trailers_error_code_drop_writable(trailers_writer);
  app_waitable_set_drop(wset);
}

app_callback_code_t exports_wasi_http_handler_handle(exports_wasi_http_handler_own_request_t request) {
  wasi_http_types_request_drop_own(request);
  compute();
  wasi_http_types_own_fields_t headers = wasi_http_types_constructor_fields();
  wasi_http_types_stream_u8_t body_reader = wasi_http_types_stream_u8_new(&body_writer);
  wasi_http_types_future_result_option_own_trailers_error_code_t trailers_reader =
      wasi_http_types_future_result_option_own_trailers_error_code_new(&trailers_writer);
  wasi_http_types_tuple2_own_response_future_result_void_error_code_t resp;
  wasi_http_types_static_response_new(headers, &body_reader, trailers_reader, &resp);
  wasi_http_types_future_result_void_error_code_drop_readable(resp.f1);
  exports_wasi_http_handler_result_own_response_error_code_t ret = { .is_err = false, .val = { .ok = resp.f0 } };
  exports_wasi_http_handler_handle_return(ret);

  app_waitable_status_t st = wasi_http_types_stream_u8_write(body_writer, (const uint8_t *)MSG, MSG_LEN);
  if (st == APP_WAITABLE_STATUS_BLOCKED) {
    wset = app_waitable_set_new();
    app_waitable_join(body_writer, wset);
    return APP_CALLBACK_CODE_WAIT(wset);
  }
  wset = app_waitable_set_new();
  finish();
  return APP_CALLBACK_CODE_EXIT;
}

app_callback_code_t exports_wasi_http_handler_handle_callback(app_event_t *event) {
  if (event->event == APP_EVENT_STREAM_WRITE) {
    app_waitable_join(body_writer, 0);
    finish();
    return APP_CALLBACK_CODE_EXIT;
  }
  return APP_CALLBACK_CODE_WAIT(wset);
}
