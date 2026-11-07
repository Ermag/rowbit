<script>
  import { onMount } from 'svelte';
  import * as api from './api.js';
  import Change from './Change.svelte';
  import Deliveries from './Deliveries.svelte';
  import Log from './Log.svelte';
  import Queue from './Queue.svelte';
  import Setup from './Setup.svelte';
  import Shadow from './Shadow.svelte';

  let reviewer = $state('');
  let guarding = $state(null);
  let route = $state(location.hash);

  onMount(() => {
    api.me().then((s) => {
      reviewer = s.reviewer;
      api.setup().then((s) => (guarding = s.database)).catch(() => {});
    });
    const onHash = () => (route = location.hash);
    addEventListener('hashchange', onHash);
    return () => removeEventListener('hashchange', onHash);
  });

  const changeId = $derived(route.match(/^#\/changes\/(\d+)$/)?.[1]);

  async function signOut() {
    await api.logout().catch(() => {});
    location.assign('/login');
  }
</script>

<header>
  <a class="brand" href="#/">Rowbit</a>
  <nav>
    <a href="#/" class:active={route === '' || route === '#/'}>Held</a>
    <a href="#/log" class:active={route === '#/log'}>Log</a>
    <a href="#/shadow" class:active={route === '#/shadow'}>Shadow</a>
    <a href="#/notifications" class:active={route === '#/notifications'}>Notifications</a>
    <a href="#/setup" class:active={route === '#/setup'}>Setup</a>
  </nav>
  {#if guarding}
    <a class="guarding" href="#/setup" title="What this Rowbit is connected to">
      Guarding <code>{guarding.database}</code> on <code>{guarding.host}:{guarding.port}</code> as <code>{guarding.user}</code>
    </a>
  {/if}
  <span class="who">
    {#if reviewer}{reviewer} <button class="link" onclick={signOut}>Sign out</button>{/if}
  </span>
</header>

<main>
  {#if !reviewer}
    <p class="muted">Loading...</p>
  {:else if changeId}
    {#key changeId}<Change id={changeId} />{/key}
  {:else if route === '#/log'}
    <Log />
  {:else if route === '#/shadow'}
    <Shadow />
  {:else if route === '#/notifications'}
    <Deliveries />
  {:else if route === '#/setup'}
    <Setup />
  {:else}
    <Queue />
  {/if}
</main>
