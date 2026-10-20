import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile, symlink, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { connectionFromEnvironment, discoverLocalEndpoint, readEnvironment, T3Client, T3Error } from "./t3.ts";

const endpoint = "http://127.0.0.1:3773/mcp";
const token = "test-credential-do-not-log";
type Request = { id?: number; method: string; params?: { name: string; arguments: Record<string, unknown> } };
function server(handler: (name: string, args: Record<string, unknown>) => unknown, options: { sse?: boolean } = {}) {
  const requests: Request[] = [];
  const headers: Headers[] = [];
  const fetchImplementation = (async (_url: unknown, init: RequestInit) => {
    assert.equal(_url, endpoint);
    const request = JSON.parse(String(init.body)) as Request;
    requests.push(request);
    headers.push(new Headers(init.headers));
    assert.equal(init.redirect, "error");
    assert.equal(new Headers(init.headers).get("authorization"), `Bearer ${token}`);
    if (request.method === "notifications/initialized") return new Response(null, { status: 202 });
    const result = request.method === "initialize" ? { protocolVersion: "2025-06-18" } : { content: [], structuredContent: await handler(request.params!.name, request.params!.arguments) };
    const envelope = { jsonrpc: "2.0", id: request.id, result };
    if (options.sse) {
      const encoder = new TextEncoder();
      const source = `: keepalive\r\n\r\ndata: ${JSON.stringify({ jsonrpc: "2.0", method: "notifications/message", params: {} })}\r\n\r\ndata: ${JSON.stringify(envelope)}\r\n\r\n`;
      const bytes = encoder.encode(source);
      let offset = 0;
      return new Response(new ReadableStream({ pull(controller) { if (offset >= bytes.length) controller.close(); else controller.enqueue(bytes.slice(offset, offset += 7)); } }), { headers: { "content-type": "text/event-stream", "mcp-session-id": "session-1" } });
    }
    return Response.json(envelope, { headers: { "mcp-session-id": "session-1" } });
  }) as typeof fetch;
  return { client: new T3Client({ endpoint, authorization: token, fetch: fetchImplementation }), requests, headers };
}
const thread = (workspace: string | null, extra = {}) => ({ threadId: "thread-a", projectId: "project-a", title: "Review 🔎", status: "idle", activeRunId: null, latestRunId: null, branch: null, worktreePath: workspace, archived: false, settled: false, snoozed: false, ...extra });

test("reads canonical worktree and changing lifecycle metadata through JSON MCP", async () => {
  const root = await mkdtemp(join(tmpdir(), "hx-t3-"));
  try {
    const checkout = join(root, "checkout");
    await mkdir(checkout);
    await symlink(checkout, join(root, "alias"));
    let archived = false;
    const { client, requests, headers } = server((name) => {
      assert.equal(name, "t3_thread_read");
      return { thread: thread(join(root, "alias"), { archived }), recentRuns: [] };
    });
    assert.equal((await client.readThread("thread-a")).cwd, checkout);
    archived = true;
    assert.equal((await client.readThread("thread-a")).archived, true);
    assert.deepEqual(requests.map((r) => r.method), ["initialize", "notifications/initialized", "tools/call", "tools/call"]);
    assert.equal(headers[2].get("mcp-session-id"), "session-1");
    assert.equal(headers[2].get("mcp-protocol-version"), "2025-06-18");
    assert.equal(requests[2].params!.arguments.maxCharsPerItem, 1);
    assert.equal(requests[2].params!.arguments.afterPosition, Number.MAX_SAFE_INTEGER);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("resolves main checkout from project every read so workspace moves are visible", async () => {
  const root = await mkdtemp(join(tmpdir(), "hx-t3-"));
  try {
    const next = join(root, "next");
    await mkdir(next);
    let workspace = root;
    const { client } = server((name) => name === "t3_thread_read" ? { thread: thread(null), recentRuns: [] } : { id: "project-a", title: "Project", workspaceRoot: workspace, deletedAt: null });
    assert.equal((await client.readThread("thread-a")).cwd, root);
    workspace = next;
    assert.equal((await client.readThread("thread-a")).cwd, next);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("reads the attached PR but never interprets another host's URL shape as GitHub", async () => {
  const root = await mkdtemp(join(tmpdir(), "hx-t3-"));
  try {
    let linkedPullRequest = { url: "https://github.com/team/project/pull/7", repository: "team/project", number: 7 };
    const { client } = server(() => ({ thread: thread(root, { linkedPullRequest }), recentRuns: [] }));
    assert.deepEqual((await client.readThread("thread-a")).linkedPullRequest, linkedPullRequest);
    linkedPullRequest = { ...linkedPullRequest, url: "https://gitlab.com/team/project/-/merge_requests/7" };
    assert.equal((await client.readThread("thread-a")).linkedPullRequest, undefined);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("streaming SSE ignores notifications, handles split UTF-8 and CRLF, and negotiates once", async () => {
  const { client, requests } = server((name, args) => {
    assert.equal(name, "t3_thread_list");
    assert.equal(args.projectId, "project-a");
    return { projectId: "project-a", currentThreadId: "thread-a", threads: [{ threadId: "thread-a", title: "Review 🔎", status: "running" }], nextCursor: null, total: 1 };
  }, { sse: true });
  const [a, b] = await Promise.all([client.listThreads({ projectId: "project-a" }), client.listThreads({ projectId: "project-a" })]);
  assert.equal(a.threads[0].title, "Review 🔎");
  assert.equal(b.currentThreadId, "thread-a");
  assert.equal(requests.filter((r) => r.method === "initialize").length, 1);
});

test("delegates start/steer/queue selection to T3 and preserves id across retries", async () => {
  for (const delivery of ["started", "steered", "queued"]) {
    const { client, requests } = server((name, args) => {
      assert.equal(name, "t3_thread_send");
      assert.deepEqual(args, { threadId: "thread-a", message: "User review", mode: "auto", clientRequestId: "handoff-fixed-id" });
      return { threadId: "thread-a", runId: "run-1", messageId: "message-1", status: "running", delivery };
    });
    assert.equal((await client.send("thread-a", "User review", "handoff-fixed-id")).delivery, delivery);
    await client.send("thread-a", "User review", "handoff-fixed-id");
    assert.equal(requests.filter((r) => r.params?.name === "t3_thread_read").length, 0);
  }
});

test("preserves explicit queue delivery for a separate commit follow-up", async () => {
  const { client } = server((name, args) => {
    assert.equal(name, "t3_thread_send");
    assert.equal(args.mode, "queue");
    assert.equal(args.clientRequestId, "commit-fixed-id");
    return { threadId: "thread-a", runId: "run-queued", messageId: "message-queued", status: "queued", delivery: "queued" };
  });
  assert.equal((await client.send("thread-a", "Commit reviewed changes", "commit-fixed-id", "queue")).delivery, "queued");
});

test("surfaces deleted threads and redacts credentials from server errors", async () => {
  const { client } = server(() => ({ _tag: "OrchestratorMcpFailure", code: "thread_not_found", message: `Missing. Bearer ${token}` }));
  await assert.rejects(client.readThread("deleted"), (error: unknown) => error instanceof T3Error && error.code === "thread_not_found" && !error.message.includes(token));
});

test("supports MCP JSON text output and isError failures", async () => {
  const fetchImplementation = (async (_url: unknown, init: RequestInit) => {
    const request = JSON.parse(String(init.body));
    if (request.method === "notifications/initialized") return new Response(null, { status: 202 });
    const result = request.method === "initialize" ? { protocolVersion: "2025-06-18" } : { isError: true, content: [{ type: "text", text: JSON.stringify({ code: "thread_not_sendable", message: "Thread is archived" }) }] };
    return Response.json({ jsonrpc: "2.0", id: request.id, result });
  }) as typeof fetch;
  const client = new T3Client({ endpoint, authorization: token, fetch: fetchImplementation });
  await assert.rejects(client.send("thread-a", "Review", "id"), (error: unknown) => error instanceof T3Error && error.code === "thread_not_sendable");
});

test("authentication/redirect/transport failures never include response bodies or credentials", async () => {
  for (const status of [401, 403, 500]) {
    const client = new T3Client({ endpoint, authorization: token, fetch: (async () => new Response(token, { status })) as typeof fetch });
    await assert.rejects(client.readThread("thread-a"), (error: unknown) => error instanceof T3Error && !error.message.includes(token));
  }
  const client = new T3Client({ endpoint, authorization: token, fetch: (async () => { throw new Error(token); }) as typeof fetch });
  await assert.rejects(client.send("thread-a", "Review", "stable-id"), (error: unknown) => error instanceof T3Error && error.code === "transport" && !error.message.includes(token));
});

test("rejects mismatched response identity, invalid workspace, and invalid request IDs", async () => {
  const { client } = server(() => ({ thread: thread("/missing", { threadId: "wrong-thread" }), recentRuns: [] }));
  await assert.rejects(client.readThread("thread-a"), { code: "invalid_response" });
  await assert.rejects(client.send("thread-a", "Review", ""), { code: "invalid_request" });
  const wrongIdClient = new T3Client({ endpoint, authorization: token, fetch: (async () => Response.json({ jsonrpc: "2.0", id: 999, result: {} })) as typeof fetch });
  await assert.rejects(wrongIdClient.listProjects(), { code: "invalid_response" });
});

test("lists projects with bounded cursor pagination", async () => {
  const { client } = server((_name, args) => ({ projects: [{ id: args.cursor === undefined ? "p1" : "p2", title: "Project", workspaceRoot: "/tmp" }], nextCursor: args.cursor === undefined ? 1 : null }));
  assert.deepEqual((await client.listProjects()).map((p) => p.id), ["p1", "p2"]);
  const loop = server(() => ({ projects: [], nextCursor: 1 })).client;
  await assert.rejects(loop.listProjects(), { code: "invalid_response" });
});

test("environment precedence is explicit and missing credentials fail closed", () => {
  assert.deepEqual(connectionFromEnvironment({ T3_MCP_URL: endpoint, T3_MCP_BEARER_TOKEN: token }), { endpoint, authorization: token });
  assert.deepEqual(connectionFromEnvironment({ T3_ACP_MCP_ENDPOINT: endpoint, T3_ACP_MCP_AUTHORIZATION: `Bearer ${token}` }), { endpoint, authorization: `Bearer ${token}` });
  assert.equal(connectionFromEnvironment({}), undefined);
  assert.throws(() => connectionFromEnvironment({ HX_T3_MCP_URL: endpoint, T3_MCP_URL: endpoint, T3_MCP_BEARER_TOKEN: token }), { code: "configuration" });
  assert.throws(() => new T3Client({ endpoint: "http://user:secret@localhost/mcp", authorization: token }), { code: "configuration" });
});

test("discovers only the published non-secret runtime descriptor", async () => {
  const root = await mkdtemp(join(tmpdir(), "hx-t3-"));
  try {
    await mkdir(join(root, "userdata"));
    await assert.rejects(discoverLocalEndpoint(root), { code: "server_unavailable" });
    await writeFile(join(root, "userdata", "server-runtime.json"), JSON.stringify({ version: 1, origin: "http://127.0.0.1:3773" }));
    assert.equal(await discoverLocalEndpoint(root), endpoint);
    const devRuntimePath = join(root, "selected-dev-runtime.json");
    await writeFile(devRuntimePath, JSON.stringify({ version: 1, origin: "http://127.0.0.1:4555" }));
    assert.equal(await discoverLocalEndpoint(root, devRuntimePath), "http://127.0.0.1:4555/mcp");
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("reads public environment identity without transmitting a stored credential", async () => {
  const fetchImplementation = (async (url: URL, init: RequestInit) => {
    assert.equal(url.href, "http://127.0.0.1:3773/.well-known/t3/environment");
    assert.equal(new Headers(init.headers).get("authorization"), null);
    assert.equal(init.redirect, "error");
    return Response.json({ environmentId: "env-dev" });
  }) as typeof fetch;
  assert.deepEqual(await readEnvironment(endpoint, { fetch: fetchImplementation }), { environmentId: "env-dev" });
});
