// SuiDrop — Sui on-chain layer (browser wallet signing + receipt verification).
// No bundler: everything is ESM loaded straight from a CDN.
//
// - Signing/execution is done by the user's wallet (Slush, via wallet-standard).
// - Reads/verification go through OUR backend /api/rpc, which proxies to Tatum
//   (rate-limited), so Tatum RPC does real, recurring work.

// Loaded through our backend /esm proxy (same-origin) to avoid CDN CORS/MIME
// issues. Versions pinned: @mysten/sui 2.x, @mysten/wallet-standard 0.20.x.
import { getWallets } from "/esm/@mysten/wallet-standard@0.20.3?target=es2022";
import { Transaction } from "/esm/@mysten/sui@2.17.0/transactions?target=es2022";

const CLOCK_ID = "0x6"; // shared sui::clock::Clock object

/* ---------------- wallet ---------------- */

/** Find a Sui-capable wallet (Slush, Nightly, etc.); prefer Slush if present. */
export function findSuiWallet(preferred = "slush") {
  const sui = getWallets()
    .get()
    .filter((w) => {
      const isSui = (w.chains || []).some((c) => c.startsWith("sui:"));
      const canSign =
        w.features["sui:signAndExecuteTransaction"] ||
        w.features["sui:signAndExecuteTransactionBlock"];
      return isSui && canSign;
    });
  return (
    sui.find((w) => w.name.toLowerCase().includes(preferred)) || sui[0] || null
  );
}

/** Connect and return the first account. */
export async function connect(wallet) {
  await wallet.features["standard:connect"].connect();
  const account = wallet.accounts[0];
  if (!account) throw new Error("Wallet returned no account");
  return account;
}

/* ---------------- write: mint receipt ---------------- */

/**
 * Build + sign + execute the create_receipt move call via the wallet.
 * Returns the transaction digest.
 */
export async function createReceipt({
  wallet,
  account,
  packageId,
  chain,
  blobId,
  recipient,
  size,
  nameHash, // Uint8Array
  expiryEpochs,
}) {
  const tx = new Transaction();
  tx.moveCall({
    target: `${packageId}::receipt::create_receipt`,
    arguments: [
      tx.pure.string(blobId),
      tx.pure.address(recipient),
      tx.pure.u64(BigInt(size)),
      tx.pure.vector("u8", Array.from(nameHash)),
      tx.pure.u64(BigInt(expiryEpochs)),
      tx.object(CLOCK_ID),
    ],
  });

  // Prefer the v2 feature; fall back to v1 for older wallet builds.
  const v2 = wallet.features["sui:signAndExecuteTransaction"];
  const v1 = wallet.features["sui:signAndExecuteTransactionBlock"];
  if (v2) {
    const res = await v2.signAndExecuteTransaction({ transaction: tx, account, chain });
    return res.digest;
  }
  if (v1) {
    const res = await v1.signAndExecuteTransactionBlock({ transactionBlock: tx, account, chain });
    return res.digest;
  }
  throw new Error("Wallet does not support signing Sui transactions");
}

/* ---------------- read: via Tatum RPC proxy ---------------- */

/** Call Sui JSON-RPC through our throttled Tatum proxy. */
export async function rpc(method, params) {
  const res = await fetch("/api/rpc", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ id: 1, jsonrpc: "2.0", method, params }),
  });
  const j = await res.json();
  if (j.error) throw new Error(j.error.message || "RPC error");
  return j.result;
}

/**
 * Resolve the created DropReceipt object id from a tx digest.
 * Retries because the indexer may lag a beat behind execution.
 */
export async function receiptIdFromTx(digest, { retries = 6, delayMs = 1500 } = {}) {
  for (let i = 0; i < retries; i++) {
    try {
      const tx = await rpc("sui_getTransactionBlock", [
        digest,
        { showObjectChanges: true, showEffects: true },
      ]);
      const created = (tx?.objectChanges || []).find(
        (c) => c.type === "created" && (c.objectType || "").includes("::receipt::DropReceipt"),
      );
      if (created) return created.objectId;
    } catch (_) {
      /* keep retrying */
    }
    await new Promise((r) => setTimeout(r, delayMs));
  }
  return null;
}

/** Fetch a receipt object's fields (the on-chain proof) via Tatum RPC. */
export async function fetchReceipt(objectId) {
  const obj = await rpc("sui_getObject", [
    objectId,
    { showContent: true, showOwner: true },
  ]);
  return obj?.data?.content?.fields || null;
}

/* ---------------- helpers ---------------- */

export const ZERO_ADDR = "0x0000000000000000000000000000000000000000000000000000000000000000";

/** SHA-256 of a string -> Uint8Array (used to hash the filename). */
export async function sha256(text) {
  const buf = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return new Uint8Array(buf);
}

export function bytesToHex(bytes) {
  return Array.from(bytes).map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** SuiVision explorer base for the active network. */
export function explorerBase(network) {
  if (network === "mainnet") return "https://suivision.xyz";
  return `https://${network}.suivision.xyz`;
}
