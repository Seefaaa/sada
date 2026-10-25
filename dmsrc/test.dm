// Standalone smoke test for the bindings in sada.dm.
//
// This is not part of any game codebase. The SS13 integration lives in dmsrc/ss13
// and deliberately contains no /world/New() override, because dropping one into a
// codebase would replace the game's own world setup.

// Where config.toml puts the control socket. dmsrc/test.sh starts a server of its own
// elsewhere and names it in SADA_TEST_SOCKET.
#define SADA_TEST_SOCKET (world.GetConfig("env", "SADA_TEST_SOCKET") || "/tmp/sada.sock")

// Chebyshev distance the server's proximity policy uses by default.
#define SADA_TEST_RANGE 7

/world/New()
	. = ..()
	// keeps game ticking
	spawn(0)
		while(TRUE)
			sleep(world.tick_lag)

	world.log << "sada client version: [sada_get_version()]"

	sada_test_batch_without_client()

	var/datum/sada_response/version/response

	try
		response = sada_init(SADA_TEST_SOCKET)
	catch(var/init_error)
		world.log << "sada init failed: [init_error]"
		Del()
		return

	world.log << "sada server version: [response.version] (protocol [response.protocol])"

	sada_test_player_ids()
	sada_test_patches()
	sada_test_auth()
	sada_test_session_token()
	sada_test_events()

	var/error = sada_take_error()
	if(error)
		world.log << "FAIL: control error: [error]"
	else
		world.log << "OK: no control errors."

	sada_stop()
	Del()

// The id derive() gives "dave"
#define SADA_TEST_DAVE_ID "0746ea1ecf20ac67"

// The bridge issues ids, and the rest of the bindings only ever see those. An id is
// the 16 hex digits of a 64-bit number, so DM compares them as strings.
/proc/sada_test_player_ids()
	var/alpha = sada_player_id("alpha")
	var/again = sada_player_id("alpha")
	var/beta = sada_player_id("beta")

	if(!istext(alpha) || length(alpha) != 16)
		world.log << "FAIL: alpha was not issued a player id: [alpha]"
	else if(again != alpha)
		world.log << "FAIL: alpha was issued a different id the second time: [alpha], [again]"
	else if(beta == alpha)
		world.log << "FAIL: alpha and beta were issued the same id."
	else
		world.log << "OK: player ids are stable ([alpha])."

	var/dave = sada_player_id("dave")

	if(dave != SADA_TEST_DAVE_ID)
		world.log << "FAIL: dave's id is [dave], expected [SADA_TEST_DAVE_ID]."
	else
		world.log << "OK: an id arrives as its 16 hex digits."

	var/datum/sada_batch/batch = new
	batch.add_patch("alpha", list("mute" = TRUE))

	var/refused = batch.send()
	var/error = sada_take_error()

	if(refused)
		world.log << "FAIL: a batch with an unreadable entry was not sent: [refused]"
	else if(error)
		world.log << "OK: a ckey where an id belongs was left out ([error])."
	else
		world.log << "FAIL: a ckey where an id belongs was accepted."

	batch.add_patch(uppertext(alpha), list("mute" = TRUE))

	refused = batch.send()
	error = sada_take_error()

	if(refused)
		world.log << "FAIL: a batch with an unreadable entry was not sent: [refused]"
	else if(error)
		world.log << "OK: an id spelled in capitals was left out ([error])."
	else
		world.log << "FAIL: an id spelled in capitals was accepted."

	sada_release_player_id("alpha")
	error = sada_take_error()

	if(error)
		world.log << "OK: releasing something that is not an id leaves an error ([error])."
	else
		world.log << "FAIL: releasing something that is not an id left no error."

// Without a client nothing is sent, and the batch has to keep all of it, in order:
// its patches are already recorded as sent, and nothing would ever send a lost
// removal again.
/proc/sada_test_batch_without_client()
	var/alpha = sada_player_id("alpha")
	var/beta = sada_player_id("beta")
	var/datum/sada_batch/batch = new

	batch.add_patch(alpha, list("mute" = TRUE))
	var/refused = batch.remove_player(beta)
	var/list/entries = batch.entries

	if(!refused)
		world.log << "FAIL: a batch was sent without a client."
	else if(length(entries) != 2 || length(entries[1]) != 2 || entries[1][1] != alpha || length(entries[2]) != 1 || entries[2][1] != beta)
		world.log << "FAIL: a refused batch kept [length(entries)] entries, expected alpha's patch then beta's removal."
	else
		world.log << "OK: a refused batch keeps its patch and its removal, in order ([refused])."

	batch = new
	for(var/i in 1 to SADA_BATCH_LIMIT)
		batch.add_patch(alpha, list("mute" = TRUE))

	if(batch.add_patch(alpha, list("mute" = TRUE)))
		world.log << "FAIL: a full batch took another patch."
	else
		batch.remove_player(beta)
		var/list/last = batch.entries[length(batch.entries)]
		if(length(batch.entries) != SADA_BATCH_LIMIT + 1 || length(last) != 1 || last[1] != beta)
			world.log << "FAIL: a full batch refused a removal."
		else
			world.log << "OK: a full batch refuses patches but still takes a removal."

// Adds one player's description to the batch.
/proc/sada_test_patch(datum/sada_batch/batch, ckey, x, y, z)
	batch.add_patch(sada_player_id(ckey), list(
		"mute" = FALSE,
		"deaf" = FALSE,
		"position" = (x << 16) | (y << 8) | z,
	))

// Sends the batch, failing loudly if it is refused or anything in it was left out.
/proc/sada_test_send(datum/sada_batch/batch, what)
	var/refused = batch.send()
	var/error = sada_take_error()

	if(refused)
		world.log << "FAIL: [what]: the batch was refused: [refused]"
	else if(error)
		world.log << "FAIL: [what]: [error]"
	else
		world.log << "OK: [what]."

// Two players within earshot, then far apart. The server recomputes routing for every
// patch it applies, so its log should show "routing recomputed" with the player count.
/proc/sada_test_patches()
	var/datum/sada_batch/batch = new

	sada_test_patch(batch, "alpha", 10, 10, 2)
	sada_test_patch(batch, "beta", 10 + SADA_TEST_RANGE - 1, 10, 2)
	sada_test_send(batch, "two players patched within earshot")

	sada_test_patch(batch, "beta", 10 + SADA_TEST_RANGE + 1, 10, 2)
	sada_test_send(batch, "beta moved out of earshot")

	// A patch the client cannot make sense of has to be reported rather than disappear
	// into a batch, and must not take the rest of the batch with it.
	batch.add_patch(sada_player_id("alpha"), list("nonsense" = 1))
	var/refused = batch.send()
	var/error = sada_take_error()
	if(refused)
		world.log << "FAIL: a batch with an unknown patch field was not sent: [refused]"
	else if(error)
		world.log << "OK: an unknown patch field was left out ([error])."
	else
		world.log << "FAIL: an unknown patch field was accepted."

// Mints a code the way the game would, then asks whether anyone redeemed it.
/proc/sada_test_auth()
	var/code = "TEST42"
	var/alpha = sada_player_id("alpha")

	var/datum/sada_response/ok/response

	try
		response = sada_register_code(code, alpha, "alpha")
	catch(var/register_error)
		world.log << "FAIL: registering a code failed: [register_error]"
		return

	world.log << "OK: registered auth code [code] for alpha; enter it in the web client to bind a session."

	// Answered from a callback, where every reference is persistent; the library has
	// to let go of its own, or every awaited answer would outlive its caller.
	if(refcount(response) != 1)
		world.log << "FAIL: an awaited answer is held [refcount(response)] times, expected only by its caller."
	else
		world.log << "OK: an awaited answer is held only by its caller."

	var/datum/sada_response/session/auth

	try
		auth = sada_check_auth(alpha)
	catch(var/auth_error)
		world.log << "FAIL: checking auth failed: [auth_error]"
		return

	world.log << "OK: checking auth returned a session token ([auth.session])."

// Session ids are 64 bit and DM numbers are single-precision floats, so the client
// hands them over as strings. Decoding one as a number loses the low bits, which is
// exactly the slot half of the id, and every start_transmitting built from it would name the
// wrong session. This guards the day someone decides the quotes look redundant.
/proc/sada_test_session_token()
	var/list/as_string = json_decode("{\"session\": \"4294967303\"}")
	var/list/as_number = json_decode("{\"session\": 4294967303}")

	if(as_string["session"] != "4294967303")
		world.log << "FAIL: a session token did not survive json_decode: [as_string["session"]]"
	else
		world.log << "OK: session token survives as a string; as a number it would arrive as [as_number["session"]]."

/proc/sada_test_events()
	var/datum/sada_events/taken = sada_take_events(32)
	var/list/datum/sada_event/queued = taken?.events

	world.log << "Events waiting: [length(queued)]"

	for(var/datum/sada_event/event in queued)
		world.log << "  [event]"

#undef SADA_TEST_SOCKET
#undef SADA_TEST_RANGE
#undef SADA_TEST_DAVE_ID
