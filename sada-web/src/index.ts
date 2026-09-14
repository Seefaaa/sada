import { css, html, LitElement, nothing, type TemplateResult } from "lit";
import { customElement, state } from "lit/decorators.js";
import config from "./config.json";
import {
    type AudibleSpeaker,
    PROTOCOL_VERSION,
    type ServerMessage,
    type ServerOrderedMessage,
    type ServerUnorderedMessage,
} from "./schema";
import { SignalingClient } from "./signaling";
import { assertNever } from "./utils";
import { WebRTCManager } from "./webrtc";

type ConnectionStatus = "disconnected" | "connecting" | "connected";

/** Everything one speaker's audio passes through on its way to the listener. */
type SpeakerAudio = {
    source: MediaStreamAudioSourceNode;
    panner: PannerNode;
    /** Silent element the track is also attached to, which is what makes Chrome deliver it to WebAudio at all. */
    sink: HTMLAudioElement;
};

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

        .audible {
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
    private audible: number[] = [];

    @state()
    private error: string | null = null;

    @state()
    private authCode = "";

    private signalling?: SignalingClient;
    private rtc?: WebRTCManager;

    private audioContext = new AudioContext();

    /** One entry per incoming m-line, keyed the way `positions` names them. */
    private speakerAudio = new Map<string, SpeakerAudio>();

    private async tryConnect(): Promise<void> {
        this.error = null;
        this.connectionState = "connecting";

        // Browsers start an AudioContext suspended until a user gesture, and this runs inside the click that asked
        // to connect. Without it the whole graph is built and silent.
        await this.audioContext.resume().catch((e) => console.error("could not resume audio", e));

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
            onRemoteTrack: (mid, track) => this.attachRemoteTrack(mid, track),
            onConnectionState: (state) => {
                if (state === "connected") {
                    this.connectionState = "connected";
                } else if (state === "disconnected" || state === "failed" || state === "closed") {
                    this.cleanup();
                }
            },
            onOrdered: (message) => this.onOrderedMessage(message),
            onUnordered: (message) => this.onUnorderedMessage(message),
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

    /**
     * Give one speaker their own place in the room.
     *
     * Each incoming m-line gets its own chain, because the m-line is what the server names a speaker by and what it
     * moves them around on.
     */
    private attachRemoteTrack(mid: string, track: MediaStreamTrack): void {
        // A slot can be handed to a different speaker, and a renegotiation can raise the same m-line again.
        this.detachRemoteTrack(mid);

        const stream = new MediaStream([track]);

        // Chrome will not pump a remote stream through a MediaStreamAudioSourceNode unless it is also attached to a
        // media element. The reference is kept so it is not collected.
        const sink = new Audio();
        sink.srcObject = stream;
        sink.muted = true;
        sink.play().catch((e) => console.error("audio sink failed to start", e));

        const source = this.audioContext.createMediaStreamSource(stream);
        const panner = new PannerNode(this.audioContext, {
            panningModel: "equalpower",
            distanceModel: "inverse",
            refDistance: 1,
            maxDistance: 10_000,
            rolloffFactor: 1,
            coneInnerAngle: 360,
            coneOuterAngle: 0,
            coneOuterGain: 0,
        });

        source.connect(panner);
        panner.connect(this.audioContext.destination);

        this.speakerAudio.set(mid, { source, panner, sink });
    }

    /** Tear one speaker's chain down, leaving nothing connected to the destination. */
    private detachRemoteTrack(mid: string): void {
        const audio = this.speakerAudio.get(mid);

        if (!audio) {
            return;
        }

        audio.source.disconnect();
        audio.panner.disconnect();
        audio.sink.srcObject = null;
        this.speakerAudio.delete(mid);
    }

    /**
     * Put each speaker where the server says they are.
     *
     * The game's axes are not the listener's: east is the panner's x, but north is *forward*, which is negative z.
     * Putting north on y would raise the speaker over the listener's head instead.
     */
    private placeSpeakers(speakers: AudibleSpeaker[]): void {
        for (const speaker of speakers) {
            const audio = this.speakerAudio.get(speaker.mid);

            if (!audio) continue;

            // No offset means do not place them at all: on the radio, on another z-level, or somewhere the game has
            // not described. The centre is the honest answer for all three.
            audio.panner.positionX.value = speaker.offset?.x ?? 0;
            audio.panner.positionY.value = speaker.offset?.y ?? 0;
            // audio.panner.positionZ.value = 0;
        }
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
            case "error":
                console.error("server error", message.code, message.message);
                this.error = message.message;
                this.cleanup();
                break;
            case "bye":
                this.error = message.reason || null;
                this.cleanup();
                break;
            default:
                assertNever(message);
        }
    }

    private onOrderedMessage(message: ServerOrderedMessage): void {
        switch (message.type) {
            case "offer":
                console.debug("received negotiation offer");
                this.rtc?.applyOffer(message.sdp).catch((e) => {
                    console.error("applyOffer failed", e);
                    this.cleanup();
                });
                break;
            default:
                assertNever(message.type);
        }
    }

    private onUnorderedMessage(message: ServerUnorderedMessage): void {
        switch (message.type) {
            case "positions":
                this.audible = message.speakers.map((speaker) => speaker.session);
                this.placeSpeakers(message.speakers);
                break;
            default:
                assertNever(message.type);
        }
    }

    private cleanup(): void {
        this.signalling?.close();
        this.signalling = undefined;
        this.rtc?.hangup();
        this.rtc = undefined;
        for (const mid of [...this.speakerAudio.keys()]) {
            this.detachRemoteTrack(mid);
        }
        this.muted = false;
        this.session = null;
        this.ckey = null;
        this.audible = [];
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
                <h1>sada</h1>

                <span class="status status-${this.connectionState}">
                    ${this.connectionState}
                </span>

                ${this.identityTemplate()}
                ${this.error ? html`<div class="error">${this.error}</div>` : nothing}

                <div class="controls">${this.connectionStateTemplate()}</div>

                <span class="audible">
                    ${this.audible.length > 0 ? `Audible: ${this.audible.join(", ")}` : nothing}
                </span>
            </div>
        `;
    }
}
