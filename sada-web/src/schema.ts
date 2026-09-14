/**
 * The browser's half of the wire protocol, mirrored by hand from `sada-server/src/proto.rs`.
 *
 * Neither side negotiates and both ship together, so a change there is a change here in the same commit. Anything
 * that does not match is rejected rather than passed on half-formed, which puts a protocol mismatch here instead of
 * somewhere confusing later.
 */

import * as z from "zod/mini";

/** Protocol version this client was built against. */
export const PROTOCOL_VERSION = 1;

/** A message the browser sends over the WebSocket. */
export type ClientMessage = z.infer<typeof clientMessageSchema>;

/** A message the server sends over the WebSocket. */
export type ServerMessage = z.infer<typeof serverMessageSchema>;

/** Machine-readable reason a request was refused. */
export type ErrorCode = z.infer<typeof errorCodeSchema>;

/** A message the browser sends over the ordered channel. */
export type ClientOrderedMessage = z.infer<typeof clientOrderedMessageSchema>;

/** A message the server sends over the ordered channel. */
export type ServerOrderedMessage = z.infer<typeof serverOrderedMessageSchema>;

/**
 * A message from the unordered channel.
 *
 * Every one carries its own `seq` because the channel may deliver two out of order; the reader keeps the newest it
 * has seen per message type and throws away anything that has been overtaken. The counter wraps, so newer is
 * decided by distance rather than by magnitude; see `isNewer`.
 */
export type ServerUnorderedMessage = z.infer<typeof serverUnorderedMessageSchema>;

/** One speaker the listener holds an audio slot for. */
export type AudibleSpeaker = z.infer<typeof audibleSpeakerSchema>;

/** How far a speaker is from the listener, in tiles. */
export type Offset = z.infer<typeof offsetSchema>;

export const clientMessageSchema = z.discriminatedUnion("type", [
    z.object({
        type: z.literal("hello"),
        protocol: z.number(),
        authCode: z.nullable(z.string()),
    }),
    z.object({
        type: z.literal("offer"),
        sdp: z.string(),
    }),
    z.object({
        type: z.literal("bye"),
    }),
]);

export const errorCodeSchema = z.enum([
    "unsupportedProtocol",
    "badAuthCode",
    "authRequired",
    "unexpectedMessage",
    "badSdpOffer",
    "internal",
]);

export const serverMessageSchema = z.discriminatedUnion("type", [
    z.object({
        type: z.literal("welcome"),
        protocol: z.number(),
        /** The player this connection was bound to, null when anonymous. */
        ckey: z.nullable(z.string()),
    }),
    z.object({
        type: z.literal("answer"),
        sdp: z.string(),
        /** Identifier the server assigned to this session. */
        session: z.number(),
    }),
    z.object({
        type: z.literal("error"),
        code: errorCodeSchema,
        message: z.string(),
    }),
    z.object({
        type: z.literal("bye"),
        reason: z.string(),
    }),
]);

export const clientOrderedMessageSchema = z.discriminatedUnion("type", [
    z.object({
        type: z.literal("answer"),
        sdp: z.string(),
    }),
    z.object({
        type: z.literal("mute"),
        muted: z.boolean(),
    }),
]);

export const serverOrderedMessageSchema = z.discriminatedUnion("type", [
    z.object({
        type: z.literal("offer"),
        sdp: z.string(),
    }),
]);

export const offsetSchema = z.object({
    /** Tiles east of the listener. */
    x: z.number(),
    /** Tiles north of the listener. */
    y: z.number(),
});

export const audibleSpeakerSchema = z.object({
    session: z.number(),
    /** The m-line carrying them, matching some `RTCRtpTransceiver.mid`. */
    mid: z.string(),
    /**
     * Where they are, absent when they are not to be placed at all.
     *
     * Three things leave it empty: either side is somewhere the game has not described, the two are on different
     * z-levels, or the speaker is on the radio rather than in the room.
     */
    offset: z.nullish(offsetSchema),
});

export const serverUnorderedMessageSchema = z.discriminatedUnion("type", [
    z.object({
        type: z.literal("positions"),
        seq: z.number(),
        /** One entry per outgoing audio slot this listener holds, empty when it holds none. */
        speakers: z.array(audibleSpeakerSchema),
    }),
]);

/**
 * Turn one received frame into a message, or `null` with the reason logged.
 */
export function parseMessage<T extends z.ZodMiniType>(
    data: unknown,
    schema: T,
    label: string
): z.input<T> | null {
    if (typeof data !== "string") {
        console.error(`${label} message was not text`, data);
        return null;
    }

    let parsed: unknown;

    try {
        parsed = JSON.parse(data);
    } catch (e) {
        console.error(`failed to parse ${label} message`, data, e);
        return null;
    }

    if (!z.validate(schema, parsed)) {
        console.error(`${label} message failed validation`, parsed);
        return null;
    }

    return parsed;
}
