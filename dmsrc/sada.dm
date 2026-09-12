#ifndef SADA
#define SADA (world.system_type == MS_WINDOWS ? "./sada.dll" : "./libsada.so")
#endif

#define SADA_CALL_BYONDAPI(func, args...) call_ext(SADA, "byond:[#func]")(##args)

// Nothing below waits on the voice server. Calls that carry a result hand back a
// ticket and the answer is collected on a later tick, because call_ext runs on
// BYOND's only thread and a stalled server would stall the world.

// A response that has not arrived yet.
#define SADA_PENDING "pending"
// A ticket the client does not know about: never issued, already collected, or dropped as stale.
#define SADA_UNKNOWN "unknown"

// How wait() gives the world a turn between polls. A codebase that has stoplag()
// should define this as stoplag() before including this file.
#ifndef SADA_YIELD
#define SADA_YIELD sleep(world.tick_lag)
#endif

/proc/sada_get_version()
	return SADA_CALL_BYONDAPI(get_version)

// Starts the control worker. Returns the ticket the server version arrives under,
// or 0 if the worker could not be started. Connecting happens on the worker, so a
// server that is not up yet shows as an error on that ticket rather than here.
/proc/sada_init(path)
	return SADA_CALL_BYONDAPI(init, path)

// Stops the control worker and forgets every outstanding ticket.
/proc/sada_stop()
	SADA_CALL_BYONDAPI(stop)

// Returns SADA_PENDING, SADA_UNKNOWN, or the JSON-encoded response.
/proc/sada_poll(ticket)
	return SADA_CALL_BYONDAPI(poll_ticket, ticket)

// The oldest error no ticket was waiting for, such as a failed fire-and-forget
// request, or "" when there is none.
/proc/sada_take_error()
	return SADA_CALL_BYONDAPI(take_error)

/proc/sada_register_code(code, ckey)
	return SADA_CALL_BYONDAPI(register_code, code, ckey)

/proc/sada_check_auth(ckey)
	return SADA_CALL_BYONDAPI(check_auth, ckey)

// Fire and forget: returns before the request reaches the socket. A hot microphone
// cannot wait for a round trip.
/proc/sada_start_transmitting(session, freq)
	SADA_CALL_BYONDAPI(start_transmitting, "[session]", freq ? "[freq]" : "")

/proc/sada_stop_transmitting(session)
	SADA_CALL_BYONDAPI(stop_transmitting, "[session]")

// Adds one player's state delta to the batch that the next sada_flush() sends.
// Absent keys mean unchanged. Returns "" when the patch was accepted, or the reason
// it was not; nothing reaches the socket until the flush.
/proc/sada_patch_player(ckey, list/patch)
	return SADA_CALL_BYONDAPI(patch_player, ckey, json_encode(patch))

// Sends everything sada_patch_player() has piled up as one frame.
/proc/sada_flush()
	SADA_CALL_BYONDAPI(flush)

// Forgets a player entirely. This drops their authentication with it, so it belongs
// to a client going away rather than to a player changing mobs.
/proc/sada_remove_player(ckey)
	SADA_CALL_BYONDAPI(remove_player, ckey)

// Asks for up to max queued server events. Returns the ticket they arrive under.
/proc/sada_poll_events(max)
	return SADA_CALL_BYONDAPI(poll_events, max)

// The reason a response is a failure, or null when it is not one.
//
// Not every response is a list: a request that carries no result answers with the
// bare string "ok", and every element of a batch answers the same way, so indexing a
// response blind is a runtime error rather than a null. Read errors through here.
/proc/sada_error_of(response)
	if(isnull(response))
		return "the request was dropped before the server answered it"

	if(!islist(response))
		return null

	var/list/error = response["error"]
	return error?["message"] || null

// A control request whose answer has not arrived yet.
/datum/sada_future
	// Ticket the client answers under; cleared once the answer is taken.
	var/ticket = 0

/datum/sada_future/New(ticket)
	. = ..()
	src.ticket = ticket

// Returns /datum/poll/pending while the request is still with the worker, otherwise
// /datum/poll/ready holding the decoded response, or null for a ticket the client
// no longer knows about.
/datum/sada_future/proc/poll()
	if(!ticket)
		return new /datum/poll/ready(null)

	var/raw = sada_poll(ticket)
	if(raw == SADA_PENDING)
		return new /datum/poll/pending

	ticket = 0
	return new /datum/poll/ready(raw == SADA_UNKNOWN ? null : json_decode(raw))

// Yields the calling proc, not the world, until the answer arrives.
/datum/sada_future/proc/wait()
	var/datum/poll/result = poll()

	while(istype(result, /datum/poll/pending))
		SADA_YIELD
		result = poll()

	var/datum/poll/ready/ready = result
	return ready.value

/datum/poll

/datum/poll/pending

/datum/poll/ready
	var/value

/datum/poll/ready/New(value)
	. = ..()
	src.value = value
