// Sada voice chat: the game side of the control socket.
//
// Local speech only. The protocol carries radio too, but nothing here fills in
// hot_freqs or hear_freqs, so the voice server never routes a radio channel.
//
// Nothing in this file stalls the world on the voice server. The few calls that
// carry an answer sleep the proc that made them until it arrives, and fire() only
// ever makes calls that return at once; see dmsrc/sada.dm for the bindings.
//
// Install: include this file from the codebase (it pulls in sada_integration.dm
// itself), include dmsrc/sada.dm somewhere earlier for the bindings, and point the
// sada_control_socket config entry at the server's control socket.

#include "sada_integration.dm"

// Wire protocol this build speaks. The server refuses nothing, so the game checks.
#define SADA_PROTOCOL_VERSION 1

// How many of the events waiting in the client one fire may take.
#define SADA_EVENTS_PER_FIRE 64

// Chance per fire that an untouched player is described again anyway, so a state
// change nothing marked dirty cannot go unnoticed forever.
#define SADA_RESYNC_CHANCE 5

// Characters an auth code is built from. No I, O, 0 or 1 to avoid confusion.
#define SADA_CODE_CHARSET "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"

// How many characters one auth code has.
#define SADA_CODE_LENGTH 6

/// Where the voice chat page lives.
///
/// Testing only, will be removed or replaced with a proper something later.
/datum/config_entry/string/sada_web_url
	protection = CONFIG_ENTRY_LOCKED | CONFIG_ENTRY_HIDDEN
	default = "http://localhost:5173"

/datum/config_entry/string/sada_control_socket
	protection = CONFIG_ENTRY_LOCKED | CONFIG_ENTRY_HIDDEN
	default = "/tmp/sada.sock"

SUBSYSTEM_DEF(sada)
	name = "Sada"
	priority = FIRE_PRIORITY_TTS + 5
	// ss_flags = SS_BACKGROUND
	wait = 0.2 SECONDS
	runlevels = RUNLEVEL_GAME | RUNLEVEL_POSTGAME

	/// Version the loaded library reports.
	var/client_version
	/// Version the voice server reports.
	var/server_version

	/// Players left to describe in this fire, so a long run can resume next tick.
	var/list/current_run

	/// Every code minted this round, so one is never handed out twice.
	var/list/used_codes = list()

	/// Maps the voice server's player id to the client that owns it
	var/alist/id_to_client = list()

/*
	Startup and shutdown
*/

/datum/controller/subsystem/sada/Initialize()
	if(world.maxx > 255 || world.maxy > 255 || world.maxz > 255)
		stack_trace("world size [world.maxx]x[world.maxy]x[world.maxz] exceeds 255x255x255")
		return SS_INIT_FAILURE

	var/control_socket = CONFIG_GET(string/sada_control_socket)

	if(!control_socket)
		return SS_INIT_NO_NEED

	client_version = sada_get_version()

	var/datum/sada_response/version/response

	try
		response = sada_init(control_socket)
	catch(var/error)
		stack_trace("could not initialize the voice server: [error]")
		return SS_INIT_FAILURE

	var/protocol = response.protocol

	if(protocol != SADA_PROTOCOL_VERSION)
		stack_trace("the server speaks protocol [protocol], this build speaks [SADA_PROTOCOL_VERSION]")
		return SS_INIT_FAILURE

	server_version = response.version

	return SS_INIT_SUCCESS

/datum/controller/subsystem/sada/Shutdown()
	sada_stop()

/datum/controller/subsystem/sada/stat_entry(msg)
	msg = "C:[client_version || "NOT-INITIALIZED"]|S:[server_version || "NOT-CONNECTED"]"
	return ..()

/*
	Per-tick work
*/

/datum/controller/subsystem/sada/fire(resumed = FALSE)
	if(!resumed)
		current_run = GLOB.player_list.Copy()

	collect_events()
	collect_failures()

	var/list/run = current_run

	while(length(run))
		var/mob/living/player = run[length(run)]
		run.len--

		if(QDELETED(player) || isnull(player.client) || !isliving(player))
			continue

		if(player.sada_dirty || prob(SADA_RESYNC_CHANCE))
			player.sada_update()

		if(player.sada_talking)
			player.create_speaking_indicator()

		if(MC_TICK_CHECK)
			sada_flush()
			return

	sada_flush()

/// Reports whatever the control client could not deliver, and starts over when it lost
/// something.
///
/// A failed request takes its whole batch with it, and a reconnect may have landed on a
/// restarted server that knows nobody at all. Working out which patch died is not worth
/// it when describing everyone again is one cheap pass; this runs only on the error path.
/datum/controller/subsystem/sada/proc/collect_failures()
	var/failure = sada_take_error()
	var/has_failure = FALSE

	while(failure)
		stack_trace("sada fire: [failure]")
		failure = sada_take_error()
		has_failure = TRUE

	if(has_failure)
		for(var/mob/living/player in GLOB.player_list)
			player.sada_invalidate()

/// Acts on whatever the server has said of its own accord.
///
/// The server sends these as they happen and the client holds them until asked.
/datum/controller/subsystem/sada/proc/collect_events()
	var/datum/sada_events/taken = sada_take_events(SADA_EVENTS_PER_FIRE)

	if(isnull(taken))
		return

	for(var/datum/sada_event/event as anything in taken.events)
		handle_event(event)

/datum/controller/subsystem/sada/proc/handle_event(datum/sada_event/event)
	switch(event.type)
		if(/datum/sada_event/synchronized)
			var/datum/sada_event/synchronized/synced = event
			on_synchronized(synced.players)
		if(/datum/sada_event/authenticated)
			var/datum/sada_event/authenticated/auth = event
			on_authenticated(auth.player_id, auth.session)
		if(/datum/sada_event/disconnected)
			var/datum/sada_event/disconnected/disc = event
			on_disconnected(disc.player_id, disc.session)
		else
			stack_trace("unknown event type [event.type]")

/// The client the voice server knows by this player id, or null.
/datum/controller/subsystem/sada/proc/client_of(player_id) as /client
	return id_to_client[player_id]

/// Drops every session the server did not just say it still has.
///
/// The server sends this as soon as a control connection is new. A disconnect notice
/// produced while there was no connection reached nobody, so without this a session the
/// server has forgotten lives on in the game for the rest of the round: the player stays
/// "connected", nothing tells them to reconnect, and every push-to-talk addresses a
/// session that is gone.
/datum/controller/subsystem/sada/proc/on_synchronized(list/players)
	for(var/client/candidate as anything in GLOB.clients)
		if(isnull(candidate.sada_session) || (candidate.sada_id in players))
			continue

		on_disconnected(candidate.sada_id, candidate.sada_session)

/datum/controller/subsystem/sada/proc/on_authenticated(id, session)
	var/client/player = client_of(id)

	if(isnull(player))
		return

	// A fresh control connection is told every live session again, so this is often
	// something the game already knows; saying so twice would chat at the player and
	// re-describe their mob for nothing.
	if(player.sada_session == session)
		return

	player.sada_session = session

	to_chat(player, span_notice("Voice chat connected."))

	var/mob/living/living = player.mob

	if(isliving(living))
		// The server knows this player only as a session id until the game describes
		// them, and it starts them mute and deaf.
		living.sada_reset()

/datum/controller/subsystem/sada/proc/on_disconnected(id, session)
	var/client/player = client_of(id)

	// The player may already have reconnected on a newer session
	if(isnull(player) || player.sada_session != session)
		return

	player.sada_session = null

	var/mob/living/living = player.mob

	if(isliving(living))
		living.sada_stop_talking()

	to_chat(player, span_warning("Voice chat disconnected."))

/*
	Controls
*/

/// Mints a code, registers it with the server and returns it, or null if the server
/// refused.
///
/// Every call mints a new one, and deliberately does not remember the last code a
/// player was given. The server drops a code once somebody connects with it, and
/// again when it expires, and neither of those reaches the game; a remembered code
/// would therefore go dead without the game noticing and the player would be handed
/// the same useless one for the rest of the round. The server keeps only the newest
/// code per player, so the one this returns is also the only one that still works.
/datum/controller/subsystem/sada/proc/generate_auth_code(client/player)
	if(isnull(player))
		return

	var/id = player.sada_get_player_id()
	if(!id)
		return

	var/code

	do code = random_auth_code()
	while (code in used_codes)

	used_codes += code

	try
		sada_register_code(code, id, player.ckey)
	catch(var/error)
		stack_trace("could not register an auth code for [key_name(player)]: [error]")
		return

	return code

/// Generates a random auth code from SADA_CODE_CHARSET of length SADA_CODE_LENGTH.
/datum/controller/subsystem/sada/proc/random_auth_code()
	. = ""
	for(var/i in 1 to SADA_CODE_LENGTH)
		var/at = rand(1, length(SADA_CODE_CHARSET))
		. += copytext(SADA_CODE_CHARSET, at, at + 1)
