// Sada voice chat: the hooks that describe players to the voice server.
//
// The voice server starts every player it has not heard about mute and deaf, so a
// player is silent until sada_update() has described them at least once.

// Key held down to talk.
#define SADA_PTT_KEY "V"

// How far local speech carries, in tiles. Must match the voice server's
// [routing].radius, which defaults to 7.
#define SADA_HEARING_RANGE 7

/mob/living
	/// Set when something changed that the voice server has not been told about.
	var/sada_dirty = FALSE
	/// Last values sent, so only what actually changed is sent again.
	var/list/sada_cache
	/// Ckey the last update was sent under, kept for Logout.
	var/sada_ckey
	/// Whether this player is holding the talk key down right now.
	var/sada_talking = FALSE
	/// Session the open microphone belongs to.
	var/sada_talking_session

/client
	/// Opaque session token the voice server issued, or null when not connected.
	///
	/// A string rather than a number because DM integers are 24 bit but session ids are 64 bit.
	var/sada_session = null

/*
	State reporting
*/

/// Forgets what the server has been told, so the next update describes this player
/// again from scratch. Sends nothing by itself.
/mob/living/proc/sada_invalidate()
	sada_cache = list()
	sada_dirty = TRUE

/// Forgets what the server has been told and describes this player again right away.
/mob/living/proc/sada_reset()
	sada_invalidate()
	sada_update()

/// Sends whatever changed since the last call.
/mob/living/proc/sada_update()
	sada_dirty = FALSE

	if(!SSsada.can_fire || isnull(client))
		return

	// Remembered so Logout can silence this body: by then the client is already gone.
	sada_ckey = client.ckey

	var/list/patch = sada_make_patch(sada_cache)

	if(!length(patch))
		return

	var/rejected = sada_patch_player(sada_ckey, patch)
	if(rejected)
		// sada_make_patch has already recorded these values as sent, so the cache has to go;
		// otherwise the fields it just wrote would never be offered again.
		sada_invalidate()
		stack_trace("sada rejected update: [rejected]")

/// Builds the delta the voice server needs. Absent keys mean unchanged, so a patch
/// that comes back empty is not sent at all.
/mob/living/proc/sada_make_patch(list/cache)
	. = list()

	var/mute = !can_speak() || stat != CONSCIOUS
	if(cache["mute"] != mute)
		cache["mute"] = mute
		.["mute"] = mute

	var/deaf = HAS_TRAIT(src, TRAIT_DEAF) || stat != CONSCIOUS
	if(cache["deaf"] != deaf)
		cache["deaf"] = deaf
		.["deaf"] = deaf

	var/turf/here = get_turf(src)

	if(isnull(here))
		return

	if(cache["x"] != here.x || cache["y"] != here.y || cache["z"] != here.z)
		cache["x"] = here.x
		cache["y"] = here.y
		cache["z"] = here.z
		.["position"] = list("x" = here.x, "y" = here.y, "z" = here.z)

/*
	Things that change what the server needs to know
*/

/mob/living/Login()
	. = ..()
	sada_reset()

/mob/living/Logout()
	sada_stop_talking()
	sada_silence()
	return ..()

/// Tells the server this body can no longer speak or hear.
///
/// Logout cannot go through the client, which BYOND has already taken away, so this
/// uses the ckey the last update was sent under.
/mob/living/proc/sada_silence()
	if(!SSsada.can_fire || isnull(sada_ckey))
		return

	var/rejected = sada_patch_player(sada_ckey, list("mute" = TRUE, "deaf" = TRUE))
	if(rejected)
		stack_trace("sada could not silence [sada_ckey]: [rejected]")

	sada_cache = list()
	sada_ckey = null

/mob/living/Move()
	. = ..()
	if(. && client)
		sada_dirty = TRUE

/mob/living/set_stat(new_stat)
	. = ..()
	if(!isnull(.) && client)
		sada_dirty = TRUE

/mob/living/on_hearing_loss()
	. = ..()
	sada_dirty = TRUE

/mob/living/on_hearing_regain()
	. = ..()
	sada_dirty = TRUE

/client/Destroy()
	var/mob/living/living = mob

	if(isliving(living))
		living.sada_stop_talking()

	if(SSsada.can_fire)
		sada_remove_player(ckey)

	return ..()

/*
	Push to talk
*/

/mob/living/key_down(key, client/client, full_key)
	. = ..()
	if(key == SADA_PTT_KEY)
		sada_start_talking()

/mob/living/key_up(key, client/user)
	. = ..()
	if(key == SADA_PTT_KEY)
		sada_stop_talking()

/mob/living/proc/sada_start_talking()
	if(sada_talking || !SSsada.can_fire || isnull(client?.sada_session))
		return

	if(!can_speak() || stat != CONSCIOUS)
		return

	sada_talking = TRUE
	sada_talking_session = client.sada_session

	// null means local
	sada_set_ptt(sada_talking_session, null)
	create_speaking_indicator()

/mob/living/proc/sada_stop_talking()
	if(!sada_talking)
		return

	sada_talking = FALSE
	remove_speaking_indicator()

	if(SSsada.can_fire)
		sada_clear_ptt(sada_talking_session)

	sada_talking_session = null

/*
	Speech bubble
*/

/mob/living
	/// Bubble shown to indicate to nearby players that this player is talking.
	var/active_speaking_indicator

/mob/living/proc/create_speaking_indicator()
	if(active_speaking_indicator || stat != CONSCIOUS)
		return FALSE
	active_speaking_indicator = mutable_appearance('icons/mob/effects/talk.dmi', "[bubble_icon]0", TYPING_LAYER)
	add_overlay(active_speaking_indicator)
	play_fov_effect(src, SADA_HEARING_RANGE, "talk", ignore_self = TRUE)

/mob/living/proc/remove_speaking_indicator()
	if(!active_speaking_indicator)
		return FALSE
	cut_overlay(active_speaking_indicator)
	active_speaking_indicator = null

/*
	Status panel
*/

/mob/living/get_status_tab_items()
	. = ..()

	if(!SSsada.initialized)
		return

	if(!SSsada.can_fire)
		. += "Voice chat: unavailable"
	else if(client?.sada_session)
		. += "Voice chat: connected, hold [SADA_PTT_KEY] to talk"
	else
		. += "Voice chat: not connected"

/*
	Authentication

	The game mints a single-use code and shows it to the player, who types it into the
	web client. The server answers with an Authenticated event, which SSsada turns
	into client.sada_session.
*/

/client/verb/sada_authenticate()
	set category = "OOC"
	set name = "Connect Voice Chat"

	if(!SSsada.initialized || !SSsada.can_fire)
		to_chat(src, span_warning("Voice chat is not available right now."))
		return

	if(sada_session)
		to_chat(src, span_notice("Your voice chat is already connected."))
		return

	var/code = SSsada.generate_auth_code(src)

	if(isnull(code))
		to_chat(src, span_warning("Voice chat could not issue you a code. Try again in a moment."))
		return

	var/datum/browser/window = new(src, "sada_auth", "Voice Chat", 380, 470)
	window.set_content({"
		<iframe src="[CONFIG_GET(string/sada_web_url)]" allow="microphone" style="width: 100%; height: 280px; border: none;"></iframe>
		<p>Open the voice chat page and enter this code:</p>
		<code style="font-size: 2em; font-family: monospace; letter-spacing: 0.2em; cursor: pointer;">[code]📋</code>
		<p>This code is only valid for one connection and expires after a few minutes. Ask
		again for a fresh one, which retires this one.</p>
		<script>
			const codeElement = document.querySelector('code');
			codeElement.addEventListener('click', () => {
				navigator.clipboard.writeText("[code]");
			});
		</script>
	"})
	window.open()
