import config from "./config.json";
import type { SignalingClient } from "./signaling";

/**
 * Labels of the two data channels, mirrored in `sada-server/src/sfu/peer.rs`.
 *
 * Named for what each one promises rather than for what it happens to carry: the ordered one for anything that
 * would break if it went missing, the other for state that is replaced by the next message rather than repaired.
 */
const ORDERED_CHANNEL_LABEL = "ordered";
const UNORDERED_CHANNEL_LABEL = "unordered";

export type CallEvents = {
    /** One incoming audio track, identified by the m-line carrying it so `positions` can be matched to it. */
    onRemoteTrack: (mid: string, track: MediaStreamTrack) => void;
    onConnectionState: (state: RTCPeerConnectionState) => void;
    onOrdered: (message: ServerOrderedMessage) => void;
    onUnordered: (message: ServerUnorderedMessage) => void;
};

export type ClientOrderedMessage =
    | { type: "answer"; sdp: string }
    | { type: "mute"; muted: boolean };

export type ServerOrderedMessage = { type: "offer"; sdp: string };

/**
 * A message from the unordered channel.
 *
 * Every one carries its own `seq` because the channel may deliver two out of order; the reader keeps the newest it
 * has seen per message type and throws away anything that has been overtaken. The counter wraps, so newer is
 * decided by distance rather than by magnitude; see `isNewer`.
 */
export type ServerUnorderedMessage = {
    type: "positions";
    seq: number;
    speakers: AudibleSpeaker[];
};

/** One speaker the listener holds an audio slot for. */
export type AudibleSpeaker = {
    session: number;
    /** The m-line carrying them, matching some `RTCRtpTransceiver.mid`. */
    mid: string;
    /** Absent when they are not to be placed: on the radio, on another z-level, or somewhere the game has not described. */
    offset: Offset | null;
};

/** How far a speaker is from the listener, in tiles. */
export type Offset = { x: number; y: number };

export class WebRTCManager {
    private peerConnection: RTCPeerConnection;
    private localStream: MediaStream | null = null;
    private readonly signaling: SignalingClient;
    private readonly events: CallEvents;
    private signalingQueue: Promise<void> = Promise.resolve();
    private readonly ordered: RTCDataChannel;
    private readonly unordered: RTCDataChannel;
    private readonly lastSeq = new Map<string, number>();

    constructor(signaling: SignalingClient, events: CallEvents, iceServers?: RTCIceServer[]) {
        this.signaling = signaling;
        this.events = events;

        this.peerConnection = new RTCPeerConnection({
            iceServers: iceServers ?? config.iceServers.map((url) => ({ urls: url })),
        });
        this.peerConnection.ontrack = (ev) => {
            // The mid comes from the remote description being applied right now, so it is always set here. It is
            // the only thing tying this track to the speaker the server names in `positions`.
            const mid = ev.transceiver.mid;
            if (!mid) {
                console.error("remote track arrived with no mid", ev.track.id);
                return;
            }
            this.events.onRemoteTrack(mid, ev.track);
        };

        this.peerConnection.onconnectionstatechange = () => {
            this.events.onConnectionState(this.peerConnection.connectionState);
        };

        this.ordered = this.peerConnection.createDataChannel(ORDERED_CHANNEL_LABEL);

        this.ordered.onmessage = (event) => {
            const msg =
                typeof event.data === "string" ? parseServerOrderedMessage(event.data) : null;
            if (!msg) {
                console.error("Failed to parse ordered channel message", event.data);
                return;
            }
            this.events.onOrdered(msg);
        };

        this.ordered.onopen = () => {
            console.debug("ordered channel opened");
            // A mute pressed before the channel was up never reached the server.
            if (this.isMuted()) {
                this.sendMessage({ type: "mute", muted: true });
            }
        };

        // Losing the channel is not the end of the call, but it is the end of renegotiation: the server can no
        // longer ask us to take more speakers, and nothing else says so out loud.
        this.ordered.onclose = () => {
            console.warn("ordered channel closed; no more audio slots can be added");
        };

        this.ordered.onerror = (event) => {
            console.error("ordered channel error", event);
        };

        this.unordered = this.peerConnection.createDataChannel(UNORDERED_CHANNEL_LABEL, {
            ordered: false,
            maxRetransmits: 0,
        });

        this.unordered.onmessage = (event) => {
            const msg =
                typeof event.data === "string" ? parseServerUnorderedMessage(event.data) : null;
            if (!msg) {
                console.error("Failed to parse unordered channel message", event.data);
                return;
            }

            const seen = this.lastSeq.get(msg.type);
            if (seen !== undefined && !isNewer(msg.seq, seen)) {
                console.debug("dropping an overtaken message", msg.type, msg.seq, seen);
                return;
            }

            this.lastSeq.set(msg.type, msg.seq);
            this.events.onUnordered(msg);
        };

        this.unordered.onopen = () => {
            console.debug("unordered channel opened");
        };

        this.unordered.onclose = () => {
            console.warn("unordered channel closed; positions will stop updating");
        };

        this.unordered.onerror = (event) => {
            console.error("unordered channel error", event);
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
            if (this.ordered.readyState !== "open") {
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

    private sendMessage(msg: ClientOrderedMessage): void {
        if (this.ordered.readyState !== "open") {
            throw new Error("data channel is not open; cannot send message");
        }
        this.ordered.send(JSON.stringify(msg));
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

function parseServerOrderedMessage(raw: string): ServerOrderedMessage | null {
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

/**
 * Whether `seq` comes after `seen` on a counter that wraps at 2^32.
 */
function isNewer(seq: number, seen: number): boolean {
    const ahead = (seq - seen) >>> 0;
    return ahead !== 0 && ahead < 0x8000_0000;
}

function parseServerUnorderedMessage(raw: string): ServerUnorderedMessage | null {
    let parsed: unknown;

    try {
        parsed = JSON.parse(raw);
    } catch {
        return null;
    }

    if (typeof parsed !== "object" || parsed === null) return null;

    const obj = parsed as Record<string, unknown>;

    switch (obj.type) {
        case "positions": {
            if (typeof obj.seq !== "number" || !Array.isArray(obj.speakers)) return null;

            const speakers: AudibleSpeaker[] = [];
            for (const entry of obj.speakers) {
                const speaker = parseAudibleSpeaker(entry);
                // One malformed entry makes the whole set untrustworthy: a missing speaker would be read as one
                // who has stopped being audible.
                if (!speaker) return null;
                speakers.push(speaker);
            }

            return { type: "positions", seq: obj.seq, speakers };
        }
        default:
            return null;
    }
}

function parseAudibleSpeaker(raw: unknown): AudibleSpeaker | null {
    if (typeof raw !== "object" || raw === null) return null;

    const obj = raw as Record<string, unknown>;
    if (typeof obj.session !== "number" || typeof obj.mid !== "string") return null;

    if (obj.offset === null || obj.offset === undefined) {
        return { session: obj.session, mid: obj.mid, offset: null };
    }

    if (typeof obj.offset !== "object") return null;

    const offset = obj.offset as Record<string, unknown>;
    if (typeof offset.x !== "number" || typeof offset.y !== "number") return null;

    return { session: obj.session, mid: obj.mid, offset: { x: offset.x, y: offset.y } };
}
