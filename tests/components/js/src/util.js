// What QuickJS lacks: UTF-8 (it has no TextEncoder), strict where Rust's is lossy; and Rust's `{:?}` of a WIT error.
export const encode = (s) => Uint8Array.from(unescape(encodeURIComponent(s)), (c) => c.charCodeAt(0));
export const decode = (bytes) => decodeURIComponent(escape(Array.from(bytes, (b) => String.fromCharCode(b)).join("")));

/** The case of a WIT error, as Rust's Debug spells it, less its payload: `DNS-error` is `DnsError`. */
export function debug(e) {
  const tag = (e.payload ?? e).tag;
  return tag ? tag.toLowerCase().replace(/(?:^|-)(.)/g, (_, c) => c.toUpperCase()) : String(e);
}
