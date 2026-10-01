import { get } from 'wasi:config/store@0.2.0-rc.1';
import { open } from 'wasi:keyvalue/store@0.2.0-draft';

const dec = new TextDecoder();
const text = (v) => (v === undefined ? undefined : dec.decode(v));

async function handle(_req) {
  const greeting = get('greeting') ?? '(unset)';
  const bucket = open('');
  const seed = text(bucket.get('seed'));
  bucket.set('k', new TextEncoder().encode('v1'));
  const k = text(bucket.get('k'));
  return new Response(
    `config.greeting=${greeting} kv.seed=${JSON.stringify(seed)} kv.k=${JSON.stringify(k)}\n`);
}

addEventListener('fetch', (event) => event.respondWith(handle(event.request)));
