<script>
  import { onMount } from 'svelte';
  import * as api from './api.js';
  import Change from './Change.svelte';
  import Log from './Log.svelte';
  import Queue from './Queue.svelte';
  import Shadow from './Shadow.svelte';

  let reviewer = $state('');
  let route = $state(location.hash);

  onMount(() => {
    api.me().then((s) => (reviewer = s.reviewer));
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
  </nav>
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
  {:else}
    <Queue />
  {/if}
</main>
