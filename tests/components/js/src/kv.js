// The `/kv` route, as kv.rs: wasi:keyvalue, with the same ops, in the same terms (see there). The reply is
// `{"ok": value}` or `{"err": "<the case of the error>"}`.
import store from "wasi:keyvalue/store@0.2.0-draft2";
import atomics from "wasi:keyvalue/atomics@0.2.0-draft2";
import batch from "wasi:keyvalue/batch@0.2.0-draft2";
import { debug, decode, encode } from "./util.js";

export function respond(name, q) {
  try {
    return { ok: run(name, q) };
  } catch (e) {
    return { err: debug(e) };
  }
}

/** Sets `key` of `name` to `value`. */
export const keep = (name, key, value) => store.open(name).set(key, encode(value));

const text = (v) => (v ? decode(v) : null);

/** Null when the swap went through, else the refreshed handle. */
function swap(cas, value) {
  try {
    atomics.swap(cas, value);
    return null;
  } catch (e) {
    if (e.payload.tag !== "cas-failed") throw e.payload.val; // a `store-error` of the error itself
    return e.payload.val;
  }
}

function run(name, q) {
  const arg = (k) => q.get(k) ?? "";
  const [key, value] = [arg("key"), encode(arg("value"))];
  const keys = arg("keys").split(",").filter(Boolean);
  const bucket = store.open(name);
  switch (arg("op")) {
    case "open":
      return null;
    case "get":
      return text(bucket.get(key));
    case "set":
      return bucket.set(key, value) ?? null;
    case "delete":
      return bucket.delete(key) ?? null;
    case "exists":
      return bucket.exists(key);
    case "list":
      return bucket.listKeys(q.get("cursor") ?? null);
    case "incr":
      return atomics.increment(bucket, key, Number(arg("delta") || 1));
    case "cas": {
      const cas = atomics.Cas.new(bucket, key);
      const seen = text(cas.current());
      if (q.has("between")) bucket.set(key, encode(q.get("between")));
      const latest = swap(cas, value);
      return latest ? { seen, swapped: false, latest: text(latest.current()) } : { seen, swapped: true };
    }
    case "rmw": {
      let retries = 0;
      for (let i = 0; i < Number(arg("n") || 1); i++) {
        let cas = atomics.Cas.new(bucket, key);
        for (;;) {
          const latest = swap(cas, encode(String((Number(text(cas.current())) || 0) + 1)));
          if (!latest) break;
          [cas, retries] = [latest, retries + 1];
        }
      }
      return retries;
    }
    case "get-many":
      return batch.getMany(bucket, keys).map(([k, v]) => [k, text(v)]);
    case "set-many":
      return batch.setMany(bucket, keys.map((k) => [k, value])) ?? null;
    case "delete-many":
      return batch.deleteMany(bucket, keys) ?? null;
    default:
      throw `unknown op ${JSON.stringify(arg("op"))}`;
  }
}
