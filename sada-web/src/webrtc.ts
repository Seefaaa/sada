import config from "./config.json";
import type { SignalingClient } from "./signaling";

/** Label of the data channel, mirrored by `CHANNEL_LABEL` in `sada-server/src/sfu/peer.rs`. */
const CHANNEL_LABEL = "main";

export type CallEvents = {
    onRemoteTrack: (stream: MediaStream) => void;
    onConnectionState: (state: RTCPeerConnectionState) => void;
    onMessage: (message: ServerChannelMessage) => void;
};

export type ClientChannelMessage =
    | { type: "answer"; sdp: string }
    | { type: "mute"; muted: boolean };

export type ServerChannelMessage = { type: "offer"; sdp: string };

export class WebRTCManager {
    private peerConnection: RTCPeerConnection;
    private localStream: MediaStream | null = null;
    private readonly signaling: SignalingClient;
    private readonly events: CallEvents;
    private remoteStream: MediaStream;
    private signalingQueue: Promise<void> = Promise.resolve();
    private readonly channel: RTCDataChannel;

    constructor(signaling: SignalingClient, events: CallEvents, iceServers?: RTCIceServer[]) {
        this.signaling = signaling;
        this.events = events;

        this.peerConnection = new RTCPeerConnection({
            iceServers: iceServers ?? config.iceServers.map((url) => ({ urls: url })),
        });
        this.remoteStream = new MediaStream();

        this.peerConnection.ontrack = (ev) => {
            const tracks = ev.streams[0]?.getTracks() ?? [ev.track];
            tracks.forEach((track) => {
                if (!this.remoteStream.getTracks().includes(track)) {
                    this.remoteStream.addTrack(track);
                }
            });
            this.events.onRemoteTrack(this.remoteStream);
        };

        this.peerConnection.onconnectionstatechange = () => {
            this.events.onConnectionState(this.peerConnection.connectionState);
        };

        this.channel = this.peerConnection.createDataChannel(CHANNEL_LABEL);

        this.channel.onmessage = (event) => {
            const msg =
                typeof event.data === "string" ? parseServerChannelMessage(event.data) : null;
            if (!msg) {
                console.error("Failed to parse channel message", event.data);
                return;
            }
            this.events.onMessage(msg);
        };

        this.channel.onopen = () => {
            console.debug("data channel opened");
            // A mute pressed before the channel was up never reached the server.
            if (this.isMuted()) {
                this.sendMessage({ type: "mute", muted: true });
            }
        };

        // Losing the channel is not the end of the call, but it is the end of renegotiation: the server can no
        // longer ask us to take more speakers, and nothing else says so out loud.
        this.channel.onclose = () => {
            console.warn("data channel closed; no more audio slots can be added");
        };

        this.channel.onerror = (event) => {
            console.error("data channel error", event);
        };
    }

    async acquireMic(): Promise<void> {
        this.localStream = await navigator.mediaDevices.getUserMedia({
            audio: {
                echoCancellation: true,
                noiseSuppression: true,
                autoGainControl: true,
            },
            video: false,
        });
        this.localStream.getTracks().forEach((track) => {
            // biome-ignore lint/style/noNonNullAssertion: see just above
            this.peerConnection.addTrack(track, this.localStream!);
        });
    }

    /**
     * Send the one offer this client ever makes.
     *
     * Every later negotiation is started by the server, which is what keeps the
     * two sides from ever offering at the same time.
     */
    async createOffer(): Promise<void> {
        const offer = await this.peerConnection.createOffer();
        await this.peerConnection.setLocalDescription(offer);
        await this.waitForIceGathering();

        this.signaling.send({
            type: "offer",
            // biome-ignore lint/style/noNonNullAssertion: it's set just above
            sdp: this.peerConnection.localDescription!.sdp,
        });
    }

    async applyAnswer(sdp: string): Promise<void> {
        return this.enqueueSignaling(() =>
            this.peerConnection.setRemoteDescription({ type: "answer", sdp })
        );
    }

    /** Accept a server-initiated renegotiation, usually adding audio slots. */
    async applyOffer(sdp: string): Promise<void> {
        return this.enqueueSignaling(async () => {
            // Checked before the connection is touched: an answer we cannot send would leave our local
            // description ahead of the server's, with no way to tell it so.
            if (this.channel.readyState !== "open") {
                throw new Error("data channel is not open; cannot answer an offer");
            }

            await this.peerConnection.setRemoteDescription({ type: "offer", sdp });
            const answer = await this.peerConnection.createAnswer();
            await this.peerConnection.setLocalDescription(answer);
            await this.waitForIceGathering();

            this.sendMessage({
                type: "answer",
                // biome-ignore lint/style/noNonNullAssertion: it's set just above
                sdp: this.peerConnection.localDescription!.sdp,
            });

            console.debug("negotiation offer applied and answer sent");
        });
    }

    private sendMessage(msg: ClientChannelMessage): void {
        if (this.channel.readyState !== "open") {
            throw new Error("data channel is not open; cannot send message");
        }
        this.channel.send(JSON.stringify(msg));
    }

    /** Whether the microphone is muted, as the local tracks have it. */
    private isMuted(): boolean {
        const tracks = this.localStream?.getAudioTracks() ?? [];
        return tracks.length > 0 && tracks.every((track) => !track.enabled);
    }

    /**
     * Flip the microphone and tell the server.
     *
     * Disabling the track already stops audio leaving the browser; telling the
     * server lets it stop relaying immediately, rather than at the end of the
     * talkspurt it is in the middle of forwarding.
     */
    toggleMute(): boolean {
        if (!this.localStream) return false;
        const tracks = this.localStream.getAudioTracks();
        if (tracks.length === 0) return false;
        // biome-ignore lint/style/noNonNullAssertion: we check tracks.length above
        const newEnabled = !tracks[0]!.enabled;
        tracks.forEach((track) => {
            track.enabled = newEnabled;
        });

        const muted = !newEnabled;

        try {
            this.sendMessage({ type: "mute", muted });
        } catch {
            // The channel is not open yet, or is already gone. The track is disabled either way, and a channel
            // that has yet to open will carry the state as soon as it does.
        }

        return muted;
    }

    hangup(): void {
        this.localStream?.getTracks().forEach((track) => {
            track.stop();
        });
        this.localStream = null;
        try {
            this.peerConnection.close();
        } catch {}
    }

    private waitForIceGathering(): Promise<void> {
        // Wait for ICE gathering to finish so all candidates are bundled
        // in the SDP. The check guards against the rare case where
        // gathering is already complete before we attach the listener.
        return new Promise<void>((resolve) => {
            if (this.peerConnection.iceGatheringState === "complete") {
                resolve();
                return;
            }
            const handler = () => {
                if (this.peerConnection.iceGatheringState === "complete") {
                    this.peerConnection.removeEventListener("icegatheringstatechange", handler);
                    resolve();
                }
            };
            this.peerConnection.addEventListener("icegatheringstatechange", handler);
        });
    }

    private enqueueSignaling(task: () => Promise<void>): Promise<void> {
        const next = this.signalingQueue.then(task, task);
        this.signalingQueue = next.catch(() => {});
        return next;
    }
}

function parseServerChannelMessage(raw: string): ServerChannelMessage | null {
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

    switch (str("type")) {
        case "offer": {
            const sdp = str("sdp");
            return sdp ? { type: "offer", sdp } : null;
        }
        default:
            return null;
    }
}
