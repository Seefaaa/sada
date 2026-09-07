import { css, html, LitElement, nothing, type TemplateResult } from "lit";
import { customElement, query, state } from "lit/decorators.js";
import config from "./config.json";
import { PROTOCOL_VERSION, type ServerMessage, SignalingClient } from "./signaling.js";
import { WebRTCManager } from "./webrtc.js";

type ConnectionStatus = "disconnected" | "connecting" | "connected";

@customElement("sada-app")
export class Sada extends LitElement {
    static styles = css`
        :host {
            display: block;
            font-family:
                system-ui,
                -apple-system,
                sans-serif;
            color: light-dark(#1a1a2e, #e0e0e0);
        }

        .container {
            display: flex;
            flex-direction: column;
            align-items: center;
            gap: 1.5rem;
            padding: 2rem;
            max-width: 400px;
            margin: 0 auto;
        }

        h1 {
            margin: 0;
            font-size: 1.5rem;
            font-weight: 600;
        }

        .status {
            font-size: 0.875rem;
            padding: 0.35rem 0.85rem;
            border-radius: 9999px;
            font-weight: 500;
            text-transform: capitalize;
        }

        .status-disconnected {
            background: light-dark(#fee2e2, #451a1a);
            color: light-dark(#991b1b, #fca5a5);
        }

        .status-connecting {
            background: light-dark(#fef3c7, #451a03);
            color: light-dark(#92400e, #fcd34d);
            animation: pulse 1.5s ease-in-out infinite;
        }

        .status-connected {
            background: light-dark(#d1fae5, #064e3b);
            color: light-dark(#065f46, #6ee7b7);
        }

        @keyframes pulse {
            0%,
            100% {
                opacity: 1;
            }
            50% {
                opacity: 0.5;
            }
        }

        .controls {
            display: flex;
            flex-direction: column;
            gap: 0.75rem;
            width: 100%;
        }

        button {
            padding: 0.65rem 1.25rem;
            border: none;
            border-radius: 0.5rem;
            font-size: 0.9375rem;
            font-weight: 500;
            cursor: pointer;
            transition: opacity 0.15s ease;
            color: #fff;
        }

        button:hover {
            opacity: 0.85;
        }

        button:active {
            opacity: 0.7;
        }

        button:disabled {
            opacity: 0.4;
            cursor: not-allowed;
        }

        .btn-connect {
            background: #22c55e;
        }

        .btn-disconnect {
            background: #ef4444;
        }

        .btn-mute {
            background: #f59e0b;
        }

        .btn-unmute {
            background: #3b82f6;
        }

        input {
            font: inherit;
            padding: 0.6rem 0.75rem;
            border-radius: 0.5rem;
            border: 1px solid light-dark(#d4d4d8, #3f3f46);
            background: light-dark(#fff, #18181b);
            color: inherit;
            width: 100%;
            box-sizing: border-box;
            text-transform: uppercase;
            letter-spacing: 0.1em;
        }

        .identity {
            font-size: 0.8125rem;
            color: light-dark(#52525b, #a1a1aa);
        }

        .error {
            font-size: 0.8125rem;
            padding: 0.5rem 0.75rem;
            border-radius: 0.5rem;
            background: light-dark(#fee2e2, #451a1a);
            color: light-dark(#991b1b, #fca5a5);
            width: 100%;
            box-sizing: border-box;
        }

        .speaking {
            font-size: 0.8125rem;
            min-height: 1.2em;
            color: light-dark(#065f46, #6ee7b7);
        }
    `;

    @state()
    private connectionState: ConnectionStatus = "disconnected";

    @state()
    private muted = false;

    @state()
    private ckey: string | null = null;

    @state()
    private session: number | null = null;

    @state()
    private speaking: number[] = [];

    @state()
    private error: string | null = null;

    @state()
    private authCode = "";

    private signalling?: SignalingClient;
    private rtc?: WebRTCManager;

    @query("audio.remote-audio")
    private remoteAudio?: HTMLAudioElement;

    private async tryConnect(): Promise<void> {
        this.error = null;
        this.connectionState = "connecting";

        const url = config.signalingUrl.replace("{hostname}", location.hostname);
        const signaling = new SignalingClient(url, {
            onServerMessage: (msg) => this.onMessage(msg),
            onOpen: () => console.log("signaling open"),
            onClose: () => {
                console.log("signaling closed");
                this.cleanup();
            },
            onError: (e) => {
                console.error("signaling error", e);
                this.cleanup();
            },
        });
        this.signalling = signaling;

        try {
            await signaling.connect();
        } catch {
            this.error = "Could not reach the server.";
            this.cleanup();
            return;
        }

        // The handshake comes first; media only starts once the server has
        // accepted us and told us who we are.
        const code = this.authCode.trim().toUpperCase();
        signaling.hello(code === "" ? null : code);
    }

    /** Start the call. Runs once the server's welcome arrives. */
    private async startMedia(): Promise<void> {
        // biome-ignore lint/style/noNonNullAssertion: only called from onMessage, after connect
        const signaling = this.signalling!;

        const rtc = new WebRTCManager(signaling, {
            onRemoteTrack: (stream) => this.attachRemoteStream(stream),
            onConnectionState: (state) => {
                if (state === "connected") {
                    this.connectionState = "connected";
                } else if (state === "disconnected" || state === "failed" || state === "closed") {
                    this.cleanup();
                }
            },
        });
        this.rtc = rtc;

        try {
            await rtc.acquireMic();
            await rtc.createOffer();
        } catch (e) {
            console.error("could not start media", e);
            this.error = "Could not access the microphone.";
            this.cleanup();
        }
    }

    private attachRemoteStream(stream: MediaStream): void {
        if (!this.remoteAudio) {
            return;
        }

        if (this.remoteAudio.srcObject !== stream) {
            this.remoteAudio.srcObject = stream;
        }

        this.remoteAudio.play().catch((e) => console.error("remote audio play failed", e));
    }

    private onMessage(message: ServerMessage): void {
        switch (message.type) {
            case "welcome":
                if (message.protocol !== PROTOCOL_VERSION) {
                    this.error = `Server speaks protocol ${message.protocol}, this page speaks ${PROTOCOL_VERSION}.`;
                    this.cleanup();
                    return;
                }
                this.ckey = message.ckey;
                if (message.ckey !== null) {
                    this.authCode = "";
                }
                this.startMedia().catch((e) => console.error("startMedia failed", e));
                break;
            case "answer":
                this.session = message.session;
                this.rtc?.applyAnswer(message.sdp).catch((e) => {
                    console.error("applyAnswer failed", e);
                });
                break;
            case "offer":
                console.debug("received negotiation offer");
                this.rtc?.applyOffer(message.sdp).catch((e) => {
                    console.error("applyOffer failed", e);
                    this.cleanup();
                });
                break;
            case "speaking":
                this.speaking = message.sessions;
                break;
            case "error":
                console.error("server error", message.code, message.message);
                this.error = message.message;
                this.cleanup();
                break;
            case "bye":
                this.error = message.reason || null;
                this.cleanup();
                break;
        }
    }

    private cleanup(): void {
        this.signalling?.close();
        this.signalling = undefined;
        this.rtc?.hangup();
        this.rtc = undefined;
        if (this.remoteAudio) {
            this.remoteAudio.srcObject = null;
        }
        this.muted = false;
        this.session = null;
        this.ckey = null;
        this.speaking = [];
        this.connectionState = "disconnected";
    }

    private toggleMute(): void {
        if (!this.rtc) return;
        this.muted = this.rtc.toggleMute();
    }

    private connectionStateTemplate(): TemplateResult | typeof nothing {
        switch (this.connectionState) {
            case "disconnected":
                return html`
                    <input
                        class="auth-code"
                        placeholder="Code"
                        maxlength="6"
                        autocomplete="off"
                        .value="${this.authCode}"
                        @input="${(e: Event) => {
                            this.authCode = (e.target as HTMLInputElement).value;
                        }}"
                        @keydown="${(e: KeyboardEvent) => e.key === "Enter" && this.tryConnect()}"
                    />
                    <button
                        class="btn-connect"
                        @click="${() => this.tryConnect()}"
                    >
                        Connect
                    </button>
                `;
            case "connecting":
                return html`
                    <button
                        class="btn-disconnect"
                        @click="${() => this.cleanup()}"
                    >
                        Cancel
                    </button>
                `;
            case "connected":
                return html`
                    <button
                        class=${this.muted ? "btn-unmute" : "btn-mute"}
                        @click="${() => this.toggleMute()}"
                    >
                        ${this.muted ? "Unmute" : "Mute"}
                    </button>
                    <button
                        class="btn-disconnect"
                        @click="${() => this.cleanup()}"
                    >
                        Disconnect
                    </button>
                `;
            default:
                return nothing;
        }
    }

    private identityTemplate(): TemplateResult | typeof nothing {
        if (this.session === null) {
            return nothing;
        }

        return html`
            <span class="identity">
                ${this.ckey ?? "anonymous"} &middot; session ${this.session}
            </span>
        `;
    }

    protected render(): TemplateResult {
        return html`
            <div class="container">
                <audio class="remote-audio" autoplay playsinline></audio>
                <h1>sada</h1>

                <span class="status status-${this.connectionState}">
                    ${this.connectionState}
                </span>

                ${this.identityTemplate()}
                ${this.error ? html`<div class="error">${this.error}</div>` : nothing}

                <div class="controls">${this.connectionStateTemplate()}</div>

                <span class="speaking">
                    ${this.speaking.length > 0 ? `Speaking: ${this.speaking.join(", ")}` : nothing}
                </span>
            </div>
        `;
    }
}
