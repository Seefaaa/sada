/**
 * Signaling protocol spoken with server.
 *
 * The client opens with `hello`, then offers exactly once. After that only the
 * server offers: str0m allows one SDP negotiation in flight and drops a pending
 * offer the moment it accepts an incoming one, so a client that renegotiated on
 * its own would race the server for no benefit.
 */

/** Protocol version this client was built against. */
export const PROTOCOL_VERSION = 1;

export type ClientMessage =
    | { type: "hello"; protocol: number; authCode: string | null }
    | { type: "offer"; sdp: string }
    | { type: "answer"; sdp: string }
    | { type: "mute"; muted: boolean }
    | { type: "bye" };

/** Machine-readable reason a request was refused. */
export type ErrorCode =
    | "unsupportedProtocol"
    | "badAuthCode"
    | "authRequired"
    | "unexpectedMessage"
    | "badSdpOffer"
    | "internal";

export type ServerMessage =
    | { type: "welcome"; protocol: number; ckey: string | null }
    | { type: "answer"; sdp: string; session: number }
    | { type: "offer"; sdp: string }
    | { type: "speaking"; sessions: number[] }
    | { type: "error"; code: ErrorCode; message: string }
    | { type: "bye"; reason: string };

export type SignalingHandlers = {
    onServerMessage: (msg: ServerMessage) => void;
    onOpen?: () => void;
    onClose?: (e: CloseEvent) => void;
    onError?: (e: Event) => void;
};

export class SignalingClient {
    private webSocket: WebSocket | null = null;
    private readonly url: string;
    private readonly handlers: SignalingHandlers;

    constructor(url: string, handlers: SignalingHandlers) {
        this.url = url;
        this.handlers = handlers;
    }

    connect(): Promise<void> {
        return new Promise((resolve, reject) => {
            const webSocket = new WebSocket(this.url);
            this.webSocket = webSocket;
            webSocket.onopen = () => {
                this.handlers.onOpen?.();
                resolve();
            };
            webSocket.onmessage = (ev) => {
                const msg = typeof ev.data === "string" ? parseServerMessage(ev.data) : null;
                if (!msg) {
                    console.error("Failed to parse signaling message", ev.data);
                    return;
                }
                this.handlers.onServerMessage(msg);
            };
            webSocket.onclose = (ev) => this.handlers.onClose?.(ev);
            webSocket.onerror = (ev) => {
                this.handlers.onError?.(ev);
                if (webSocket.readyState !== WebSocket.OPEN)
                    reject(new Error("WebSocket connect failed"));
            };
        });
    }

    /** Send the opening handshake. Must be the first message on the socket. */
    hello(authCode: string | null): void {
        this.send({ type: "hello", protocol: PROTOCOL_VERSION, authCode });
    }

    send(msg: ClientMessage): void {
        if (!this.webSocket || this.webSocket.readyState !== WebSocket.OPEN) {
            throw new Error("signaling: not connected");
        }
        this.webSocket.send(JSON.stringify(msg));
    }

    close(): void {
        if (this.webSocket && this.webSocket.readyState === WebSocket.OPEN) {
            try {
                this.webSocket.send(JSON.stringify({ type: "bye" } satisfies ClientMessage));
            } catch {}
        }
        this.webSocket?.close();
        this.webSocket = null;
    }

    get state(): number {
        return this.webSocket?.readyState ?? WebSocket.CLOSED;
    }
}

/** Every error code the server can send, used to validate incoming messages. */
const ERROR_CODES: readonly ErrorCode[] = [
    "unsupportedProtocol",
    "badAuthCode",
    "authRequired",
    "unexpectedMessage",
    "badSdpOffer",
    "internal",
];

/**
 * Validate one incoming message.
 *
 * Anything that does not match the schema is rejected rather than passed on
 * half-formed, so a protocol mismatch surfaces here instead of as a confusing
 * failure later.
 */
function parseServerMessage(raw: string): ServerMessage | null {
    let parsed: unknown;

    try {
        parsed = JSON.parse(raw);
    } catch {
        return null;
    }

    if (typeof parsed !== "object" || parsed === null) return null;

    const obj = parsed as Record<string, unknown>;

    const str = (key: string): string | undefined =>
        typeof obj[key] === "string" ? (obj[key] as string) : undefined;
    const num = (key: string): number | undefined =>
        typeof obj[key] === "number" ? (obj[key] as number) : undefined;

    switch (str("type")) {
        case "welcome": {
            const protocol = num("protocol");
            if (protocol === undefined) return null;
            return { type: "welcome", protocol, ckey: str("ckey") ?? null };
        }
        case "answer": {
            const sdp = str("sdp");
            const session = num("session");
            if (!sdp || session === undefined) return null;
            return { type: "answer", sdp, session };
        }
        case "offer": {
            const sdp = str("sdp");
            return sdp ? { type: "offer", sdp } : null;
        }
        case "speaking": {
            const sessions = obj.sessions;
            if (!Array.isArray(sessions) || sessions.some((s) => typeof s !== "number")) {
                return null;
            }
            return { type: "speaking", sessions: sessions as number[] };
        }
        case "error": {
            const code = str("code");
            const message = str("message");
            if (!code || message === undefined) return null;
            if (!ERROR_CODES.includes(code as ErrorCode)) return null;
            return { type: "error", code: code as ErrorCode, message };
        }
        case "bye":
            return { type: "bye", reason: str("reason") ?? "" };
        default:
            return null;
    }
}
