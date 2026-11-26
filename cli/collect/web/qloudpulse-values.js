// Reads qloudpulse-collect's encrypted values (`qpe1_<key id>_<data>`) in the browser, with the view token (`qp_…`)
// the customer's client created. This is the code qloudpulse's web UI runs when someone enters a view token: it is
// published here so you can check that the token and the real values stay in the browser.
//
// - It uses WebCrypto alone and makes no network request.
// - The token's keys are imported as non-extractable CryptoKeys; the token text itself is not kept.
// - A caller should keep the token in memory (or sessionStorage at most), never in a cookie, a URL, localStorage or a
//   request, and never log it.
//
// The format is described in `cli/collect/src/view_token.rs`, which encrypts; `test-vectors.json`, written by that
// code's tests, is what `qloudpulse-values.test.mjs` checks this file against.

const LABEL = "qloudpulse-collect/v1\0";
const TOKEN_PREFIX = "qp_";
const VALUE_PREFIX = "qpe1_";

/** Every encrypted value in a text: the key id is group 1. The data is at least 44 bytes, 59 base64url characters. */
export const VALUE_PATTERN = /qpe1_([0-9a-f]{8})_([A-Za-z0-9_-]{59,})/g;

const utf8 = new TextEncoder();

function base64urlDecode(text) {
  if (!/^[A-Za-z0-9_-]*$/.test(text) || text.length % 4 === 1) {
    throw new Error("not base64url");
  }
  const b64 = text.replace(/-/g, "+").replace(/_/g, "/") + "===".slice((text.length + 3) % 4);
  const binary = atob(b64);
  const out = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
  return out;
}

function hex(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

async function hmac(keyBytes, message) {
  const key = await crypto.subtle.importKey("raw", keyBytes, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  return new Uint8Array(await crypto.subtle.sign("HMAC", key, message));
}

function concat(a, b) {
  const out = new Uint8Array(a.length + b.length);
  out.set(a);
  out.set(b, a.length);
  return out;
}

/** The 32 secret bytes of a view token. Throws on anything but `qp_` and 43 base64url characters. */
export function parseViewToken(text) {
  const trimmed = String(text).trim();
  if (!trimmed.startsWith(TOKEN_PREFIX)) throw new Error("a view token starts with qp_");
  const body = trimmed.slice(TOKEN_PREFIX.length);
  if (body.length !== 43) throw new Error("a view token is qp_ and 43 characters");
  const secret = base64urlDecode(body);
  if (secret.length !== 32) throw new Error("a view token holds 32 bytes");
  return secret;
}

/** The keys behind one view token. `open` throws when the value is not this token's or was changed. */
export async function importViewToken(text) {
  const secret = parseViewToken(text);
  const derive = (label) => hmac(secret, utf8.encode(LABEL + label));
  const [aesBytes, nonceBytes, idBytes] = await Promise.all([derive("value-key"), derive("value-nonce"), derive("key-id")]);
  secret.fill(0);
  const keyId = hex(idBytes.slice(0, 4));
  const aes = await crypto.subtle.importKey("raw", aesBytes, { name: "AES-GCM" }, false, ["decrypt"]);
  const nonceKey = await crypto.subtle.importKey("raw", nonceBytes, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  aesBytes.fill(0);
  nonceBytes.fill(0);
  const prefix = `${VALUE_PREFIX}${keyId}_`;
  const aad = utf8.encode(prefix);

  async function open(sealed) {
    if (!sealed.startsWith(prefix)) {
      const id = sealed.startsWith(VALUE_PREFIX) ? sealed.slice(VALUE_PREFIX.length, VALUE_PREFIX.length + 8) : null;
      throw new Error(id ? `encrypted with the view token ${id}, not ${keyId}` : "not an encrypted value");
    }
    const data = base64urlDecode(sealed.slice(prefix.length));
    if (data.length < 12 + 16 + 16) throw new Error("not an encrypted value");
    const nonce = data.slice(0, 12);
    let padded;
    try {
      padded = new Uint8Array(
        await crypto.subtle.decrypt({ name: "AES-GCM", iv: nonce, additionalData: aad, tagLength: 128 }, aes, data.slice(12)),
      );
    } catch {
      throw new Error("the value was changed, or the token is not the one it was encrypted with");
    }
    let end = padded.length - 1;
    while (end >= 0 && padded[end] === 0) end--;
    if (end < 0 || padded[end] !== 0x80 || padded.length - end > 16) throw new Error("bad padding");
    const value = padded.slice(0, end);
    const check = new Uint8Array(await crypto.subtle.sign("HMAC", nonceKey, value)).slice(0, 12);
    if (hex(check) !== hex(nonce)) throw new Error("the value's nonce does not match it");
    return new TextDecoder("utf-8", { fatal: true }).decode(value);
  }

  return { keyId, open };
}

/**
 * Holds the view tokens a person entered (one per key id, as a rotated token leaves older values under the old one)
 * and decrypts what it can. Values under a key id it has no token for are left as they are.
 */
export class ValueReader {
  #tokens = new Map();
  #cache = new Map();

  /** Adds a token; returns its key id. Throws when the text is not a view token. */
  async addToken(text) {
    const token = await importViewToken(text);
    this.#tokens.set(token.keyId, token);
    return token.keyId;
  }

  removeToken(keyId) {
    this.#tokens.delete(keyId);
    for (const k of this.#cache.keys()) if (k.startsWith(`${VALUE_PREFIX}${keyId}_`)) this.#cache.delete(k);
  }

  /** Forgets every token and every decrypted value. */
  clear() {
    this.#tokens.clear();
    this.#cache.clear();
  }

  keyIds() {
    return [...this.#tokens.keys()];
  }

  /** The key ids a text's encrypted values need, and whether a token for each was entered. */
  neededKeyIds(text) {
    const out = new Map();
    for (const m of String(text).matchAll(VALUE_PATTERN)) out.set(m[1], this.#tokens.has(m[1]));
    return out;
  }

  /** The real value, or null when no token entered can read it. */
  async open(sealed) {
    if (this.#cache.has(sealed)) return this.#cache.get(sealed);
    const keyId = sealed.slice(VALUE_PREFIX.length, VALUE_PREFIX.length + 8);
    const token = sealed.startsWith(VALUE_PREFIX) ? this.#tokens.get(keyId) : undefined;
    if (!token) return null;
    let value = null;
    try {
      value = await token.open(sealed);
    } catch {
      value = null;
    }
    this.#cache.set(sealed, value);
    return value;
  }

  /** The text with every encrypted value it can read replaced by the real value; the rest is left as it is. */
  async decryptText(text) {
    const s = String(text);
    const found = [...new Set(Array.from(s.matchAll(VALUE_PATTERN), (m) => m[0]))];
    const opened = new Map(await Promise.all(found.map(async (v) => [v, await this.open(v)])));
    return s.replace(VALUE_PATTERN, (m) => opened.get(m) ?? m);
  }
}
