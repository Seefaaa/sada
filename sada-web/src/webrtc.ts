import config from "./config.json";
import type { SignalingClient } from "./signaling";

export type CallEvents = {
    onRemoteTrack: (stream: MediaStream) => void;
    onConnectionState: (state: RTCPeerConnectionState) => void;
};

export class WebRTCManager {
    private peerConnection: RTCPeerConnection;
    private localStream: MediaStream | null = null;
    private readonly signaling: SignalingClient;
    private readonly events: CallEvents;
    private remoteStream: MediaStream;
    private signalingQueue: Promise<void> = Promise.resolve();

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
            await this.peerConnection.setRemoteDescription({ type: "offer", sdp });
            const answer = await this.peerConnection.createAnswer();
            await this.peerConnection.setLocalDescription(answer);
            await this.waitForIceGathering();

            this.signaling.send({
                type: "answer",
                // biome-ignore lint/style/noNonNullAssertion: it's set just above
                sdp: this.peerConnection.localDescription!.sdp,
            });

            console.debug("negotiation offer applied and answer sent");
        });
    }

    /**
     * Flip the microphone and tell the server.
     *
     * Disabling the track already stops audio leaving the browser; telling the
     * server lets it stop relaying immediately and inform the game.
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
            this.signaling.send({ type: "mute", muted });
        } catch {
            // The socket is already gone; the local track state still stands.
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
