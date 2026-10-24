# AGENTS.md

Guidance for coding agents working on DarkClient. The architecture — crates,
injection flow, mapping system, packet layer — is described in depth in
[`CLAUDE.md`](CLAUDE.md); read it first. This file covers the practical side:
how the project is built, run and tested here, and the rules that keep changes
working on the game the user actually plays.

## Build, install, run

```bash
cargo build -p client --release     # the crate almost every change touches
cargo build --release               # whole workspace (injector, agent, client)
~/src/darkc/install.sh              # builds everything + installs (see below)
```

- **Always build `--release`.** That is what gets injected.
- `~/src/darkc/install.sh` builds DarkClient and the `darkC` terminal launcher,
  then installs `injector`, `libagent_loader.so` and `libclient.so` into
  `~/.local/share/darkclient/` and `darkC` into `~/.local/bin/`.
- The user injects with `darkC` → **Inject**. Injecting again hot-reloads the
  client in the running game; no restart needed.
- Injection needs `sudo` (ptrace). **Never run the injector yourself** — it
  prompts for the user's password and attaches to their live game. Hand the
  step to the user.
- Config, rule files and logs live in `~/.local/share/darkclient/`:
  `dark_client.log` (client, overwritten on every inject — read it *before*
  asking for a re-inject), `agent_loader.log`, `app.log`,
  `dark_client_config.json` and the per-feature JSON files listed below.

## Testing

- There is **no JDK** on this machine, so `cargo test -p client` fails at link
  time (`undefined symbol: JNI_GetCreatedJavaVMs`). Tests that need a JVM
  cannot run here.
- Therefore: **keep logic pure and unit-test it.** Put the decision-making
  (planners, parsers, math, glob matching, route building) in plain functions
  over plain data, with `#[cfg(test)]` tests next to them, and keep JNI in thin
  readers / executors. Examples: `module/world/autosort.rs` (click planner),
  `module/misc/tptodeath.rs` (sky route), `module/render/trajectories.rs`
  (ballistics), `module/render/block_esp.rs` (bit-storage unpacking).
- To run such tests without a JVM, copy the pure items and their test module
  into a scratch `.rs` file and run `rustc --edition 2021 --test`.
- Nothing can be verified in-game by an agent. Say so plainly when reporting.
- **Game hung?** Take a thread dump — read-only, no sudo:
  `~/.local/share/ElyPrismLauncher/java/java-runtime-delta/bin/jstack <pid>`.
  A `glfwSwapBuffers` frame nested inside another (via a game call made
  from `on_frame`) is a re-entered frame hook; `on_frame` skips nested
  passes (`FrameGuard`) so they cannot deadlock on the outer pass's locks.

## The runtime that matters: NeoForge 1.21.1

The user plays **Minecraft 1.21.1 on NeoForge** (Sodium, Free Camera API,
Baritone and ~70 other mods). NeoForge runs with Mojmap names, so the client
takes the *reflected* mapping path — but that path assumes the **latest**
version, so the legacy names in `mapping/renames.rs` are **not** applied.
APIs renamed after 1.21.1 silently fail through the mapping layer.

Rules for anything version-sensitive:

- Prefer **direct JNI with explicit Mojmap descriptors** through
  `mapping/raw.rs` (`raw::call`, `raw::field`, `raw::set_field`,
  `raw::to_string` — all clear the pending Java exception on failure).
- **Try the new name, then the old one** (or vice versa), never assume one
  version. Known splits, all handled already — reuse the helpers:

  | What | 1.21.1 | newer |
  |---|---|---|
  | container click | `handleInventoryMouseClick` + `ClickType` | `handleContainerInput` + `ContainerInput` — see `MultiPlayerGameMode::container_click` |
  | move-player packet ctors | `(DDDFFZ)V` … (no `horizontalCollision`) | `(DDDFFZZ)V` … — `net/packet/serverbound_move_player.rs` |
  | entity-motion packet | `getId()`, `getXa/Ya/Za()` | record `id()`, `movement()` |
  | selected hotbar slot | field `Inventory.selected` | `getSelectedSlot` / `setSelectedSlot` — `module/player/hands.rs` |
  | lowest section | `getMinSection()` | `getMinSectionY()` |
  | build height | `getMaxBuildHeight()` | `getMaxY()` |
  | `ServerPlayer.teleportTo` | `(ServerLevel,DDDFF)V` | `(ServerLevel,DDD,Set,FF,Z)Z` |
  | `ConnectScreen.startConnecting` | `…ServerData;ZLTransferState;)V` | same; pre-1.20.5 without `TransferState` |

- Parse identifiers from `toString()` when an accessor was renamed: block ids
  from `BlockState` (`Block{minecraft:wheat}[age=7]`), dimensions from
  `ResourceKey` (`ResourceKey[minecraft:dimension / minecraft:the_nether]`),
  entity types from `getDescriptionId()`. `Item.toString()` is a bare path
  before 1.20.5 — normalize with `hands::normalize_id`.
- Game classes must be looked up through the game class loader:
  `mapping().resolve_class(env, "net/minecraft/…")`, never `env.find_class`
  (that only works for `java.*`).

## Adding a module

1. Add a variant to `ModuleId` and its `display_name` (`module/mod.rs`).
2. Write the module in the right category folder and `pub mod` it in that
   folder's `mod.rs`.
3. Register it in `register_modules()` (`lib.rs`).
4. Settings are `ModuleSetting`s (Toggle / Slider / Choice / Color) and are
   persisted by name. Choice settings render with an egui id built from the
   setting **name**, so give different modules' choices distinct names.

Things to know:

- **Modules only tick in a world with a living player** (`set_active`). Work
  that must happen on the death screen, the disconnect screen or right on
  joining goes in a per-frame hook instead — see `module/player/session.rs`
  (AutoRespawn, AutoReconnect, AutoCommand), called from `graphic/hook.rs`.
- A module that is an **action**, not a state (TP to Death), calls
  `module::registry::request_disable(id)`; it is switched off on the next tick,
  outside its own lock.
- `on_start` / `on_stop` / `on_tick` run **with the module's lock held**.
  Background threads must never lock their own module (pass values through
  atomics / statics), and must be **joined in `on_stop`** — the library may be
  unloaded right after (hot reload). See `module/render/block_esp.rs`.
- Per-frame work (camera, ambience) is called from `on_frame` in
  `graphic/hook.rs`; keep it to a handful of JNI calls. Heavy scans run on a
  thread or at a throttled rate.
- Lists that do not fit a setting (block / player / item lists, rules) are
  stored as JSON in `config::base_dir()`: `block_esp.json`,
  `aura_targets.json`, `friends.json`, `auto_commands.json` (plain text — may
  hold passwords), `autosort.json`, `macros.json`, `optimization.json`,
  `optimization_originals.json`.
- **Optimization** modules (`module/optimization/`) are different: declared
  with `optimization_module!`, empty `on_*`, and driven every frame by
  `optimization::frame()` from the frame hook — they act whenever enabled
  (menu, death screen too) and undo their changes once disabled.
  `optimization::cleanup()` runs from `cleanup_client`. Shared pieces:
  `options.rs` (`want(key, value)` per frame → `commit()` applies, records
  originals in `optimization_originals.json`, restores what is no longer
  wanted; clamp values — `OptionInstance.set` resets out-of-range values to
  the default), `java.rs` (method-handle predicates / NeoForge event-bus
  listeners, so per-object work stays in the JVM), `lists.rs`
  (`optimization.json`). Never hide the firework spark particle through its
  provider — the firework starter dereferences it without a null check.
- `raw::call` / `field` / `set_field` and `Mapping::get_field` / `set_field`
  resolve member ids once per (runtime class, name, descriptor) through
  `mapping/member_cache.rs` — misses are cached too, so "try the new name,
  then the old one" fallbacks are cheap after the first call. It validates
  arguments against the descriptor (an unchecked call with wrong ones would
  corrupt the JVM); keep descriptors exact.
- **Workspaces** (`graphic/workspace.rs`): every category panel and tool
  window lives on workspace 1 or 2 (`ui_value` `workspace:<key>`, default 1).
  A new panel or window must call `workspace::opacity` (skip at 0, multiply
  into `progress`) and register its layer with `drawn_panel` /
  `drawn_window` — `tool_window` already does both.
- Tool windows (searchable lists, rule editors) live in `graphic/windows.rs`
  and `graphic/autosort_window.rs`; a window shows while its module is enabled
  or expanded (`wanted()`), only with the ClickGUI open. Always open them
  through `tool_window`: it restores and records the window position and
  collapsed state in the config (keyed by title). Other GUI state worth
  keeping (a chosen tab, a selected entry) goes through
  `config::ui_value` / `config::set_ui_value`. Layout changes are written by
  `config::autosave_layout` ~1 s after they settle — layout keys only — so
  they survive a Panic, a re-inject with the menu open, or a crash.
- egui's bundled fonts lack some symbols (`✕`, `⇄` render as boxes). Use `×`,
  `▲ ▼ ▶ →`, or check the glyph against the fonts in
  `~/.cargo/registry/src/*/epaint_default_fonts-*/fonts/` first.
- Macros (`module/misc/macros.rs`) record at the GLFW-callback level and play
  back through `graphic::input::replay` / `replay_cursor`, timed on the game
  tick clock (`graphic::hook::game_clock`); cursor events carry both an offset
  (captured cursor) and a centre-relative position (free cursor, in screens); any new input path must call
  `macros::record` before forwarding and respect `macros::playing_now()`.
- **Spin** (`module/render/spin.rs`) shows the pattern for changing how an
  entity renders without native code in the render path: a
  `RenderLivingEvent.Pre` listener built from method handles
  (`java::is_test`, `java::set_float_from`) reads a live Java `float[1]`
  that Rust rewrites per frame. `spin::cleanup()` runs from `cleanup_client`.
- **World→screen projection** (`graphic/esp.rs` `View::project`) uses the
  game's own `projection × model-view` and camera position of the frame,
  copied by a `RenderLevelStageEvent` listener (`graphic/frame_matrices.rs`)
  — so zoom mods, Camera Overhaul roll and view bobbing line up. It falls
  back to yaw / pitch / FOV only when no level render was captured. Anything
  drawn over the world must go through `View::project`.
- **Damage tracker** (`module/render/hit_effects.rs`): HitMarker,
  DamageNumbers and TargetHUD share one per-tick health scan of nearby
  living entities (`hit_effects::tick`, deduplicated by game tick). A hit is
  "yours" when its entity was under the crosshair or the aura's target
  (`combat::set_combat_target`) within the last second.
- **World overlays outside `esp.rs`** (ItemESP, damage numbers) take the
  ESP's projection as a `&dyn Fn([f64; 3]) -> Option<Pos2>`; screen overlays
  (Crosshair, HitMarker, TargetHUD) are called from `gui::render_all` and use
  `crosshair::world_view_visible()` (in world, first person, no screen, no F1).
- The ClickGUI has a module search (`menu::matches`, toolbar field, Ctrl+F or
  just type); it is cleared each time the menu opens.
- **Panic** goes through `ui_engine::request_panic()` (once, on its own
  thread — never call `call_panic` from a hook directly). Bound to
  **Ctrl+End** in `on_key`, before every other key handling. **Hide HUD**
  (`gui::hud_hidden`, ui value `hide_hud`) skips ESP, HUD and notifications;
  anything new drawn outside the ClickGUI must respect it.
- Input hooks are in `graphic/input.rs`. Mouse buttons 3–8 are bindable as
  `MOUSE_BUTTON_BASE + button`; Spectate and Freelook capture the cursor there.
- Every JNI scope goes inside `mapping().in_frame(..)` or
  `env.with_local_frame(..)` — unbounded local references cost frame rate.

## Style

- Match the surrounding code: module-level `//!` docs explaining *how* a
  feature works and its limits, `///` on items, comments only for the
  non-obvious. Keep `cargo build` free of warnings.
- Be honest in docs and replies about what cannot work: a client cannot
  change server-side health, teleport across dimensions on a remote server,
  or hide from a server-side anti-cheat.

## The user's machine

- Arch Linux, KDE Plasma (Wayland), Intel Alder Lake iGPU, 15 GB RAM.
- **Never run display-, session- or power-affecting commands** (`dpms`,
  `loginctl`, `systemctl suspend`, `powerprofilesctl set`, restarting
  PowerDevil, …) on the live desktop — not even to probe syntax. Reading files
  under `/sys` and `/var/lib` is fine. Suggest the command; the user runs it.
- Talk to the user in Russian; code, comments and docs stay in English.
