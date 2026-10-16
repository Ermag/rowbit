// Talks to Rowbit's JSON API. Reads need the session cookie; anything that
// changes something also sends the session's token in a header.

let token = '';

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function call(method, path, body) {
  const headers = { Accept: 'application/json' };
  if (method !== 'GET') {
    headers['Content-Type'] = 'application/json';
    headers['X-Rowbit-Token'] = token;
  }
  const response = await fetch(path, {
    method,
    headers,
    credentials: 'same-origin',
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (response.status === 401) {
    window.location.assign('/login');
    throw new ApiError(401, 'Signed out');
  }
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new ApiError(response.status, data.error || `HTTP ${response.status}`);
  return data;
}

export async function me() {
  const session = await call('GET', '/api/session');
  token = session.token;
  return session;
}

export const changes = (filters = {}) => {
  const q = new URLSearchParams(Object.entries(filters).filter(([, v]) => v));
  return call('GET', `/api/changes?${q}`);
};
export const change = (id, offset = 0) => call('GET', `/api/changes/${id}?offset=${offset}`);
export const approve = (id, review, rows) => call('POST', `/api/changes/${id}/approve`, { review, rows });
export const reject = (id, reason) => call('POST', `/api/changes/${id}/reject`, { reason });
export const undo = (id) => call('POST', `/api/changes/${id}/undo`, {});
export const verify = () => call('GET', '/api/verify');
export const logout = () => call('POST', '/api/logout', {});
