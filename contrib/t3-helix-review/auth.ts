// Local setup uses T3's CLI pairing control plane and public MCP OAuth API.
// Credentials are returned to the caller, never printed or persisted here.
import { execFile } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { promisify } from "node:util";
import { defaultT3BaseDir, discoverLocalEndpoint, readEnvironment, T3Error, validateEndpoint } from "./t3.ts";

export type T3Access = "read-only" | "approval-required" | "auto-accept-edits" | "auto" | "full-access";
export interface PairLocalClientOptions {
  baseDir?: string;
  endpoint?: string;
  runtimeStatePath?: string;
  environmentId?: string;
  cliPath?: string;
  access?: T3Access;
  timeoutMs?: number;
  fetch?: typeof fetch;
  /** Test seam; production executes argv directly, without a shell. */
  runCli?: (file: string, args: string[], timeoutMs: number) => Promise<string>;
}
export interface PairedT3Client { endpoint: string; authorization: string; expiresAt: string }
const execute = promisify(execFile);
const LABEL = "Helix review";
const LOOPBACK = new Set(["localhost", "127.0.0.1", "[::1]"]);
const ACCESSES = new Set<T3Access>(["read-only", "approval-required", "auto-accept-edits", "auto", "full-access"]);
const record = (value: unknown): value is Record<string, unknown> => typeof value === "object" && value !== null && !Array.isArray(value);
const nonempty = (value: unknown): value is string => typeof value === "string" && value.length > 0;

export async function pairLocalClient(options: PairLocalClientOptions = {}): Promise<PairedT3Client> {
  const baseDir = options.baseDir ?? defaultT3BaseDir();
  const endpoint = validateEndpoint(options.endpoint ?? await discoverLocalEndpoint(baseDir, options.runtimeStatePath));
  if (!LOOPBACK.has(endpoint.hostname) || endpoint.pathname !== "/mcp") throw new T3Error("configuration", "Local T3 pairing requires a loopback /mcp endpoint. Configure a persistent MCP token explicitly for remote servers.");
  const access = options.access ?? "full-access";
  if (!ACCESSES.has(access)) throw new T3Error("configuration", "Unsupported T3 MCP access mode.");
  const timeoutMs = options.timeoutMs ?? 30_000;
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) throw new T3Error("configuration", "T3 pairing timeout must be positive.");
  const fetchImplementation = options.fetch ?? fetch;
  if (options.environmentId !== undefined) {
    const actual = await readEnvironment(endpoint.href, { fetch: fetchImplementation, timeoutMs });
    if (actual.environmentId !== options.environmentId) throw new T3Error("environment_mismatch", "The local T3 server is not the selected environment. No pairing credential was created.");
  }
  const signal = AbortSignal.timeout(timeoutMs);
  const cliPath = options.cliPath ?? process.env.T3CODE_CLI_PATH ?? "t3";
  const runCli = options.runCli ?? (async (file, args, timeout) => {
    const { stdout } = await execute(file, args, { timeout, maxBuffer: 64 * 1024, encoding: "utf8", windowsHide: true });
    return stdout;
  });
  let pairingId: string | undefined;
  let pairingConsumed = false;

  const post = async (path: string, body: unknown, form = false): Promise<Record<string, unknown>> => {
    let response: Response;
    try {
      response = await fetchImplementation(new URL(path, endpoint), { method: "POST", redirect: "error", headers: { accept: "application/json", "content-type": form ? "application/x-www-form-urlencoded" : "application/json" }, body: form ? new URLSearchParams(body as Record<string, string>).toString() : JSON.stringify(body), signal });
    } catch { throw new T3Error("pairing_failed", "T3 pairing could not reach the local server or timed out."); }
    if (!response.ok) {
      await response.body?.cancel();
      throw new T3Error("pairing_failed", `T3 pairing request failed (HTTP ${response.status}). Check that the server and CLI use the same T3 data directory.`);
    }
    let value: unknown;
    try { value = await response.json(); } catch { throw new T3Error("pairing_failed", "T3 pairing returned an invalid response."); }
    if (!record(value)) throw new T3Error("pairing_failed", "T3 pairing returned an invalid response.");
    return value;
  };

  try {
    // The server returns the callback URL as data, so no browser/listener is needed.
    const redirectUri = "http://127.0.0.1/helix-review/callback";
    const registration = await post("/oauth/mcp/register", { client_name: LABEL, redirect_uris: [redirectUri], token_endpoint_auth_method: "none" });
    if (!nonempty(registration.client_id)) throw new T3Error("pairing_failed", "T3 did not register the Helix review client.");
    const verifier = randomBytes(32).toString("base64url");
    const state = randomBytes(24).toString("base64url");
    const authorization = { response_type: "code", client_id: registration.client_id, redirect_uri: redirectUri, code_challenge: createHash("sha256").update(verifier).digest("base64url"), code_challenge_method: "S256", state, resource: endpoint.href };
    const scopes = access === "read-only" ? ["orchestration:read"] : ["orchestration:read", "orchestration:operate", ...(access === "full-access" ? ["settings:write", "source-control:write"] : [])];
    let pairing: unknown;
    try {
      pairing = JSON.parse(await runCli(cliPath, ["auth", "pairing", "create", "--base-dir", baseDir, "--ttl", "2m", "--label", LABEL, "--json", ...scopes.flatMap((scope) => ["--scope", scope])], timeoutMs));
    } catch { throw new T3Error("pairing_failed", "The T3 CLI could not issue a local pairing code. Check the CLI path and T3 data directory."); }
    if (!record(pairing) || !nonempty(pairing.credential) || !nonempty(pairing.id)) throw new T3Error("pairing_failed", "The T3 CLI returned an invalid pairing response.");
    pairingId = pairing.id;
    const decision = await post("/oauth/mcp/decision", { authorization, decision: { _tag: "pairing-code", access, code: pairing.credential } });
    pairingConsumed = true;
    let redirect: URL;
    try { redirect = new URL(String(decision.redirectTo)); } catch { throw new T3Error("pairing_failed", "T3 returned an invalid OAuth redirect."); }
    const callback = new URL(redirectUri);
    if (redirect.origin !== callback.origin || redirect.pathname !== callback.pathname || redirect.hash || redirect.searchParams.get("state") !== state || redirect.searchParams.get("iss") !== endpoint.origin || redirect.searchParams.has("error") || !nonempty(redirect.searchParams.get("code"))) throw new T3Error("pairing_failed", "T3 OAuth returned an invalid callback, state, or issuer.");
    const token = await post("/oauth/mcp/token", { grant_type: "authorization_code", code: redirect.searchParams.get("code")!, redirect_uri: redirectUri, client_id: registration.client_id, code_verifier: verifier, resource: endpoint.href }, true);
    if (!nonempty(token.access_token) || /[\r\n]/u.test(token.access_token) || token.token_type !== "Bearer" || typeof token.expires_in !== "number" || !Number.isFinite(token.expires_in) || token.expires_in <= 0 || token.expires_in > 366 * 86400) throw new T3Error("pairing_failed", "T3 returned an invalid or expired MCP token.");
    return { endpoint: endpoint.href, authorization: `Bearer ${token.access_token}`, expiresAt: new Date(Date.now() + token.expires_in * 1000).toISOString() };
  } finally {
    // A failed exchange must not leave an unused local pairing grant behind.
    if (pairingId && !pairingConsumed) {
      await runCli(cliPath, ["auth", "pairing", "revoke", "--base-dir", baseDir, pairingId], Math.min(timeoutMs, 5_000)).catch(() => {});
    }
  }
}
