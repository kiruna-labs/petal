// minisign format + verification ALGORITHM for the plugin registry, with the
// crypto primitives injected so this file (like all of shared/) has no npm
// dependency. The web client binds @noble/* in web-harness/src/plugins/
// minisign.ts; the desktop verifies natively with the `minisign-verify`
// crate. One key signs for both: index.json and every bundle.json.
//
// Format (https://jedisct1.github.io/minisign/):
//   public key : base64( "Ed" | key_id[8] | pk[32] )            (42 bytes)
//   signature  : line 1 untrusted comment
//                line 2 base64( alg[2] | key_id[8] | sig[64] )  (74 bytes)
//                line 3 "trusted comment: ..."
//                line 4 base64( global_sig[64] ) over sig || trusted comment
//   alg "ED" = prehashed: sig is over blake2b-512(message) (what minisign
//   and `tauri signer` emit today); "Ed" = legacy, sig over the raw message.

export interface MinisignPrimitives {
  ed25519Verify(signature: Uint8Array, message: Uint8Array, publicKey: Uint8Array): boolean;
  blake2b512(data: Uint8Array): Uint8Array;
}

export interface MinisignSigningPrimitives extends MinisignPrimitives {
  ed25519Sign(message: Uint8Array, secretKey: Uint8Array): Uint8Array;
}

export interface MinisignPublicKey {
  keyId: Uint8Array;
  key: Uint8Array;
  /** Hex key id for messages. */
  keyIdHex: string;
}

export interface MinisignSignature {
  algorithm: 'ED' | 'Ed';
  keyId: Uint8Array;
  signature: Uint8Array;
  trustedComment: string;
  globalSignature: Uint8Array;
  untrustedComment: string;
}

export type MinisignVerdict = { ok: true; trustedComment: string } | { ok: false; reason: string };

export function fromBase64(text: string): Uint8Array {
  const bin = atob(text.trim());
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function toBase64(bytes: Uint8Array): string {
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

export function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

function same(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

/** Accepts the raw base64 or the two-line `.pub` file (comment + base64). */
export function parseMinisignPublicKey(text: string): MinisignPublicKey {
  const lines = text.split(/\r?\n/).map((l) => l.trim()).filter((l) => l.length > 0);
  const b64Line = lines.find((l) => !l.startsWith('untrusted comment:'));
  if (!b64Line) throw new Error('minisign public key: no key line');
  let raw: Uint8Array;
  try {
    raw = fromBase64(b64Line);
  } catch {
    throw new Error('minisign public key: not base64');
  }
  if (raw.length !== 42 || raw[0] !== 0x45 || raw[1] !== 0x64) throw new Error('minisign public key: not an Ed25519 minisign key');
  const keyId = raw.slice(2, 10);
  return { keyId, key: raw.slice(10, 42), keyIdHex: toHex(keyId) };
}

export function parseMinisignSignature(text: string): MinisignSignature {
  const lines = text.split(/\r?\n/).map((l) => l.trimEnd());
  while (lines.length && lines[lines.length - 1] === '') lines.pop();
  if (lines.length < 4) throw new Error('minisign signature: expected 4 lines');
  const [untrusted, sigB64, trustedLine, globalB64] = lines as [string, string, string, string];
  if (!trustedLine.startsWith('trusted comment: ')) throw new Error('minisign signature: missing trusted comment');
  const sig = fromBase64(sigB64);
  if (sig.length !== 74) throw new Error('minisign signature: bad length');
  const alg = String.fromCharCode(sig[0]!, sig[1]!);
  if (alg !== 'ED' && alg !== 'Ed') throw new Error(`minisign signature: unknown algorithm ${alg}`);
  const globalSignature = fromBase64(globalB64);
  if (globalSignature.length !== 64) throw new Error('minisign signature: bad global signature length');
  return {
    algorithm: alg,
    keyId: sig.slice(2, 10),
    signature: sig.slice(10, 74),
    trustedComment: trustedLine.slice('trusted comment: '.length),
    globalSignature,
    untrustedComment: untrusted.replace(/^untrusted comment:\s*/, ''),
  };
}

/** Verify `data` against a minisign signature file and public key using the given primitives. */
export function verifyMinisignWith(p: MinisignPrimitives, publicKey: MinisignPublicKey | string, signatureText: string, data: Uint8Array): MinisignVerdict {
  let pk: MinisignPublicKey;
  let sig: MinisignSignature;
  try {
    pk = typeof publicKey === 'string' ? parseMinisignPublicKey(publicKey) : publicKey;
    sig = parseMinisignSignature(signatureText);
  } catch (e) {
    return { ok: false, reason: (e as Error).message };
  }
  if (!same(pk.keyId, sig.keyId)) return { ok: false, reason: `signature key id ${toHex(sig.keyId)} does not match the registry key ${pk.keyIdHex}` };
  const message = sig.algorithm === 'ED' ? p.blake2b512(data) : data;
  let ok: boolean;
  try {
    ok = p.ed25519Verify(sig.signature, message, pk.key);
  } catch {
    ok = false;
  }
  if (!ok) return { ok: false, reason: 'signature does not match the content' };
  const trusted = new TextEncoder().encode(sig.trustedComment);
  const globalMessage = new Uint8Array(sig.signature.length + trusted.length);
  globalMessage.set(sig.signature, 0);
  globalMessage.set(trusted, sig.signature.length);
  let globalOk: boolean;
  try {
    globalOk = p.ed25519Verify(sig.globalSignature, globalMessage, pk.key);
  } catch {
    globalOk = false;
  }
  if (!globalOk) return { ok: false, reason: 'trusted comment was altered' };
  return { ok: true, trustedComment: sig.trustedComment };
}

/** Sign like `minisign -S` (prehashed "ED"). The marketplace publisher and fixture generator use this; the app never signs. */
export function signMinisignWith(
  p: MinisignSigningPrimitives,
  secretKey: Uint8Array,
  keyId: Uint8Array,
  data: Uint8Array,
  trustedComment: string,
  untrustedComment = 'signature from petal plugin registry',
): string {
  if (keyId.length !== 8) throw new Error('key id must be 8 bytes');
  const signature = p.ed25519Sign(p.blake2b512(data), secretKey);
  if (signature.length !== 64) throw new Error('ed25519 signature must be 64 bytes');
  const trusted = new TextEncoder().encode(trustedComment);
  const globalMessage = new Uint8Array(64 + trusted.length);
  globalMessage.set(signature, 0);
  globalMessage.set(trusted, 64);
  const globalSignature = p.ed25519Sign(globalMessage, secretKey);
  const sigBlob = new Uint8Array(74);
  sigBlob.set([0x45, 0x44], 0); // "ED"
  sigBlob.set(keyId, 2);
  sigBlob.set(signature, 10);
  return `untrusted comment: ${untrustedComment}\n${toBase64(sigBlob)}\ntrusted comment: ${trustedComment}\n${toBase64(globalSignature)}\n`;
}

export function encodeMinisignPublicKey(keyId: Uint8Array, publicKey: Uint8Array, comment = 'minisign public key'): string {
  if (keyId.length !== 8 || publicKey.length !== 32) throw new Error('key id must be 8 bytes and the public key 32');
  const raw = new Uint8Array(42);
  raw.set([0x45, 0x64], 0); // "Ed"
  raw.set(keyId, 2);
  raw.set(publicKey, 10);
  return `untrusted comment: ${comment}\n${toBase64(raw)}\n`;
}
