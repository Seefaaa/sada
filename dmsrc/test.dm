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

	world.log << "Sada client version: [sada_get_version()]"

	var/datum/sada_future/version = new(sada_init(SADA_TEST_SOCKET))
	var/response = version.wait()
	var/failure = sada_error_of(response)

	if(failure)
		world.log << "Sada init failed: [failure]"
		shutdown()
		return

	var/list/server = islist(response) ? response["version"] : null
	world.log << "Sada server version: [server?["version"]] (protocol [server?["protocol"]])"

	sada_test_patches()
	sada_test_auth()
	sada_test_events()
	sada_test_session_token()

	var/error = sada_take_error()
	if(error)
		world.log << "FAIL: control error: [error]"
	else
		world.log << "OK: no control errors."

	sada_stop()
	shutdown()

// Describes one player to the server, failing loudly if the patch is refused.
/proc/sada_test_patch(ckey, x, y, z)
	var/rejected = sada_patch_player(ckey, list(
		"mute" = FALSE,
		"deaf" = FALSE,
		"position" = list("x" = x, "y" = y, "z" = z),
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
	if(!sada_patch_player("alpha", list("nonsense" = 1)))
		world.log << "FAIL: an unknown patch field was accepted."
	else
		world.log << "OK: an unknown patch field is refused."

// Mints a code the way the game would, then asks whether anyone redeemed it.
/proc/sada_test_auth()
	var/code = "TEST42"
	var/datum/sada_future/registered = new(sada_register_code(code, "alpha"))
	var/response = registered.wait()

	// A request that carries no result answers with the bare string "ok". Reading it
	// as a list is a runtime error, which is what sada_error_of() exists to prevent.
	if(islist(response))
		world.log << "FAIL: an Ok response arrived as a list: [json_encode(response)]"
	else if(sada_error_of(response))
		world.log << "FAIL: registering a code failed: [sada_error_of(response)]"
	else
		world.log << "OK: registered auth code [code] for alpha ([response]); enter it in the web client to bind a session."

	var/datum/sada_future/auth = new(sada_check_auth("alpha"))
	var/bound = auth.wait()
	var/list/session = islist(bound) ? bound["session"] : null
	world.log << "Session for alpha: [session?["session"] || "none"]"

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
	var/datum/sada_future/events = new(sada_poll_events(32))
	var/response = events.wait()
	var/list/queued = islist(response) ? response["events"] : null

	world.log << "Events queued: [length(queued)]"
	for(var/list/event in queued)
		world.log << "  [json_encode(event)]"

#undef SADA_TEST_SOCKET
#undef SADA_TEST_RANGE
