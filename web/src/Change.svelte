<script>
  import * as api from './api.js';
  import { count, verbs, when } from './format.js';
  import State from './State.svelte';
  import Value from './Value.svelte';

  let { id } = $props();

  let c = $state(null);
  let error = $state('');
  let busy = $state(false);
  let showUnchanged = $state(false);
  let rowsTyped = $state('');
  let reason = $state('');

  async function load(offset = 0) {
    try {
      c = await api.change(id, offset);
      error = '';
    } catch (e) {
      error = e.message;
    }
  }

  $effect(() => {
    load();
  });

  async function act(fn) {
    busy = true;
    try {
      c = await fn();
      error = '';
    } catch (e) {
      error = e.message;
    } finally {
      busy = false;
    }
  }

  const total = $derived(c?.preview?.total_rows ?? 0);
  const approveLabel = $derived(
    c?.preview
      ? total === 1
        ? 'Approve and apply 1 row'
        : `Approve and apply all ${total.toLocaleString('en')} rows`
      : 'Approve and run'
  );
  const rowsOk = $derived(!c?.confirm_rows || Number(rowsTyped) === total);
  const page = $derived(c?.preview ? Math.floor(c.preview.offset / 50) : 0);
  const pages = $derived(Math.ceil(total / 50));
</script>

<p><a href="#/">Back to the queue</a></p>

{#if error}<p class="error" role="alert">{error}</p>{/if}

{#if c}
  <section>
    <div class="card-top">
      <h1>#{c.id} from {c.agent}</h1>
      <State state={c.state} />
    </div>
    {#if c.reason}<p class="muted">{c.reason}</p>{/if}
    <pre>{c.statement}</pre>

    {#if c.ddl}
      <p>Runs this schema change, which can't be undone:</p>
      <pre>{c.ddl}</pre>
    {:else if c.preview}
      <p>
        {verbs[c.preview.kind]} <b>{count(total, 'row')}</b> in <code>{c.preview.table}</code>
        <label class="inline"
          ><input type="checkbox" bind:checked={showUnchanged} /> show unchanged columns</label
        >
      </p>
      <div class="scroll">
        <table>
          <thead><tr><th>Row</th><th>Column</th><th>Before</th><th>After</th></tr></thead>
          <tbody>
            {#each c.preview.rows as row, r (r)}
              {#each row.cells.filter((x) => showUnchanged || x.changed) as cell, i (i)}
                <tr class:first={i === 0}>
                  <td>
                    {#if i === 0}
                      {#if row.key}
                        {#each row.key as k, j (j)}{#if j > 0},
                          {/if}<Value value={k} />{/each}
                      {:else}<span class="muted">new</span>{/if}
                    {/if}
                  </td>
                  <td>{cell.column}</td>
                  <td class="before" class:same={!cell.changed}><Value value={cell.before} /></td>
                  <td class="after" class:same={!cell.changed}><Value value={cell.after} /></td>
                </tr>
              {/each}
            {/each}
          </tbody>
        </table>
      </div>
      {#if pages > 1}
        <p class="pager">
          <button disabled={page === 0} onclick={() => load((page - 1) * 50)}>Previous</button>
          Rows {c.preview.offset + 1} to {Math.min(c.preview.offset + 50, total)} of
          {total.toLocaleString('en')}
          <button disabled={page + 1 >= pages} onclick={() => load((page + 1) * 50)}>Next</button>
        </p>
      {/if}
    {/if}

    {#if c.state === 'held'}
      <div class="actions">
        <form
          onsubmit={(e) => {
            e.preventDefault();
            act(() => api.approve(c.id, c.review_hash, c.confirm_rows ? Number(rowsTyped) : undefined));
          }}
        >
          {#if c.confirm_rows}
            <label
              >Type the row count to approve
              <input inputmode="numeric" bind:value={rowsTyped} placeholder={String(total)} required /></label
            >
          {/if}
          <button class="approve" disabled={busy || !rowsOk}>{approveLabel}</button>
        </form>
        <form
          onsubmit={(e) => {
            e.preventDefault();
            act(() => api.reject(c.id, reason));
          }}
        >
          <input bind:value={reason} placeholder="Why reject?" required maxlength="500" />
          <button disabled={busy}>Reject</button>
        </form>
      </div>
      <p class="muted small">Approving covers review {c.review_hash?.slice(0, 12)}: exactly the rows above.</p>
    {/if}

    {#if c.undoable}
      <div class="actions">
        <button disabled={busy} onclick={() => act(() => api.undo(c.id))}>Undo</button>
        <span class="muted small">Restores every row as it was, unless one changed since.</span>
      </div>
    {/if}
  </section>

  <h2>History</h2>
  <table>
    <thead><tr><th>When</th><th>Event</th><th>State</th><th>By</th><th>Detail</th></tr></thead>
    <tbody>
      {#each c.history as e (e.seq)}
        <tr>
          <td class="nowrap">{when(e.at_ms)}</td>
          <td>{e.event}</td>
          <td>{e.to}</td>
          <td>{e.actor}</td>
          <td>{e.detail ?? ''}</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}
