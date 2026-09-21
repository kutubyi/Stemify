<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { onMount } from "svelte";

  type EngineState = {
    enabled: boolean;
    semitones: number;
    stems: string[];
    status: "off" | "loading" | "waiting" | "ready";
    spotify_connected: boolean;
    error: string | null;
  };

  const ALL_STEMS = ["vocals", "drums", "bass", "guitar", "piano", "other"];

  const STATUS_TEXT = {
    off: "Off",
    loading: "Starting…",
    waiting: "Play something in Spotify…",
    ready: "On: Spotify is playing through Stemify",
  };

  let engine = $state<EngineState | null>(null);

  onMount(() => {
    // Ask Rust for the current state once, then listen for every change it pushes.
    invoke<EngineState>("get_state").then((s) => (engine = s));
    const unlisten = listen<EngineState>("engine-status", (e) => (engine = e.payload));
    return () => {
      unlisten.then((stop) => stop());
    };
  });

  // Each of these calls a Rust command and takes the returned state.
  async function togglePower() {
    if (engine) engine = await invoke<EngineState>("set_enabled", { enabled: !engine.enabled });
  }
  async function setPitch(semitones: number) {
    engine = await invoke<EngineState>("set_pitch", { semitones });
  }
  async function toggleStem(stem: string) {
    if (!engine) return;
    const stems = engine.stems.includes(stem) ? engine.stems.filter((s) => s !== stem) : [...engine.stems, stem];
    engine = await invoke<EngineState>("set_stems", { stems });
  }
</script>

<main>
  {#if engine}
    <header>
      <h1>Stemify</h1>
      <button class="power" class:on={engine.enabled} disabled={!engine.spotify_connected} onclick={togglePower}>
        {engine.enabled ? "On" : "Off"}
      </button>
    </header>

    <p class="spotify" class:waiting={!engine.spotify_connected}>
      {engine.spotify_connected ? "Spotify connected" : "Waiting for Spotify… open Spotify to start"}
    </p>

    <p class="status {engine.status}">{STATUS_TEXT[engine.status]}</p>
    {#if engine.error}
      <p class="error">{engine.error}</p>
    {/if}

    <section class:dim={engine.status === "loading"}>
      <h2>Transpose: {engine.semitones > 0 ? "+" : ""}{engine.semitones}</h2>
      <div class="row">
        <button onclick={() => setPitch(engine!.semitones - 1)}>−</button>
        <input
          type="range"
          min="-12"
          max="12"
          value={engine.semitones}
          onchange={(e) => setPitch(Number((e.target as HTMLInputElement).value))}
        />
        <button onclick={() => setPitch(engine!.semitones + 1)}>+</button>
      </div>

      <h2>Mix</h2>
      <div class="tiles">
        {#each ALL_STEMS as stem}
          <button class="tile" class:selected={engine.stems.includes(stem)} onclick={() => toggleStem(stem)}>
            {stem}
          </button>
        {/each}
      </div>
    </section>
  {:else}
    <p>Connecting…</p>
  {/if}
</main>

<style>
  :global(body) {
    margin: 0;
    background: #1c1c1f;
    color: #f2f2f2;
    font-family: system-ui, sans-serif;
  }
  main {
    max-width: 380px;
    margin: 0 auto;
    padding: 20px;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  h1 {
    font-size: 18px;
    margin: 0;
  }
  h2 {
    font-size: 13px;
    color: #a0a0a8;
    font-weight: 500;
    margin: 18px 0 8px;
  }
  button {
    background: #2b2b30;
    color: inherit;
    border: 1px solid #3a3a41;
    border-radius: 8px;
    padding: 8px 12px;
    font-size: 14px;
    cursor: pointer;
  }
  button:hover {
    background: #34343a;
  }
  .power.on {
    background: #2f6df6;
    border-color: #2f6df6;
  }
  .spotify {
    margin: 12px 0 0;
    font-size: 12px;
    color: #5ecb7a;
  }
  .spotify.waiting {
    color: #e6b34a;
  }
  .status {
    margin: 6px 0 0;
    font-size: 13px;
  }
  .status.loading,
  .status.waiting {
    color: #e6b34a;
  }
  .error {
    margin: 6px 0 0;
    font-size: 12px;
    color: #f0736a;
  }
  button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .status.ready {
    color: #5ecb7a;
  }
  .status.off {
    color: #8a8a92;
  }
  .row {
    display: flex;
    gap: 10px;
    align-items: center;
  }
  input[type="range"] {
    flex: 1;
  }
  .tiles {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 8px;
  }
  .tile {
    padding: 14px 4px;
    text-transform: capitalize;
    color: #8a8a92;
  }
  .tile.selected {
    border-color: #2f6df6;
    color: #f2f2f2;
    background: #26304a;
  }
  .dim {
    opacity: 0.45;
    transition: opacity 0.2s;
  }
</style>
