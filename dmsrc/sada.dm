// Nothing below waits on the voice server. Calls that carry a result hand back a
// ticket and the answer is collected on a later tick, because call_ext runs on
// BYOND's only thread and a stalled server would stall the world.

var/static/SADA = (world.system_type == MS_WINDOWS ? "./sada.dll" : "./libsada.so")

#define SADA_CALL(func, args...) (call_ext(SADA, "byond:[#func]")(##args))

// How wait() gives the world a turn between polls. A codebase that has stoplag()
// should define this as stoplag() before including this file.
#ifndef SADA_YIELD
#define SADA_YIELD sleep(world.tick_lag)
#endif

/proc/sada_get_version() as text
	return SADA_CALL(get_version)

// Starts the control worker. Resolves to a /datum/sada_response/{version,error} or throws an error
/proc/sada_init(path) as /datum/sada_ticket
	var/datum/sada_result/result = SADA_CALL(init, path)
	if(!result.ok) throw result.value
	return result.value

/proc/sada_stop()
	SADA_CALL(stop)

// Returns null if the ticket is unknown, 1 if it is pending, or a /datum/sada_response otherwise
/proc/sada_poll(ticket) as /datum/sada_response
	return SADA_CALL(poll_ticket, ticket)

/proc/sada_take_error() as text|null
	return SADA_CALL(take_error)

// Returns string or null
/proc/sada_player_id(ckey) as text|null
	return SADA_CALL(player_id, ckey)

// Ticket resolves ControlResponse::Ok or ControlResponse::Error
/proc/sada_register_code(code, player_id, ckey) as /datum/sada_ticket
	var/datum/sada_result/result = SADA_CALL(register_code, code, player_id, ckey)
	if(!result.ok) throw result.value
	return result.value

// Ticket resolves ControlResponse::Session or ControlResponse::Error
/proc/sada_check_auth(player_id) as /datum/sada_ticket
	var/datum/sada_result/result = SADA_CALL(check_auth, player_id)
	if(!result.ok) throw result.value
	return result.value

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
	var/player_id // string
	var/session // string

/datum/sada_event/authenticated/New(player_id, session)
	src.player_id = player_id
	src.session = session

/// ControlEvent::Synchronized
/datum/sada_event/synchronized
	var/list/players // list of string

/datum/sada_event/synchronized/New(...)
	src.players = args.Copy()

/// ControlEvent::Disconnected
/datum/sada_event/disconnected
	var/player_id // string
	var/session // string

/datum/sada_event/disconnected/New(player_id, session)
	src.player_id = player_id
	src.session = session

/// ControlEvent::Speaking
/datum/sada_event/speaking
	var/speaker // string
	var/list/listeners // list of string

/datum/sada_event/speaking/New(speaker, ...)
	src.speaker = speaker
	src.listeners = args.Copy(2)

/// ControlEvent::Heard
/datum/sada_event/heard
	var/speaker // string
	var/listener // string
	var/channel // number or null
	var/language // string or null

/datum/sada_event/heard/New(speaker, listener, channel, language)
	src.speaker = speaker
	src.listener = listener
	src.channel = channel
	src.language = language


/*
	Ticket
 */

/datum/sada_ticket
	var/ticket = 0 // number >0

/datum/sada_ticket/New(ticket)
	src.ticket = ticket

/datum/sada_ticket/proc/wait() as /datum/sada_response
	var/raw = sada_poll(ticket)
	while(raw == 1) // pending
		SADA_YIELD
		raw = sada_poll(ticket)
	var/datum/sada_response/error/error = astype(raw)
	if(!isnull(error)) throw error.message
	// An unknown ticket: stopped, restarted, or evicted while this was waiting on it. Throwing keeps
	// every caller from reading a field off null.
	if(isnull(raw)) throw "the request was dropped before the server answered it"
	return raw

/*
	Result
 */

/datum/sada_result
	var/ok // boolean
	var/value // any

/datum/sada_result/New(ok, value)
	src.ok = ok
	src.value = value
