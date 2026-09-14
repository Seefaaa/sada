/**
 * The WebSocket half of the browser protocol.
 *
 * The client opens with `hello`, then offers exactly once. After that only the
 * server offers: str0m allows one SDP negotiation in flight and drops a pending
 * offer the moment it accepts an incoming one, so a client that renegotiated on
 * its own would race the server for no benefit.
 *
 * That renegotiation, and everything else the client has to say once the call
 * is up, goes over the data channels instead. See `webrtc.ts`. The message
 * shapes themselves live in `schema.ts`.
 */

import {
    type ClientMessage,
    PROTOCOL_VERSION,
    parseMessage,
    type ServerMessage,
    serverMessageSchema,
} from "./schema";

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
                const msg = parseMessage(ev.data, serverMessageSchema, "signaling");
                if (!msg) return;
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
