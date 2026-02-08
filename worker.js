import { connect } from "cloudflare:sockets";

const encoder = new TextEncoder();

async function toBytes(data) {
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
    const buffer = await data.arrayBuffer();
    return new Uint8Array(buffer);
  }
  return null;
}

async function handleSocketSession(serverSocket, env, exitPort) {
  const secureTransport =
    (env.EXIT_NODE_SCHEME || "ws").toLowerCase() === "wss" ? "on" : "off";

  let socket = null;
  let writer = null;
  let reader = null;
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
      if (writer) {
        await writer.close();
      }
    } catch {}

    try {
      writer?.releaseLock();
    } catch {}

    try {
      reader?.releaseLock();
    } catch {}

    try {
      socket?.close();
    } catch {}

    try {
      serverSocket.close(code, reason);
    } catch {}
  };

  const upstreamReady = (async () => {
    socket = connect(
      {
        hostname: env.EXIT_NODE_HOST,
        port: exitPort,
      },
      { secureTransport }
    );

    writer = socket.writable.getWriter();
    reader = socket.readable.getReader();

    await writer.write(encoder.encode(`AUTH ${env.AUTH_SECRET_KEY}\n`));
  })();

  serverSocket.addEventListener("message", (event) => {
    if (closed) {
      return;
    }

    writeQueue = writeQueue
      .then(async () => {
        await upstreamReady;
        const bytes = await toBytes(event.data);
        if (!bytes || bytes.length === 0 || closed) {
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
      await upstreamReady;
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

    const webSocketPair = new WebSocketPair();
    const clientSocket = webSocketPair[0];
    const serverSocket = webSocketPair[1];
    serverSocket.accept();

    void handleSocketSession(serverSocket, env, exitPort);

    return new Response(null, {
      status: 101,
      webSocket: clientSocket,
    });
  },
};
