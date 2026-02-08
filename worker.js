/**
 * Aegis Edge Relay Worker
 * Required environment variables:
 * - AUTH_SECRET_KEY
 * - EXIT_NODE_HOST
 * Optional:
 * - EXIT_NODE_PORT (default: 8443)
 * - EXIT_NODE_SCHEME (ws or wss, default: ws)
 */

export default {
  async fetch(request, env) {
    const upgradeHeader = request.headers.get("Upgrade");
    if (!upgradeHeader || upgradeHeader.toLowerCase() !== "websocket") {
      return new Response("WebSocket upgrade required", { status: 426 });
    }

    const incomingSecret = request.headers.get("Auth-Secret-Key");
    if (!incomingSecret || incomingSecret !== env.AUTH_SECRET_KEY) {
      return new Response("Unauthorized", { status: 401 });
    }

    if (!env.EXIT_NODE_HOST) {
      return new Response("EXIT_NODE_HOST is not configured", { status: 500 });
    }

    const upstreamScheme =
      (env.EXIT_NODE_SCHEME || "ws").toLowerCase() === "wss" ? "wss" : "ws";
    const upstreamPort = env.EXIT_NODE_PORT || "8443";
    const incomingUrl = new URL(request.url);
    const upstreamUrl = `${upstreamScheme}://${env.EXIT_NODE_HOST}:${upstreamPort}${incomingUrl.pathname}${incomingUrl.search}`;

    const upstreamHeaders = new Headers(request.headers);
    upstreamHeaders.set("Host", env.EXIT_NODE_HOST);
    upstreamHeaders.set("Auth-Secret-Key", env.AUTH_SECRET_KEY);
    upstreamHeaders.set("X-Relay-Mode", "network-resilience");

    const upstreamRequest = new Request(upstreamUrl, {
      method: "GET",
      headers: upstreamHeaders,
    });

    const upstreamResponse = await fetch(upstreamRequest);
    if (upstreamResponse.status !== 101) {
      return new Response("Upstream upgrade failed", { status: 502 });
    }

    return upstreamResponse;
  },
};
