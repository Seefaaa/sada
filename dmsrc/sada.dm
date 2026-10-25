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
// An id is 16 hex digits; compare it with == and key lists by it.
/proc/sada_player_id(ckey) as text|null
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

// Sends a batch as one frame; a list of list(player_id, patch) and list(player_id),
// a patch and a removal. Returns null when the library took it, or why it did not.
// An entry it cannot read is left out and leaves an error for sada_take_error().
// Use a /datum/sada_batch rather than building the list by hand.
/proc/sada_send_batch(list/entries) as text|null
	return SADA_CALL(send_batch, entries)

// Frees the player's id for another ckey. The removal itself goes in the batch.
/proc/sada_release_player_id(player_id)
	SADA_CALL(release_player_id, player_id)

// Takes up to max of the events the server has pushed, or null when there are none.
// Nothing is asked of the server here: events arrive on their own and wait in the
// client until the game takes them.
/proc/sada_take_events(max) as /datum/sada_events
	return SADA_CALL(take_events, max)



/*
	Batch
 */

// How many patches one batch holds before it refuses more. A round has far fewer
// players than this, so reaching it means nothing has been sending the batch.
#define SADA_BATCH_LIMIT 4096

/// Patches and removals waiting to go out together, in the order they were made.
///
/// A removal travels behind every patch before it: a patch that overtook one would
/// bring the player back on the server for the rest of the round.
/datum/sada_batch
	/// list(player_id, patch) for a patch, list(player_id) for a removal
	var/list/entries = list()

/// Adds one player's state delta. Absent keys mean unchanged. Returns FALSE when the
/// batch is full and the patch was not taken.
/datum/sada_batch/proc/add_patch(player_id, list/patch)
	if(length(entries) >= SADA_BATCH_LIMIT)
		return FALSE

	entries += list(list(player_id, patch))
	return TRUE

/// Forgets a player entirely and sends the batch at once, returning what send() does.
///
/// This drops their authentication with it, so it belongs to a client going away
/// rather than to a player changing mobs. It is taken even when the batch is full.
/datum/sada_batch/proc/remove_player(player_id) as text|null
	entries += list(list(player_id))
	return send()

/// Sends everything as one frame. Returns null when the library took it, or why it did
/// not; then everything stays, in order, for the next send.
/datum/sada_batch/proc/send() as text|null
	if(!length(entries))
		return

	var/refused = sada_send_batch(entries)

	if(!refused)
		entries = list()

	return refused


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
	var/player_id // player id
	var/session // string

/datum/sada_event/authenticated/New(player_id, session)
	src.player_id = player_id
	src.session = session

/// ControlEvent::Synchronized
/datum/sada_event/synchronized
	var/list/players // list of player ids

/datum/sada_event/synchronized/New(...)
	src.players = args.Copy()

/// ControlEvent::Disconnected
/datum/sada_event/disconnected
	var/player_id // player id
	var/session // string

/datum/sada_event/disconnected/New(player_id, session)
	src.player_id = player_id
	src.session = session

/// ControlEvent::Speaking
/datum/sada_event/speaking
	var/speaker // player id
	var/list/listeners // list of player ids

/datum/sada_event/speaking/New(speaker, ...)
	src.speaker = speaker
	src.listeners = args.Copy(2)

/// ControlEvent::Heard
/datum/sada_event/heard
	var/speaker // player id
	var/listener // player id
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

