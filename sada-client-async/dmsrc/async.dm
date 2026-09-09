#define SADA "./libsada_async.so"
#define SADA_CALL(func, args...) call_ext(SADA, "byond:[#func]")(##args)
#define SADA_CALL_ASYNC(func, args...) call_ext(SADA, "byond,await:[#func]")(##args)

/world/New()
	. = ..()
	spawn(1)
		world.log << "/world/New() begins"

		try
			var/result = SADA_CALL_ASYNC(example_async_function, 31)
			world.log << "After a long wait, my result is [result]."
		catch(var/err)
			world.log << "An error occurred while calling the async function.\n[err]"

		// try SADA_CALL(panicing)
		// catch(var/err2) world.log << "An error occurred while calling the panicking function:\n[err2]"

		world.log << "/world/New() ends"

		shutdown()
