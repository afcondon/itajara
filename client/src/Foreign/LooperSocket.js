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

var state = {
  socket: null,
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

function connect(url) {
  state.url = url;
  if (state.retry) {
    clearTimeout(state.retry);
    state.retry = null;
  }
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
    if (state.url) connect(state.url);
  }, 2000);
}

export const connectImpl = function (url) {
  return function () {
    if (state.socket && state.url === url) return;
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
