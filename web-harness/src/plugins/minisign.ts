// The web client's crypto binding for the plugin registry: @noble/* behind
// the dependency-free algorithm in shared/plugin-host/minisign.ts. Also what
// the tests and the fixture generator use to SIGN (the app itself never signs).
import * as ed from '@noble/ed25519';
import { blake2b } from '@noble/hashes/blake2.js';
import { sha256, sha512 } from '@noble/hashes/sha2.js';
import {
  encodeMinisignPublicKey,
  signMinisignWith,
  toHex,
  verifyMinisignWith,
  type MinisignSigningPrimitives,
  type MinisignVerdict,
} from '@petal/shared/plugin-host/minisign';
import type { RegistryCrypto } from '@petal/shared/plugin-host/registry';

ed.hashes.sha512 = sha512;

export const noblePrimitives: MinisignSigningPrimitives = {
  ed25519Verify: (signature, message, publicKey) => ed.verify(signature, message, publicKey),
  ed25519Sign: (message, secretKey) => ed.sign(message, secretKey),
  blake2b512: (data) => blake2b(data, { dkLen: 64 }),
};

export function verifyMinisign(publicKeyText: string, signatureText: string, data: Uint8Array): MinisignVerdict {
  return verifyMinisignWith(noblePrimitives, publicKeyText, signatureText, data);
}

export function sha256Hex(data: Uint8Array): string {
  return toHex(sha256(data));
}

export const registryCrypto: RegistryCrypto = { verifyMinisign, sha256Hex };

/** Test/publisher helpers. */
export function generateMinisignKeypair(comment = 'minisign public key'): { secretKey: Uint8Array; keyId: Uint8Array; publicKeyText: string } {
  const secretKey = ed.utils.randomSecretKey();
  const keyId = crypto.getRandomValues(new Uint8Array(8));
  return { secretKey, keyId, publicKeyText: encodeMinisignPublicKey(keyId, ed.getPublicKey(secretKey), comment) };
}

export function signMinisign(secretKey: Uint8Array, keyId: Uint8Array, data: Uint8Array, trustedComment: string): string {
  return signMinisignWith(noblePrimitives, secretKey, keyId, data, trustedComment);
}
