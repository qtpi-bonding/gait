// Workflow prelude: runs in QuickJS before the user script. The whole file is
// wrapped in an IIFE so host internals (`__host`, `__run`, `__RealDate`,
// `__PatchedDate`, `MSG_SUFFIX`, ...) are not visible to the user script;
// only `globalThis.gait`, `globalThis.Date` and the patched `Math.random`
// escape the closure.
(() => {
    // The placeholder __GAIT_RUN_JSON__ is replaced by script.rs with a JS
    // string literal holding the run object's JSON text (double encoded).
    const __host = globalThis.gait;
    const __run = JSON.parse(__GAIT_RUN_JSON__);

    // config, turn and shell are one-line wrappers forwarding their single
    // argument to the original host object and returning the promise.
    globalThis.gait = {
        wf: {
            run: __run,
            config: (a) => __host.wf.config(a),
            turn: (a) => __host.wf.turn(a),
            shell: (a) => __host.wf.shell(a),
            now: () => __host.wf.now(null),
            random: () => __host.wf.random(null),
            // Tagged template: interpolations become "$GAIT_ARG_<i>" markers in
            // the script text and GAIT_ARG_<i> entries in env. Values are never
            // spliced into the script text.
            sh: (strings, ...values) => {
                let script = strings[0];
                const env = {};
                for (let i = 0; i < values.length; i++) {
                    script += '"$GAIT_ARG_' + i + '"' + strings[i + 1];
                    env["GAIT_ARG_" + i] = String(values[i]);
                }
                return { script, env };
            },
        },
    };

    // Non-deterministic time and randomness are not available in a workflow.
    const MSG_SUFFIX = "use gait.wf.now() / gait.wf.random()";

    const __RealDate = globalThis.Date;

    function __NonDeterministic(what) {
        return new Error(
            "non-deterministic in a workflow: " +
                what +
                " is not available; " +
                MSG_SUFFIX
        );
    }

    const __PatchedDate = new Proxy(__RealDate, {
        construct(target, args) {
            if (args.length === 0) {
                throw __NonDeterministic("new Date()");
            }
            return Reflect.construct(target, args);
        },
        apply(target, thisArg, args) {
            throw __NonDeterministic("Date()");
        },
    });

    __PatchedDate.now = function () {
        throw __NonDeterministic("Date.now()");
    };

    // Close the bypass through `Date.prototype.constructor`: instances of the
    // real Date carry a reference to the real constructor, so
    // `new (Date.prototype.constructor)()` would otherwise skip the Proxy.
    Object.defineProperty(__RealDate.prototype, "constructor", {
        value: __PatchedDate,
        writable: true,
        enumerable: false,
        configurable: true,
    });

    globalThis.Date = __PatchedDate;

    Math.random = function () {
        throw __NonDeterministic("Math.random()");
    };
})();