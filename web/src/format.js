export function count(n, one, many = `${one}s`) {
  return `${n.toLocaleString('en')} ${n === 1 ? one : many}`;
}

export function ago(ms) {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} d ago`;
}

export function when(ms) {
  return new Date(ms).toLocaleString();
}

export const verbs = { insert: 'inserts', update: 'updates', delete: 'deletes', ddl: 'changes the schema' };
