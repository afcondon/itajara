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

var state = {
  socket: null,
  url: null,
  latest: null,
  connected: false,
  everConnected: false,
  lastError: "",
  retry: null
};

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
  };

  ws.onmessage = function (ev) {
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
