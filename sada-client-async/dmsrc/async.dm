var/static/SADA = (world.system_type == MS_WINDOWS ? "./sada_async.dll" : "./libsada_async.so")

#define SADA_CALL(func, args...) call_ext(SADA, "byond:[#func]")(##args)
#define SADA_CALL_ASYNC(func, args...) call_ext(SADA, "byond,await:[#func]")(##args)

/world/New()
	. = ..()
	// keeps game ticking
	spawn(0)
		while(1)
			sleep(1)

	world.log << "/world/New() begins"

	try
		var/thing = new /obj/thing
		world.log << "thing rc:\t[refcount(thing)]" // must be 1

		var/result = SADA_CALL_ASYNC(hello_world, thing)
		world.log << "After a long wait, my result is: '[result]'"

		world.log << "result rc:\t[refcount(result)]" // must be 1
		world.log << "thing rc:\t[refcount(thing)]" // must be 1
	catch(var/err)
		world.log << "An error occurred while calling the async function.\n[err]"

	world.log << "/world/New() ends"

	sleep(1)

	Del()

/obj/thing
	name = "thing"

/obj/thing/Del()
	world.log << "thing deleting"

/datum/hello
	var/value

/datum/hello/New(val)
	. = ..()
	value = val

/datum/hello/Del()
	value = null
	world.log << "hello deleting"
