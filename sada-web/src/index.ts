import { css, html, LitElement, nothing, type TemplateResult } from "lit";
import { customElement, query, state } from "lit/decorators.js";
import config from "./config.json";
import { type ServerMessage, SignalingClient } from "./signaling.js";
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
    `;

    @state()
    private connectionState: ConnectionStatus = "disconnected";

    @state()
    private muted = false;

    private signalling?: SignalingClient;
    private rtc?: WebRTCManager;

    @query("audio.remote-audio")
    private remoteAudio?: HTMLAudioElement;

    private async tryConnect(): Promise<void> {
        this.connectionState = "connecting";

        const signalingUrl = `ws://${location.hostname}:${config.signalingPort}/ws`;
        const signaling = new SignalingClient(signalingUrl, "", {
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
            this.cleanup();
            return;
        }

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
        } catch {
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
            case "answer":
                this.rtc?.applyAnswer(message.sdp).catch((e) => {
                    console.error("applyAnswer failed", e);
                });
                break;
            case "offer":
                console.debug("received negotiation offer", message);
                this.rtc?.applyOffer(message.sdp).catch((e) => {
                    console.error("applyOffer failed", e);
                    this.cleanup();
                });
                break;
            case "close":
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

    protected render(): TemplateResult {
        return html`
            <div class="container">
                <audio class="remote-audio" autoplay playsinline></audio>
                <h1>sada</h1>

                <span class="status status-${this.connectionState}">
                    ${this.connectionState}
                </span>

                <div class="controls">${this.connectionStateTemplate()}</div>
            </div>
        `;
    }
}
