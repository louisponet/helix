import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import { pairLocalClient } from "./auth.ts";

const endpoint = "http://127.0.0.1:3773/mcp";
const pairingSecret = "secret-one-time-pairing";
const bearerSecret = "secret-persistent-bearer";
type Body = Record<string, any>;
function harness(options: { badState?: boolean; badIssuer?: boolean; badRedirect?: boolean; decisionStatus?: number; invalidToken?: boolean } = {}) {
  const cliCalls: { file: string; args: string[] }[] = [];
  const httpCalls: { url: URL; body: Body; headers: Headers }[] = [];
  let authorization: Body;
  const runCli = async (file: string, args: string[]) => {
    cliCalls.push({ file, args });
    if (args[2] === "revoke") return "Revoked";
    return JSON.stringify({ id: "pairing-1", credential: pairingSecret });
  };
  const fetchImplementation = (async (url: URL, init: RequestInit) => {
    const headers = new Headers(init.headers);
    const body = headers.get("content-type") === "application/json" ? JSON.parse(String(init.body)) : Object.fromEntries(new URLSearchParams(String(init.body)));
    httpCalls.push({ url, body, headers });
    assert.equal(init.redirect, "error");
    assert.equal(url.origin, new URL(endpoint).origin);
    switch (url.pathname) {
      case "/oauth/mcp/register":
        assert.deepEqual(body.redirect_uris, ["http://127.0.0.1/helix-review/callback"]);
        return Response.json({ client_id: "registered-client-id" }, { status: 201 });
      case "/oauth/mcp/decision": {
        if (options.decisionStatus) return new Response(pairingSecret, { status: options.decisionStatus });
        assert.equal(body.decision._tag, "pairing-code");
        assert.equal(body.decision.code, pairingSecret);
        authorization = body.authorization;
        assert.equal(authorization.client_id, "registered-client-id");
        assert.equal(authorization.code_challenge_method, "S256");
        assert.match(authorization.code_challenge, /^[A-Za-z0-9_-]{43}$/u);
        const redirect = new URL(options.badRedirect ? "http://evil.example/callback" : authorization.redirect_uri);
        redirect.searchParams.set("code", "exchange-code");
        redirect.searchParams.set("state", options.badState ? "wrong" : authorization.state);
        redirect.searchParams.set("iss", options.badIssuer ? "http://other-server" : url.origin);
        return Response.json({ redirectTo: redirect.href });
      }
      case "/oauth/mcp/token":
        assert.equal(headers.get("content-type"), "application/x-www-form-urlencoded");
        assert.equal(body.grant_type, "authorization_code");
        assert.equal(body.code, "exchange-code");
        assert.equal(body.client_id, authorization.client_id);
        assert.equal(body.resource, endpoint);
        assert.equal(createHash("sha256").update(body.code_verifier).digest("base64url"), authorization.code_challenge);
        return Response.json({ access_token: bearerSecret, token_type: "Bearer", expires_in: options.invalidToken ? -1 : 86400 });
      default: throw new Error("Unexpected endpoint");
    }
  }) as typeof fetch;
  return { runCli, fetch: fetchImplementation, cliCalls, httpCalls };
}

test("pairs without a browser using CLI grant, PKCE, matching state and issuer", async () => {
  const stub = harness();
  const before = Date.now();
  const paired = await pairLocalClient({ endpoint, baseDir: "/test t3 home", cliPath: "/test t3 cli", runCli: stub.runCli, fetch: stub.fetch });
  assert.equal(paired.endpoint, endpoint);
  assert.equal(paired.authorization, `Bearer ${bearerSecret}`);
  assert.ok(Date.parse(paired.expiresAt) >= before + 86400 * 1000);
  assert.equal(stub.cliCalls.length, 1);
  assert.equal(stub.cliCalls[0].file, "/test t3 cli");
  assert.deepEqual(stub.cliCalls[0].args, ["auth", "pairing", "create", "--base-dir", "/test t3 home", "--ttl", "2m", "--label", "Helix review", "--json", "--scope", "orchestration:read", "--scope", "orchestration:operate", "--scope", "settings:write", "--scope", "source-control:write"]);
  assert.equal(stub.httpCalls[1].body.decision.access, "full-access");
});

test("narrow access requests only required scopes", async () => {
  const stub = harness();
  await pairLocalClient({ endpoint, access: "approval-required", runCli: stub.runCli, fetch: stub.fetch });
  assert.equal(stub.httpCalls[1].body.decision.access, "approval-required");
  assert.ok(!stub.cliCalls[0].args.includes("settings:write"));
  assert.ok(!stub.cliCalls[0].args.includes("source-control:write"));
});

test("refuses remote endpoints and credential-bearing URLs before local grant creation", async () => {
  for (const url of ["https://example.com/mcp", "http://127.0.0.1.evil.example/mcp", "http://user:pass@127.0.0.1/mcp", "http://127.0.0.1/not-mcp"]) {
    const stub = harness();
    await assert.rejects(pairLocalClient({ endpoint: url, runCli: stub.runCli, fetch: stub.fetch }), { code: "configuration" });
    assert.equal(stub.cliCalls.length, 0);
    assert.equal(stub.httpCalls.length, 0);
  }
});

test("never exchanges authorization codes for an unexpected callback, state or issuer", async () => {
  for (const options of [{ badState: true }, { badIssuer: true }, { badRedirect: true }]) {
    const stub = harness(options);
    await assert.rejects(pairLocalClient({ endpoint, runCli: stub.runCli, fetch: stub.fetch }), { code: "pairing_failed" });
    assert.equal(stub.httpCalls.length, 2);
  }
});

test("revokes unused pairing grants after decision failure without exposing secret responses", async () => {
  const stub = harness({ decisionStatus: 400 });
  await assert.rejects(pairLocalClient({ endpoint, baseDir: "/test home", runCli: stub.runCli, fetch: stub.fetch }), (error: unknown) => error instanceof Error && !error.message.includes(pairingSecret));
  assert.deepEqual(stub.cliCalls[1].args, ["auth", "pairing", "revoke", "--base-dir", "/test home", "pairing-1"]);
});

test("rejects an expired token and hides CLI credentials on failure", async () => {
  const stub = harness({ invalidToken: true });
  await assert.rejects(pairLocalClient({ endpoint, runCli: stub.runCli, fetch: stub.fetch }), { code: "pairing_failed" });
  await assert.rejects(pairLocalClient({ endpoint, runCli: async () => { throw new Error(pairingSecret); }, fetch: stub.fetch }), (error: unknown) => error instanceof Error && !error.message.includes(pairingSecret));
});

test("checks selected environment identity before creating any local pairing grant", async () => {
  const stub = harness();
  const fetchImplementation = (async (url: URL, init: RequestInit) => {
    assert.equal(url.pathname, "/.well-known/t3/environment");
    assert.equal(new Headers(init.headers).get("authorization"), null);
    return Response.json({ environmentId: "live-server" });
  }) as typeof fetch;
  await assert.rejects(pairLocalClient({ endpoint, environmentId: "dev-server", fetch: fetchImplementation, runCli: stub.runCli }), { code: "environment_mismatch" });
  assert.equal(stub.cliCalls.length, 0);
});
