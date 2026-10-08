// Standalone smoke test for the bindings in sada.dm.
//
// This is not part of any game codebase. The SS13 integration lives in dmsrc/ss13
// and deliberately contains no /world/New() override, because dropping one into a
// codebase would replace the game's own world setup.

#define SADA_TEST_SOCKET "/tmp/sada.sock"

// Chebyshev distance the server's proximity policy uses by default.
#define SADA_TEST_RANGE 7

/world/New()
	. = ..()
	// keeps game ticking
	spawn(0)
		while(TRUE)
			sleep(world.tick_lag)

	world.log << "sada client version: [sada_get_version()]"

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

// Two ckeys that derive the same player id.
#define SADA_TEST_COLLIDING_FIRST "zxboiwrq"
#define SADA_TEST_COLLIDING_SECOND "iqbltqzu"

// The bridge issues ids, and the rest of the bindings only ever see those.
/proc/sada_test_player_ids()
	var/alpha = sada_player_id("alpha")

	if(!istext(alpha))
		world.log << "FAIL: alpha was not issued a player id token: [alpha]"
	else if(sada_player_id("alpha") != alpha)
		world.log << "FAIL: alpha was issued a different id the second time."
	else if(sada_player_id("beta") == alpha)
		world.log << "FAIL: alpha and beta were issued the same id."
	else
		world.log << "OK: player ids are stable string tokens ([alpha])."

	var/first = sada_player_id(SADA_TEST_COLLIDING_FIRST)
	var/second = sada_player_id(SADA_TEST_COLLIDING_SECOND)

	if(isnull(first) || !isnull(second))
		world.log << "FAIL: a ckey whose id belongs to another was not refused: [first], [second]"
	else
		world.log << "OK: a ckey whose id belongs to another is refused."

// Describes one player to the server, failing loudly if the patch is refused.
/proc/sada_test_patch(ckey, x, y, z)
	var/rejected = sada_patch_player(sada_player_id(ckey), list(
		"mute" = FALSE,
		"deaf" = FALSE,
		"position" = (x << 16) | (y << 8) | z,
	))

	if(rejected)
		world.log << "FAIL: patch for [ckey] rejected: [rejected]"

// Two players within earshot, then far apart. The server recomputes routing for every
// patch it applies, so its log should show "routing recomputed" with the player count.
/proc/sada_test_patches()
	sada_test_patch("alpha", 10, 10, 2)
	sada_test_patch("beta", 10 + SADA_TEST_RANGE - 1, 10, 2)
	sada_flush()
	world.log << "OK: two players patched within earshot."

	sada_test_patch("beta", 10 + SADA_TEST_RANGE + 1, 10, 2)
	sada_flush()
	world.log << "OK: beta moved out of earshot."

	// A patch the client cannot make sense of has to be refused here rather than
	// disappearing into a batch.
	var/refused = sada_patch_player(sada_player_id("alpha"), list("nonsense" = 1))
	if(refused)
		world.log << "OK: an unknown patch field was refused ([refused])."
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
#undef SADA_TEST_COLLIDING_FIRST
#undef SADA_TEST_COLLIDING_SECOND
