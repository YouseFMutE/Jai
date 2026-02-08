import { connect } from "cloudflare:sockets";

const encoder = new TextEncoder();

function toBytes(data) {
  if (data instanceof Uint8Array) {
    return data;
  }
  if (data instanceof ArrayBuffer) {
    return new Uint8Array(data);
  }
  if (typeof data === "string") {
    return encoder.encode(data);
  }
  if (data && typeof data.arrayBuffer === "function") {
    return data.arrayBuffer().then((buffer) => new Uint8Array(buffer));
  }
  return null;
}

export default {
  async fetch(request, env) {
    const upgradeHeader = request.headers.get("Upgrade");
    if (!upgradeHeader || upgradeHeader.toLowerCase() !== "websocket") {
      return new Response("WebSocket upgrade required", { status: 426 });
    }

    if (!env.AUTH_SECRET_KEY) {
      return new Response("AUTH_SECRET_KEY is not configured", { status: 500 });
    }

    const incomingSecret = request.headers.get("Auth-Secret-Key");
    if (!incomingSecret || incomingSecret !== env.AUTH_SECRET_KEY) {
      return new Response("Unauthorized", { status: 401 });
    }

    if (!env.EXIT_NODE_HOST) {
      return new Response("EXIT_NODE_HOST is not configured", { status: 500 });
    }

    const exitPort = Number(env.EXIT_NODE_PORT || "8443");
    if (!Number.isInteger(exitPort) || exitPort < 1 || exitPort > 65535) {
      return new Response("EXIT_NODE_PORT is invalid", { status: 500 });
    }

    const secureTransport =
      (env.EXIT_NODE_SCHEME || "ws").toLowerCase() === "wss" ? "on" : "off";

    let socket;
    try {
      socket = connect(
        {
          hostname: env.EXIT_NODE_HOST,
          port: exitPort,
        },
        { secureTransport }
      );
    } catch {
      return new Response("Failed to open upstream socket", { status: 502 });
    }

    const writer = socket.writable.getWriter();
    const reader = socket.readable.getReader();

    try {
      await writer.write(encoder.encode(`AUTH ${env.AUTH_SECRET_KEY}\n`));
    } catch {
      try {
        writer.releaseLock();
      } catch {}
      try {
        reader.releaseLock();
      } catch {}
      try {
        socket.close();
      } catch {}
      return new Response("Upstream authentication failed", { status: 502 });
    }

    const webSocketPair = new WebSocketPair();
    const clientSocket = webSocketPair[0];
    const serverSocket = webSocketPair[1];
    serverSocket.accept();

    let closed = false;
    let writeQueue = Promise.resolve();

    const closeAll = async (code = 1000, reason = "normal") => {
      if (closed) {
        return;
      }
      closed = true;

      try {
        await writeQueue;
      } catch {}
      try {
        await writer.close();
      } catch {}
      try {
        writer.releaseLock();
      } catch {}
      try {
        reader.releaseLock();
      } catch {}
      try {
        socket.close();
      } catch {}
      try {
        serverSocket.close(code, reason);
      } catch {}
    };

    serverSocket.addEventListener("message", (event) => {
      if (closed) {
        return;
      }
      writeQueue = writeQueue
        .then(async () => {
          const maybeBytes = toBytes(event.data);
          const bytes =
            maybeBytes instanceof Promise ? await maybeBytes : maybeBytes;
          if (!bytes || bytes.length === 0) {
            return;
          }
          await writer.write(bytes);
        })
        .catch(async () => {
          await closeAll(1011, "upstream-write-failed");
        });
    });

    serverSocket.addEventListener("close", () => {
      void closeAll(1000, "client-closed");
    });

    serverSocket.addEventListener("error", () => {
      void closeAll(1011, "websocket-error");
    });

    (async () => {
      try {
        while (!closed) {
          const { value, done } = await reader.read();
          if (done) {
            break;
          }
          if (value && value.byteLength > 0) {
            serverSocket.send(value);
          }
        }
        await closeAll(1000, "upstream-closed");
      } catch {
        await closeAll(1011, "upstream-read-failed");
      }
    })();

    return new Response(null, {
      status: 101,
      webSocket: clientSocket,
    });
  },
};
