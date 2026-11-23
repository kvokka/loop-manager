// Import one real LM session on the local T3 server. The returned thread ID is
// authoritative, including when another client has already imported it.
import { execFileSync } from "node:child_process";
import { readFile, realpath } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, isAbsolute, join } from "node:path";

const home = process.env.T3CODE_HOME || join(homedir(), ".t3");
const data = join(home, "userdata");
const readJson = async (file) => JSON.parse(await readFile(join(data, file), "utf8"));
const cli = (args) =>
  execFileSync("t3", [...args, "--base-dir", home], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "ignore"],
    timeout: 15000,
  });
const segment = (s) =>
  encodeURIComponent(s).replace(/[.!~*'()]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());

function providerSocket(instance) {
  const config = instance.config ?? {};
  if (
    instance.driver !== "acpRegistry" || instance.enabled === false ||
    config.source !== "local" || basename(config.commandPath ?? "") !== "loop-manager"
  ) return null;
  const args = [...(config.commandArgs ?? [])];
  const acp = args.indexOf("acp");
  if (acp < 0) return null;
  args.splice(acp, 1);
  if (args.length === 2 && args[0] === "--socket") return args[1];
  if (args.length === 1 && args[0].startsWith("--socket=")) return args[0].slice("--socket=".length);
  if (args.length) return null;

  const env = { ...process.env };
  for (const item of instance.environment ?? []) {
    if (!["HOME", "LOOP_MANAGER_SOCKET", "XDG_RUNTIME_DIR", "XDG_STATE_HOME"].includes(item.name)) continue;
    if (item.secret || item.valueRedacted) return null;
    env[item.name] = item.value;
  }
  if (env.LOOP_MANAGER_SOCKET) return env.LOOP_MANAGER_SOCKET;
  const base = [env.XDG_RUNTIME_DIR, env.XDG_STATE_HOME].find((dir) => dir && isAbsolute(dir));
  return join(base || join(env.HOME || homedir(), ".local/state"), "loop-manager", "loop-manager.sock");
}

async function registeredProvider(input) {
  const socket = await realpath(input.socket);
  const settings = await readJson("settings.json");
  for (const [id, instance] of Object.entries(settings.providerInstances ?? {})) {
    const path = providerSocket(instance);
    if (!path) continue;
    try {
      if ((await realpath(path)) === socket) return id;
    } catch {
      /* Another provider may belong to an offline daemon. */
    }
  }
  throw Error("Configure a T3 Local ACP command for loop-manager acp on this daemon (see the T3 run card guide).");
}

function hostedLink(environment, thread) {
  return "https://app.t3.codes/threads/" + segment(environment) + "/" + segment(thread);
}

async function existingLinks(input) {
  const instance = await registeredProvider(input);
  const environment = (await readFile(join(data, "environment-id"), "utf8")).trim();
  const threads = JSON.parse(cli(["thread", "list", "--project", input.root, "--json"]));
  const existing = new Set(
    threads.filter((t) => t.providerInstanceId === instance && t.itemCount > 0).map((t) => t.threadId),
  );
  const links = {};
  for (const [attempt, native] of Object.entries(input.sessions)) {
    const thread =
      "thread:provider:acpRegistry:provider-instance:" +
      encodeURIComponent(instance) +
      ":native-thread:" +
      encodeURIComponent(native);
    if (existing.has(thread)) links[attempt] = hostedLink(environment, thread);
  }
  return links;
}

async function importThread(input) {
  const instanceId = await registeredProvider(input);
  const origin = new URL((await readJson("server-runtime.json")).origin);
  if (
    !["127.0.0.1", "[::1]", "localhost"].includes(origin.hostname) ||
    !["http:", "https:"].includes(origin.protocol)
  )
    throw Error("Expected the T3 server's local loopback origin.");
  const environment = (await readFile(join(data, "environment-id"), "utf8")).trim();
  const auth = JSON.parse(
    cli([
      "auth",
      "session",
      "issue",
      "--scope",
      "orchestration:read",
      "--scope",
      "orchestration:operate",
      "--ttl",
      "4m",
      "--label",
      "Open LM step in T3",
      "--json",
    ]),
  );
  let ws;
  try {
    const local = async (route, method = "GET") => {
      const response = await fetch(new URL(route, origin), {
        method,
        headers: { authorization: "Bearer " + auth.token, "x-t3-orchestration-protocol": "2" },
        redirect: "error",
        signal: AbortSignal.timeout(10000),
      });
      if (!response.ok) throw Error("T3 " + route + " returned HTTP " + response.status);
      return response.json();
    };
    const projects = (await local("/api/projects")).projects;
    const project = projects.find((p) => p.workspaceRoot === input.root && !p.deletedAt && !p.archivedAt);
    if (!project) throw Error("Add the LM project directory to T3: " + input.root);
    const { ticket } = await local("/api/auth/websocket-ticket", "POST");
    const url = new URL("/ws", origin);
    url.protocol = origin.protocol === "https:" ? "wss:" : "ws:";
    url.searchParams.set("orchestrationProtocol", "2");
    url.searchParams.set("wsTicket", ticket);
    ws = new WebSocket(url);
    const result = await new Promise((resolve, reject) => {
      const timer = setTimeout(
        () => reject(Error("T3 session import timed out. Retry to open the same thread.")),
        130000,
      );
      const finish = (fn, v) => {
        clearTimeout(timer);
        fn(v);
      };
      ws.onopen = () =>
        ws.send(
          JSON.stringify({
            _tag: "Request",
            id: "1",
            tag: "server.importAcpRegistrySession",
            payload: { instanceId, projectId: project.id, sessionId: input.sessionId, title: input.title },
            headers: [],
          }),
        );
      ws.onerror = () => finish(reject, Error("Could not connect to T3."));
      ws.onclose = () => finish(reject, Error("T3 closed the import connection."));
      ws.onmessage = ({ data }) => {
        const frame = JSON.parse(data);
        for (const m of Array.isArray(frame) ? frame : [frame]) {
          if (m._tag !== "Exit" || m.requestId !== "1") continue;
          if (m.exit._tag === "Success") finish(resolve, m.exit.value);
          else
            finish(reject, Error(m.exit.cause?.[0]?.error?.message || "T3 refused to import the session."));
        }
      };
    });
    if (!result.threadId) throw Error("T3 returned no thread ID.");
    const snapshot = await local(
      "/api/orchestration/threads/" +
        encodeURIComponent(result.threadId) +
        "/bounded?deferWork=true&compactTurnItems=true&dedupeBodies=true",
    );
    if (
      snapshot.projection?.thread?.historyOrigin !== "v1_import" &&
      !snapshot.projection?.thread?.latestRunId
    )
      throw Error("T3 has not imported the session history yet. Retry to finish the import.");
    return { url: hostedLink(environment, result.threadId), threadId: result.threadId };
  } finally {
    ws?.close();
    cli(["auth", "session", "revoke", auth.sessionId]);
  }
}

try {
  let input = "";
  for await (const chunk of process.stdin) input += chunk;
  const action = process.argv[2] === "--existing" ? existingLinks : importThread;
  console.log(JSON.stringify(await action(JSON.parse(input))));
} catch (e) {
  console.error(e.message);
  process.exitCode = 1;
}
