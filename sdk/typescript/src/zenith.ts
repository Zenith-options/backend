export interface WalletSigner {
  walletAddress: string;
  signMessage(message: string): Promise<string>;
}

export interface WalletSession {
  token: string;
  wallet_address: string;
}

export interface SpotSnapshot {
  prices: Record<string, number>;
  vols: Record<string, number>;
}

export class ZenithApiError extends Error {
  constructor(
    message: string,
    public readonly code?: string,
    public readonly requestId?: string,
  ) {
    super(message);
    this.name = "ZenithApiError";
  }
}

async function readResponse<T>(response: Response): Promise<T> {
  const payload: unknown = await response.json();
  if (!response.ok) {
    const error = (payload as { error?: string | { code?: string; message?: string; request_id?: string } }).error;
    if (typeof error === "string") {
      throw new ZenithApiError(error);
    }
    throw new ZenithApiError(
      error?.message ?? `Zenith API request failed with HTTP ${response.status}`,
      error?.code,
      error?.request_id,
    );
  }
  return payload as T;
}

export async function signInWithWallet(
  baseUrl: string,
  wallet: WalletSigner,
): Promise<WalletSession> {
  const root = baseUrl.replace(/\/+$/, "");
  const nonce = await readResponse<{ nonce: string; message: string }>(
    await fetch(`${root}/api/v1/auth/nonce`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ wallet_address: wallet.walletAddress }),
    }),
  );
  const signature = await wallet.signMessage(nonce.message);
  return readResponse<WalletSession>(
    await fetch(`${root}/api/v1/auth/verify`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        wallet_address: wallet.walletAddress,
        message: nonce.message,
        signature,
      }),
    }),
  );
}

export function subscribeToSpot(
  baseUrl: string,
  onSnapshot: (snapshot: SpotSnapshot) => void,
  onError: (error: Error) => void,
): WebSocket {
  const endpoint = new URL("/api/v1/ws/spot", baseUrl);
  endpoint.protocol = endpoint.protocol === "https:" ? "wss:" : "ws:";
  const socket = new WebSocket(endpoint);
  socket.addEventListener("message", (event: MessageEvent<string>) => {
    try {
      onSnapshot(JSON.parse(event.data) as SpotSnapshot);
    } catch (error) {
      onError(error instanceof Error ? error : new Error(String(error)));
    }
  });
  socket.addEventListener("error", () => onError(new Error("Zenith spot WebSocket failed")));
  return socket;
}

// ─── Merkle-commitment batch settlement (#196) ──────────────────────────────
//
// Lifecycle: plan a batch (`PLANNED`) → commit the root with a single signed
// transaction (`COMMITTED`, leaf set locked) → claim (fully or partially)
// against the batch. Every amount is in stroops (1e-7 of the payout asset),
// matching the backend's Money/Qty convention.

export type SettlementBatchStatus = "planned" | "committed" | "finalized";

export interface SettlementInstructionInput {
  position_id: string;
  account: string;
  /** Payout amount in stroops (amount * 1e7). */
  amount_scaled: number;
}

export interface SettlementBatch {
  batch_id: string;
  payout_asset: string;
  status: SettlementBatchStatus;
  root: string | null;
  leaf_count: number | null;
  leaf_version: number;
  total_scaled: number;
  available_scaled: number;
  committed_tx_hash: string | null;
  created_at: string;
  committed_at: string | null;
  finalized_at: string | null;
}

export interface SettlementLeaf {
  position_id: string;
  account: string;
  amount_scaled: number;
  claimed_scaled: number;
  leaf_hash: string;
}

export interface SettlementProofStep {
  sibling: string;
  sibling_is_right: boolean;
}

export interface SettlementProof {
  batch_id: string;
  position_id: string;
  account: string;
  leaf_version: number;
  leaf: string;
  root: string;
  proof: SettlementProofStep[];
}

export interface SettlementClaim {
  batch_id: string;
  position_id: string;
  account: string;
  amount_scaled: number;
  claimed_total_scaled: number;
  outstanding_scaled: number;
  idempotent_replay: boolean;
}

function settlementRoot(baseUrl: string): string {
  return `${baseUrl.replace(/\/+$/, "")}/api/v1/settlement/batches`;
}

function authed(token: string): HeadersInit {
  return { "content-type": "application/json", authorization: `Bearer ${token}` };
}

/** Plans a batch. Fails with code `invalid_batch` on a conservation breach. */
export async function planSettlementBatch(
  baseUrl: string,
  token: string,
  payoutAsset: string,
  totalScaled: number,
  instructions: SettlementInstructionInput[],
): Promise<SettlementBatch> {
  return readResponse<SettlementBatch>(
    await fetch(settlementRoot(baseUrl), {
      method: "POST",
      headers: authed(token),
      body: JSON.stringify({
        payout_asset: payoutAsset,
        total_scaled: totalScaled,
        instructions,
      }),
    }),
  );
}

/** Commits the root on chain in a single signed transaction, locking leaves. */
export async function commitSettlementBatch(
  baseUrl: string,
  token: string,
  batchId: string,
  commitmentTxHash: string,
  commitmentLedger?: number,
): Promise<SettlementBatch> {
  return readResponse<SettlementBatch>(
    await fetch(`${settlementRoot(baseUrl)}/${batchId}/commit`, {
      method: "POST",
      headers: authed(token),
      body: JSON.stringify({
        commitment_tx_hash: commitmentTxHash,
        commitment_ledger: commitmentLedger,
      }),
    }),
  );
}

export async function getSettlementBatch(
  baseUrl: string,
  token: string,
  batchId: string,
): Promise<SettlementBatch> {
  return readResponse<SettlementBatch>(
    await fetch(`${settlementRoot(baseUrl)}/${batchId}`, {
      headers: authed(token),
    }),
  );
}

export async function listSettlementLeaves(
  baseUrl: string,
  token: string,
  batchId: string,
): Promise<SettlementLeaf[]> {
  return readResponse<SettlementLeaf[]>(
    await fetch(`${settlementRoot(baseUrl)}/${batchId}/leaves`, {
      headers: authed(token),
    }),
  );
}

/** Fetches the Merkle inclusion proof for one position's payout. */
export async function getSettlementProof(
  baseUrl: string,
  token: string,
  batchId: string,
  positionId: string,
): Promise<SettlementProof> {
  return readResponse<SettlementProof>(
    await fetch(`${settlementRoot(baseUrl)}/${batchId}/leaves/${positionId}/proof`, {
      headers: authed(token),
    }),
  );
}

/**
 * Claims (fully or partially) against a committed batch. Reusing an
 * `idempotencyKey` returns the original claim with `idempotent_replay: true`
 * instead of moving money again.
 */
export async function claimSettlement(
  baseUrl: string,
  token: string,
  batchId: string,
  positionId: string,
  amountScaled: number,
  idempotencyKey: string,
): Promise<SettlementClaim> {
  return readResponse<SettlementClaim>(
    await fetch(`${settlementRoot(baseUrl)}/${batchId}/claims`, {
      method: "POST",
      headers: authed(token),
      body: JSON.stringify({
        position_id: positionId,
        amount_scaled: amountScaled,
        idempotency_key: idempotencyKey,
      }),
    }),
  );
}
