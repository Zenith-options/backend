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

export interface ChainReadModel<T> { data: T; as_of_ledger: number; stale: boolean }
export interface SettlementStatus { id: string; status: "pending" | "submitted" | "confirmed" | "failed"; transaction_hash?: string }
export interface ChainTransaction { hash: string; status: string; ledger?: number; error?: string; submitted_at: number; updated_at: number }
export interface VaultBalance { wallet: string; free: string; locked: string }
export interface MarginRequirement { wallet: string; initial: string; maintenance: string; collateral: string }
export interface LiquidationRecord { id: string; wallet: string; reason: string; transaction_hash: string; ledger: number }
export interface UnsignedTransaction { envelope_xdr: string; network_passphrase: string; required_signers: string[]; valid_until_ledger: number }

export class SettlementClient {
  private readonly root: string;
  constructor(baseUrl: string, private readonly token: string, private readonly timeoutMs = 10_000) { this.root = baseUrl.replace(/\/+$/, ""); }
  private async get<T>(path: string): Promise<T> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.timeoutMs);
    try { return await readResponse<T>(await fetch(`${this.root}${path}`, { headers: { authorization: `Bearer ${this.token}` }, signal: controller.signal })); }
    finally { clearTimeout(timer); }
  }
  settlement(id: string) { return this.get<ChainReadModel<SettlementStatus>>(`/api/v1/chain/settlements/${encodeURIComponent(id)}`); }
  transaction(hash: string) { return this.get<ChainReadModel<ChainTransaction>>(`/api/v1/chain/transactions/${encodeURIComponent(hash)}`); }
  vault() { return this.get<ChainReadModel<VaultBalance>>("/api/v1/chain/vault"); }
  margin() { return this.get<ChainReadModel<MarginRequirement>>("/api/v1/chain/margin"); }
  liquidations(cursor?: string) { return this.get<ChainReadModel<LiquidationRecord[]>>(`/api/v1/chain/liquidations${cursor ? `?cursor=${encodeURIComponent(cursor)}` : ""}`); }
  subscribe(wallet: string, cursor?: string): WebSocket {
    const endpoint = new URL("/api/v1/chain/ws", this.root); endpoint.protocol = endpoint.protocol === "https:" ? "wss:" : "ws:"; endpoint.searchParams.set("wallet", wallet); endpoint.searchParams.set("token", this.token); if (cursor) endpoint.searchParams.set("cursor", cursor); return new WebSocket(endpoint);
  }
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
