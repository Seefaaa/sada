// Nothing below stalls the world on the voice server. Calls that carry an answer
// sleep the proc that made them until it arrives, the way sleep() does, while the
// library waits for the server off BYOND's thread; everything else returns at once.

var/static/SADA = (world.system_type == MS_WINDOWS ? "./sada.dll" : "./libsada.so")

#define SADA_CALL(func, args...) (call_ext(SADA, "byond:[#func]")(##args))
#define SADA_AWAIT(func, args...) (call_ext(SADA, "byond,await:[#func]")(##args))

/proc/sada_get_version() as text
	return SADA_CALL(get_version)

// Starts the control worker and returns the server's version. Sleeps; throws on failure.
/proc/sada_init(path) as /datum/sada_response/version
	return sada_unwrap(SADA_AWAIT(init, path), /datum/sada_response/version)

/proc/sada_stop()
	SADA_CALL(stop)

/proc/sada_take_error() as text|null
	return SADA_CALL(take_error)

// Returns the player's id, or null when another ckey already holds the id this one derives.
/proc/sada_player_id(ckey) as /datum/sada_player_id
	return SADA_CALL(player_id, ckey)

// Registers an auth code for the player. Sleeps; throws on failure.
/proc/sada_register_code(code, player_id, ckey) as /datum/sada_response/ok
	return sada_unwrap(SADA_AWAIT(register_code, code, player_id, ckey), /datum/sada_response/ok)

// Returns the session bound to the player. Sleeps; throws on failure.
/proc/sada_check_auth(player_id) as /datum/sada_response/session
	return sada_unwrap(SADA_AWAIT(check_auth, player_id), /datum/sada_response/session)

// Fire and forget: returns before the request reaches the socket. A hot microphone
// cannot wait for a round trip.
/proc/sada_start_transmitting(session, freq)
	SADA_CALL(start_transmitting, session, freq)

/proc/sada_stop_transmitting(session)
	SADA_CALL(stop_transmitting, session)

// Adds one player's state delta to the batch that the next sada_flush() sends.
// Absent keys mean unchanged. Returns "" when the patch was accepted, or the reason
// it was not; nothing reaches the socket until the flush.
/proc/sada_patch_player(player_id, list/patch) as text
	return SADA_CALL(patch_player, player_id, patch)

// Sends everything sada_patch_player() has piled up as one frame.
/proc/sada_flush()
	SADA_CALL(flush)

// Forgets a player entirely. This drops their authentication with it, so it belongs
// to a client going away rather than to a player changing mobs.
/proc/sada_remove_player(player_id)
	SADA_CALL(remove_player, player_id)

// Takes up to max of the events the server has pushed, or null when there are none.
// Nothing is asked of the server here: events arrive on their own and wait in the
// client until the game takes them.
/proc/sada_take_events(max) as /datum/sada_events
	return SADA_CALL(take_events, max)



/*
	ControlResponse
 */

/// ControlResponse::Ok
/datum/sada_response/ok

/// ControlResponse::Version
/datum/sada_response/version
	var/protocol // number
	var/version // string

/datum/sada_response/version/New(protocol, version)
	src.protocol = protocol
	src.version = version

/// ControlResponse::Session
/datum/sada_response/session
	var/session // string or null

/datum/sada_response/session/New(session)
	src.session = session

/// ControlResponse::Batch
/datum/sada_response/batch
	var/list/batch // list of /datum/sada_response

/datum/sada_response/batch/New(...)
	src.batch = args.Copy()

/// ControlResponse::Error
/datum/sada_response/error
	var/message // string

/datum/sada_response/error/New(message)
	src.message = message


/*
	Event
 */

/// What sada_take_events() hands over
/datum/sada_events
	var/list/datum/sada_event/events // list of /datum/sada_event

/datum/sada_events/New(...)
	src.events = args.Copy()

/// ControlEvent::Authenticated
/datum/sada_event/authenticated
	var/datum/sada_player_id/player_id
	var/session // string

/datum/sada_event/authenticated/New(player_id, session)
	src.player_id = player_id
	src.session = session

/// ControlEvent::Synchronized
/datum/sada_event/synchronized
	var/list/datum/sada_player_id/players

/datum/sada_event/synchronized/New(...)
	src.players = args.Copy()

/// ControlEvent::Disconnected
/datum/sada_event/disconnected
	var/datum/sada_player_id/player_id
	var/session // string

/datum/sada_event/disconnected/New(player_id, session)
	src.player_id = player_id
	src.session = session

/// ControlEvent::Speaking
/datum/sada_event/speaking
	var/datum/sada_player_id/speaker
	var/list/datum/sada_player_id/listeners

/datum/sada_event/speaking/New(speaker, ...)
	src.speaker = speaker
	src.listeners = args.Copy(2)

/// ControlEvent::Heard
/datum/sada_event/heard
	var/datum/sada_player_id/speaker
	var/datum/sada_player_id/listener
	var/channel // number or null
	var/language // string or null

/datum/sada_event/heard/New(speaker, listener, channel, language)
	src.speaker = speaker
	src.listener = listener
	src.channel = channel
	src.language = language


/*
	Awaiting
 */

/// Hands back a response of the expected type, and throws for anything else: the message of an error response,
/// or the text the library answers with when it panicked.
/proc/sada_unwrap(response, expected) as /datum/sada_response
	if(istext(response)) throw response
	var/datum/sada_response/error/error = astype(response)
	if(!isnull(error)) throw error.message
	if(!istype(response, expected)) throw "expected [expected] from the voice chat library, got [response || "nothing"]"
	return response



/*
	Player ids
 */

/// A player as the voice server knows them.
///
/// The id is 64 bit and DM numbers are exact only to 2^24, so it arrives as three numbers of bits 40..64, 16..40 and 0..16.
/datum/sada_player_id
	var/chunk1
	var/chunk2
	var/chunk3
	/// The id as 16 hex digits
	var/key

/datum/sada_player_id/New(chunk1, chunk2, chunk3, key)
	src.chunk1 = chunk1
	src.chunk2 = chunk2
	src.chunk3 = chunk3
	src.key = key

/datum/sada_player_id/proc/operator""()
	return key

// following does not work:
// /datum/sada_player_id/proc/operator~=(other)
// 	return src.chunk1 == other.chunk1 && src.chunk2 == other.chunk2 && src.chunk3 == other.chunk3
// ass language
