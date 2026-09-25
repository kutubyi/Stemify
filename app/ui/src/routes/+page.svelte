<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onMount } from "svelte";
  import { Minus, Music2, Plus, Power, X } from "lucide-svelte";
  import micIcon from "$lib/icons/mic.svg?raw";
  import drumsIcon from "$lib/icons/drum-kit.svg?raw";
  import bassIcon from "$lib/icons/bass-clef.svg?raw";
  import guitarIcon from "$lib/icons/guitar.svg?raw";
  import pianoIcon from "$lib/icons/grand-piano.svg?raw";
  import otherIcon from "$lib/icons/music-notes.svg?raw";

  const win = getCurrentWindow();

  type EngineState = {
    enabled: boolean;
    semitones: number;
    stems: string[];
    keep_model: boolean;
    status: "off" | "loading" | "waiting" | "idle" | "ready";
    spotify_connected: boolean;
    error: string | null;
    track: { title: string; artist: string; art: string | null } | null;
  };

  const ALL_STEMS = ["vocals", "drums", "bass", "guitar", "piano", "other"];
  const BACKING_STEMS = ALL_STEMS.filter((s) => s !== "vocals");

  const STEM_ICONS = { vocals: micIcon, drums: drumsIcon, bass: bassIcon, guitar: guitarIcon, piano: pianoIcon, other: otherIcon };

  const STATUS_TEXT = {
    off: "Off",
    loading: "Starting…",
    waiting: "Play something in Spotify…",
    idle: "On: Spotify is playing through Stemify",
    ready: "On: Spotify is playing through Stemify",
  };

  let engine = $state<EngineState | null>(null);

  onMount(() => {
    invoke<EngineState>("get_state").then((s) => (engine = s));
    const unlisten = listen<EngineState>("engine-status", (e) => (engine = e.payload));
    return () => {
      unlisten.then((stop) => stop());
    };
  });

  async function togglePower() {
    if (engine) engine = await invoke<EngineState>("set_enabled", { enabled: !engine.enabled });
  }
  async function setPitch(semitones: number) {
    engine = await invoke<EngineState>("set_pitch", { semitones });
  }
  async function setStems(stems: string[]) {
    engine = await invoke<EngineState>("set_stems", { stems });
  }
  async function toggleStem(stem: string) {
    if (!engine) return;
    const stems = engine.stems.includes(stem) ? engine.stems.filter((s) => s !== stem) : [...engine.stems, stem];
    await setStems(stems);
  }
  async function setKeepModel(keep: boolean) {
    engine = await invoke<EngineState>("set_keep_model", { keep });
  }

  function preset(stems: string[]): "all" | "vocals" | "backing" | null {
    const set = new Set(stems);
    if (set.size === ALL_STEMS.length) return "all";
    if (set.size === 1 && set.has("vocals")) return "vocals";
    if (set.size === BACKING_STEMS.length && BACKING_STEMS.every((s) => set.has(s))) return "backing";
    return null;
  }
</script>

<main>
  <div class="titlebar" data-tauri-drag-region>
    <button class="winbtn" aria-label="Minimize" onclick={() => win.minimize()}>
      <Minus size={14} />
    </button>
    <button class="winbtn close" aria-label="Close" onclick={() => win.close()}>
      <X size={14} />
    </button>
  </div>
  {#if engine}
    <header>
      <div class="now-playing">
        {#if engine.spotify_connected}
          <span class="label">Now Playing</span>
        {/if}
        <div class="track">
          {#if engine.track?.art}
            <img class="art" src={engine.track.art} alt="" />
          {:else}
            <div class="art placeholder">
              <Music2 size={18} strokeWidth={1.5} />
            </div>
          {/if}
          <div class="meta">
            <p class="title" class:placeholder={!engine.track}>
              {engine.track?.title ?? (engine.spotify_connected ? "Nothing playing" : "Spotify not connected")}
            </p>
            <p class="artist">{engine.track?.artist ?? " "}</p>
          </div>
        </div>
      </div>
      <button
        class="power"
        class:on={engine.enabled}
        disabled={!engine.spotify_connected}
        onclick={togglePower}
        aria-label={engine.enabled ? "Turn off" : "Turn on"}
      >
        <Power size={16} strokeWidth={1.5} />
      </button>
    </header>

    <p class="status {engine.status}">{STATUS_TEXT[engine.status]}</p>
    {#if engine.error}
      <p class="error">{engine.error}</p>
    {/if}

    <section class:dim={engine.status === "loading"}>
      <h2>Transpose: {engine.semitones > 0 ? "+" : ""}{engine.semitones}</h2>
      <div class="row">
        <button class="step" onclick={() => setPitch(engine!.semitones - 1)} aria-label="Lower by a semitone">
          <Minus size={16} />
        </button>
        <input
          type="range"
          min="-12"
          max="12"
          value={engine.semitones}
          onchange={(e) => setPitch(Number((e.target as HTMLInputElement).value))}
        />
        <button class="step" onclick={() => setPitch(engine!.semitones + 1)} aria-label="Raise by a semitone">
          <Plus size={16} />
        </button>
      </div>

      <h2>Mix</h2>
      <div class="presets">
        <button class="pill" class:selected={preset(engine.stems) === "all"} onclick={() => setStems(ALL_STEMS)}>
          All
        </button>
        <button class="pill" class:selected={preset(engine.stems) === "vocals"} onclick={() => setStems(["vocals"])}>
          Vocals
        </button>
        <button class="pill" class:selected={preset(engine.stems) === "backing"} onclick={() => setStems(BACKING_STEMS)}>
          Backing
        </button>
      </div>
      <div class="tiles">
        {#each ALL_STEMS as stem}
          <button class="tile" class:selected={engine.stems.includes(stem)} onclick={() => toggleStem(stem)}>
            <span class="tile-icon">{@html STEM_ICONS[stem as keyof typeof STEM_ICONS]}</span>
            <span>{stem}</span>
          </button>
        {/each}
      </div>

      <div class="keep-row">
        <span
          class="keep-label"
          title="Turning this off may increase the startup time of the stem functions, as the model will need to reload."
        >
          Keep the stem model in GPU memory when off
        </span>
        <label class="switch">
          <input
            type="checkbox"
            checked={engine.keep_model}
            onchange={(e) => setKeepModel((e.target as HTMLInputElement).checked)}
          />
          <span class="slider"></span>
        </label>
      </div>
    </section>
  {:else}
    <p>Connecting…</p>
  {/if}
</main>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    background: transparent; 
    color: #f2f2f2;
    font-family: system-ui, sans-serif;
  }
  main {
    box-sizing: border-box;
    min-height: 100vh;
    padding: 0 20px 20px;
    background: rgba(20, 20, 24, 0.82);
  }
  .titlebar {
    display: flex;
    justify-content: flex-end;
    gap: 4px;
    height: 32px;
    margin: 0 -20px 4px;
  }
  .winbtn {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 40px;
    height: 32px;
    padding: 0;
    background: transparent;
    border: none;
    border-radius: 0;
    color: #a0a0a8;
  }
  .winbtn:hover {
    background: #34343a;
  }
  .winbtn.close:hover {
    background: #e0403f;
    color: white;
  }
  header {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding-top: 4px;
  }
  .now-playing {
    min-width: 0; 
  }
  .label {
    display: block;
    font-size: 11px;
    font-weight: 500;
    letter-spacing: 0.03em;
    text-transform: uppercase;
    color: #6e6e78;
    margin: 0 0 6px;
  }
  .track {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
  }
  .art {
    width: 40px;
    height: 40px;
    border-radius: 8px;
    object-fit: cover;
    flex-shrink: 0;
  }
  .art.placeholder {
    display: flex;
    align-items: center;
    justify-content: center;
    background: #2b2b30;
    border: 1px solid #3a3a41;
    color: #5a5a63;
  }
  .meta {
    min-width: 0;
  }
  .title,
  .artist {
    margin: 0;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .title {
    font-size: 14px;
    font-weight: 600;
  }
  .title.placeholder {
    font-weight: 500;
    color: #a0a0a8;
  }
  .artist {
    font-size: 12px;
    color: #a0a0a8;
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
  button:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
  .power {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    padding: 0;
    border-radius: 50%;
    flex-shrink: 0;
    color: #a0a0a8;
  }
  .power.on {
    border-color: #2f6df6;
    color: #2f6df6;
  }
  .status {
    margin: 6px 0 0;
    font-size: 13px;
  }
  .status.loading,
  .status.waiting {
    color: #e6b34a;
  }
  .status.idle,
  .status.ready {
    color: #5ecb7a;
  }
  .status.off {
    color: #8a8a92;
  }
  .error {
    margin: 6px 0 0;
    font-size: 12px;
    color: #f0736a;
  }
  .row {
    display: flex;
    gap: 10px;
    align-items: center;
  }
  .step {
    display: flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    padding: 0;
    border-radius: 50%;
    flex-shrink: 0;
  }
  input[type="range"] {
    flex: 1;
    -webkit-appearance: none;
    appearance: none;
    height: 4px;
    border-radius: 999px;
    background: #3a3a41;
  }
  input[type="range"]::-webkit-slider-thumb {
    -webkit-appearance: none;
    width: 16px;
    height: 16px;
    border-radius: 50%;
    background: #2f6df6;
    cursor: pointer;
  }
  .presets {
    display: flex;
    gap: 6px;
    margin-bottom: 10px;
  }
  .pill {
    flex: 1;
    padding: 6px 0;
    font-size: 12px;
    text-align: center;
    border-radius: 999px;
    color: #a0a0a8;
  }
  .pill.selected {
    border-color: #2f6df6;
    color: #f2f2f2;
    background: #26304a;
  }
  .tiles {
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    gap: 8px;
  }
  .tile {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 6px;
    padding: 14px 4px;
    text-transform: capitalize;
    color: #8a8a92;
  }
  .tile-icon :global(svg) {
    width: 18px;
    height: 18px;
    display: block;
  }
  .tile.selected {
    border-color: #2f6df6;
    color: #f2f2f2;
    background: #26304a;
    box-shadow: 0 0 0 1px rgba(47, 109, 246, 0.4), 0 0 12px rgba(47, 109, 246, 0.35);
  }
  .keep-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    margin-top: 18px;
  }
  .keep-label {
    font-size: 12px;
    color: #a0a0a8;
    cursor: help;
  }
  .switch {
    position: relative;
    display: inline-block;
    width: 36px;
    height: 20px;
    flex-shrink: 0;
  }
  .switch input {
    opacity: 0;
    width: 0;
    height: 0;
  }
  .slider {
    position: absolute;
    inset: 0;
    background: #3a3a41;
    border-radius: 999px;
    cursor: pointer;
    transition: background 0.15s;
  }
  .slider::before {
    content: "";
    position: absolute;
    width: 16px;
    height: 16px;
    left: 2px;
    top: 2px;
    background: white;
    border-radius: 50%;
    transition: transform 0.15s;
  }
  .switch input:checked + .slider {
    background: #2f6df6;
  }
  .switch input:checked + .slider::before {
    transform: translateX(16px);
  }
  .dim {
    opacity: 0.45;
    transition: opacity 0.2s;
  }
</style>
