// node --test cli/collect/web/
// Checks the browser code against values the Rust client encrypted (test-vectors.json, written by its tests).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ValueReader, importViewToken, parseViewToken } from "./qloudpulse-values.js";

const vectors = JSON.parse(readFileSync(new URL("./test-vectors.json", import.meta.url), "utf8"));
const OTHER_TOKEN = "qp_" + "A".repeat(43);

test("every value the client encrypted opens to its real value", async () => {
  const token = await importViewToken(vectors.token);
  assert.equal(token.keyId, vectors.key_id);
  for (const { plain, sealed } of vectors.values) {
    assert.equal(await token.open(sealed), plain);
  }
});

test("the reader replaces what it can read and leaves the rest", async () => {
  const reader = new ValueReader();
  const [a, b] = vectors.values.slice(1, 3);
  const text = `Name=${a.sealed}, Owner=${b.sealed}, Team=p_0123456789abcdef`;
  assert.deepEqual([...reader.neededKeyIds(text)], [[vectors.key_id, false]]);
  assert.equal(await reader.decryptText(text), text, "no token yet: unchanged");
  assert.equal(await reader.addToken(`  ${vectors.token}\n`), vectors.key_id);
  assert.deepEqual([...reader.neededKeyIds(text)], [[vectors.key_id, true]]);
  assert.equal(await reader.decryptText(text), `Name=${a.plain}, Owner=${b.plain}, Team=p_0123456789abcdef`);
  reader.removeToken(vectors.key_id);
  assert.equal(await reader.open(a.sealed), null, "removing the token forgets its values");
});

test("another token cannot read a value, and a changed value is refused", async () => {
  const other = await importViewToken(OTHER_TOKEN);
  const { sealed } = vectors.values[1];
  await assert.rejects(other.open(sealed), /encrypted with the view token/);
  const token = await importViewToken(vectors.token);
  const changed = sealed.slice(0, -1) + (sealed.endsWith("A") ? "B" : "A");
  await assert.rejects(token.open(changed));
  const moved = sealed.replace(vectors.key_id, "00000000");
  await assert.rejects(token.open(moved));
  const reader = new ValueReader();
  await reader.addToken(vectors.token);
  assert.equal(await reader.open(changed), null);
});

test("anything but a view token is refused", () => {
  for (const bad of ["", "qp_", vectors.token.slice(1), vectors.token.slice(0, -1), vectors.token + "A", "qpev_" + vectors.token.slice(3), "qp_" + "*".repeat(43)]) {
    assert.throws(() => parseViewToken(bad), undefined, bad);
  }
});
