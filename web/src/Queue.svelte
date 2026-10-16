<script>
  import { onMount } from 'svelte';
  import * as api from './api.js';
  import { ago, count, verbs } from './format.js';

  let held = $state(null);
  let interrupted = $state([]);
  let error = $state('');

  async function load() {
    try {
      const data = await api.changes({ state: 'held' });
      // Oldest first: the one waiting longest is at the top.
      held = data.changes.reverse();
      interrupted = data.interrupted;
      error = '';
    } catch (e) {
      error = e.message;
    }
  }

  onMount(() => {
    load();
    const timer = setInterval(load, 10000);
    return () => clearInterval(timer);
  });
</script>

<h1>Waiting for a person {#if held}({held.length}){/if}</h1>

{#if interrupted.length}
  <p class="error">
    Changes {interrupted.join(', ')} were interrupted while applying or undoing. Compare their rows
    with the preview before anything else.
  </p>
{/if}
{#if error}<p class="error">{error}</p>{/if}

{#if held && held.length === 0}
  <p class="muted">Nothing is waiting.</p>
{/if}

{#each held ?? [] as c (c.id)}
  <a class="card" href="#/changes/{c.id}">
    <div class="card-top">
      <b>#{c.id}</b>
      <span>{c.agent}</span>
      <span class="muted">{ago(c.created_at_ms)}</span>
    </div>
    <div>
      {verbs[c.kind] ?? 'writes'}
      {#if c.rows != null}<b>{count(c.rows, 'row')}</b> in{/if}
      {#if c.table}<code>{c.table}</code>{/if}
    </div>
    {#if c.reason}<div class="muted">Held: {c.reason}</div>{/if}
  </a>
{/each}
