// WebSocket client for the looper daemon.
//
// The daemon pushes a state snapshot thirty times a second. Halogen cannot
// usefully re-render at that rate, and does not need to: what matters is the
// latest snapshot, not every snapshot. So the socket keeps the most recent
// message in a slot and the component reads it on its own schedule. Nothing
// queues, so nothing can fall behind.
//
// Reconnection is automatic and quiet. The daemon is a separate process that
// will be started and stopped independently of the page, and a UI that needs
// reloading because a daemon restarted is a UI that will be sworn at.
//
// ## The socket can go deaf without closing, and `onclose` will not tell you
//
// Measured 2026-08-22, and it cost most of two sessions. The page sat under a
// green "Connected to the daemon." banner showing a loop playing at 5.5 s while
// the daemon had six idle loops. The socket was genuinely OPEN — commands were
// still going OUT and landing, which is why footswitches appeared to work — but
// nothing was coming BACK. `onclose` never fired, so the retry below never ran,
// and the page went on displaying the last snapshot it ever received.
//
// It is one direction of one connection dying, somewhere between here and the
// daemon (the dev router proxies this port, and a fresh connection through it
// is fine — it is the established one that goes quiet). Whose fault the stall
// is does not matter to this file: what matters is that the app can tell.
//
// **So liveness is measured, not asked about.** The daemon pushes thirty times
// a second. Silence for `STALE_MS` means the stream is dead whatever
// `readyState` claims, and the socket is closed so the ordinary reconnect can
// do its job. A display that freezes while insisting it is live is worse than
// one that says it has lost the daemon, because only the second one can be
// believed the rest of the time.

// ## Where to dial is ASKED, not assumed
//
// The stall described above happens on an established connection through the
// dev router's relay. Bosun's `broker` mode exists to get that relay out of the
// path entirely — but a brokered port answers a WebSocket upgrade with a `307`
// carrying the real address, and no WebSocket client follows a redirect. So the
// address has to be resolved BEFORE the socket is opened, which is what
// `resolveAddress` does.
//
// The good property is that the fleet's `serveMode` becomes the only switch,
// with nothing to change here: under `proxy` Bosun answers with the registered
// port and the relay stays in the path; under `broker` it answers with the
// daemon's own port and the relay is gone. Re-resolved on every reconnect, so
// flipping the fleet while the page is open is picked up rather than requiring
// a reload.
//
// Every step falls back to the next, so the worst case — no router, no answer,
// a timeout — is exactly the behaviour that existed before any of this.
var CONTROL_PORT = 3997;
var RESOLVE_TIMEOUT_MS = 1200;

function resolveAddress(registered) {
  // 1. An explicit `?looper=ws://…` wins outright: it is how you point the app
  //    at a daemon you started by hand, and it must not be second-guessed.
  try {
    var override = new URLSearchParams(window.location.search).get("looper");
    if (override) return Promise.resolve(override);
  } catch (e) { /* no window/search — fall through */ }

  var port;
  try {
    port = new URL(registered).port;
  } catch (e) {
    return Promise.resolve(registered);
  }
  if (!port) return Promise.resolve(registered);

  // 2. Ask Bosun. `/where` also STARTS the service if it is down, so this
  //    doubles as the thing that brings the daemon up on page load.
  var ctl = new AbortController();
  var timer = setTimeout(function () { ctl.abort(); }, RESOLVE_TIMEOUT_MS);
  return fetch("http://127.0.0.1:" + CONTROL_PORT + "/where?port=" + port, { signal: ctl.signal })
    .then(function (r) { return r.ok ? r.json() : null; })
    .then(function (body) {
      // Build the address from host+port rather than taking `at.url`: the
      // locator's scheme describes the transport Bosun probed (`http`), and we
      // want a WebSocket. Host and port are the facts; the scheme is ours.
      if (!body || !body.at || !body.at.host || !body.at.port) return registered;
      return "ws://" + body.at.host + ":" + body.at.port;
    })
    // 3. No router, refused, timed out, malformed — dial what we were given.
    .catch(function () { return registered; })
    .then(function (url) { clearTimeout(timer); return url; });
}

var state = {
  socket: null,
  // The address we were REGISTERED with (the port in the fleet), kept apart
  // from the address we actually dialled: a reconnect re-resolves from the
  // first, and re-resolving from the second would pin us to a stale answer.
  registered: null,
  url: null,
  latest: null,
  connected: false,
  everConnected: false,
  lastError: "",
  retry: null,
  // When a snapshot last arrived. The whole liveness check rests on this and on
  // nothing the socket says about itself.
  lastMessageAt: 0,
  watchdog: null
};

// Sixty missed frames. Long enough that a stutter, a garbage collection or a
// slow paint cannot trip it; short enough that a dead stream is noticed before
// you have pressed anything twice wondering why.
var STALE_MS = 2000;

function markAlive() {
  state.lastMessageAt = Date.now();
}

// A background tab may deliver messages in bursts, so time since the last one
// is not evidence of anything while the page is hidden. Coming back to the
// front starts the clock again rather than declaring the stream dead on the
// strength of a gap nobody was watching.
if (typeof document !== "undefined") {
  document.addEventListener("visibilitychange", function () {
    if (!document.hidden) markAlive();
  });
}

function startWatchdog() {
  if (state.watchdog) return;
  state.watchdog = setInterval(function () {
    if (!state.socket || !state.connected) return;
    if (typeof document !== "undefined" && document.hidden) return;
    if (Date.now() - state.lastMessageAt <= STALE_MS) return;
    // Deaf, not closed. Say so, then close it so `onclose` runs the reconnect
    // that already exists — rather than building a second recovery path that
    // would need its own testing and would drift from this one.
    state.lastError = "the daemon stopped sending; reconnecting";
    state.connected = false;
    state.latest = null;
    try {
      state.socket.close();
    } catch (e) {
      // Already gone underneath us; `onclose` may or may not come, so retry
      // from here as well. `scheduleRetry` is idempotent.
      state.socket = null;
      scheduleRetry();
    }
  }, 500);
}

function connect(registered) {
  state.registered = registered;
  if (state.retry) {
    clearTimeout(state.retry);
    state.retry = null;
  }
  resolveAddress(registered).then(function (url) {
    // A connect that resolved while a later one was already in flight must not
    // open a second socket behind it.
    if (state.registered !== registered) return;
    openSocket(url);
  });
}

function openSocket(url) {
  state.url = url;
  try {
    var ws = new WebSocket(url);
  } catch (e) {
    state.lastError = String(e);
    scheduleRetry();
    return;
  }
  state.socket = ws;

  ws.onopen = function () {
    state.connected = true;
    state.everConnected = true;
    state.lastError = "";
    markAlive();
    startWatchdog();
  };

  ws.onmessage = function (ev) {
    markAlive();
    try {
      state.latest = JSON.parse(ev.data);
    } catch (e) {
      // A snapshot we cannot parse is worth knowing about but not worth
      // dropping the connection over.
      state.lastError = "bad snapshot: " + String(e);
    }
  };

  ws.onerror = function () {
    // The event carries nothing useful; onclose follows and does the work.
    state.lastError = state.everConnected
      ? "connection lost"
      : "no looper daemon on " + url;
  };

  ws.onclose = function () {
    // Only if THIS socket is still the current one. Resolution made `connect`
    // asynchronous, so a replaced socket's `onclose` can now arrive after its
    // successor is already open — and un-guarded it would null out the live
    // socket and schedule a retry on top of a working connection.
    if (state.socket !== ws) return;
    state.connected = false;
    state.socket = null;
    state.latest = null;
    scheduleRetry();
  };
}

function scheduleRetry() {
  if (state.retry) return;
  state.retry = setTimeout(function () {
    state.retry = null;
    // Re-resolve, rather than redialling the address that just failed: the
    // fleet may have flipped proxy↔broker underneath us, and a daemon that
    // moved is exactly the case a reconnect should recover from.
    if (state.registered) connect(state.registered);
  }, 2000);
}

export const connectImpl = function (url) {
  return function () {
    if (state.socket && state.registered === url) return;
    if (state.socket) state.socket.close();
    connect(url);
  };
};

export const sendImpl = function (cmd) {
  return function () {
    if (state.socket && state.connected) {
      state.socket.send(cmd);
      return true;
    }
    return false;
  };
};

// Returns the latest snapshot as a plain object, or null. Deliberately a
// pull rather than a callback: the component decides when it wants to look.
export const latestImpl = function () {
  return state.latest;
};

export const statusImpl = function () {
  return {
    connected: state.connected,
    everConnected: state.everConnected,
    lastError: state.lastError,
    url: state.url === null ? "" : state.url
  };
};

// How long ago the newest snapshot arrived, in milliseconds. Lets the page say
// "this picture is old" rather than presenting it as the present.
export const snapshotAgeImpl = function () {
  if (!state.lastMessageAt) return -1;
  return Date.now() - state.lastMessageAt;
};
