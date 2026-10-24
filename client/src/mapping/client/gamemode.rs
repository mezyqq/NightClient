use crate::mapping::block::Direction;
use crate::mapping::entity::player::LocalPlayer;
use crate::mapping::entity::Entity;
use crate::mapping::math::{BlockPos, Vec3};
use crate::mapping::{FieldType, MappedObject, MinecraftClassType};
use crate::state::mapping;
use jni::objects::{GlobalRef, JValue};

#[derive(Debug, Clone, MappedObject)]
#[mapped(class = MultiPlayerGameMode)]
pub struct MultiPlayerGameMode {
    jni_ref: GlobalRef,
}

impl MultiPlayerGameMode {
    pub fn new(jni_ref: GlobalRef) -> Self {
        Self { jni_ref }
    }

    /// Attacks `target` on behalf of the local player.
    pub fn attack(&self, player: &LocalPlayer, target: &Entity) -> anyhow::Result<()> {
        self.call_method(
            "attack",
            &[
                JValue::Object(player.jni_ref().as_obj()),
                JValue::Object(target.jni_ref().as_obj()),
            ],
        )?;
        Ok(())
    }

    /// Issues a container click with the named click action (`PICKUP`,
    /// `QUICK_MOVE`, `SWAP`, …) — sends it to the server and applies it
    /// client-side.
    ///
    /// The API was renamed in 26.1: `handleContainerInput` + `ContainerInput`
    /// there, `handleInventoryMouseClick` + `ClickType` before. Both are tried,
    /// with explicit descriptors, so this works on either side of the rename
    /// whatever version the mapping layer believes it runs.
    pub fn container_click(
        &self,
        container_id: i32,
        slot: i32,
        button: i32,
        action: &str,
        player: &LocalPlayer,
    ) -> anyhow::Result<()> {
        const VARIANTS: [(&str, &str); 2] = [
            ("handleContainerInput", "net/minecraft/world/inventory/ContainerInput"),
            ("handleInventoryMouseClick", "net/minecraft/world/inventory/ClickType"),
        ];
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            for (method, class_name) in VARIANTS {
                let Ok(class) = mapping().resolve_class(&mut env, class_name) else {
                    continue;
                };
                let kind = env
                    .get_static_field(&class, action, format!("L{class_name};"))
                    .inspect_err(|_| {
                        let _ = env.exception_clear();
                    })?
                    .l()?;
                let sig = format!("(IIIL{class_name};Lnet/minecraft/world/entity/player/Player;)V");
                return crate::mapping::raw::call(
                    &mut env,
                    self.jni_ref().as_obj(),
                    method,
                    &sig,
                    &[
                        JValue::Int(container_id),
                        JValue::Int(slot),
                        JValue::Int(button),
                        JValue::Object(&kind),
                        JValue::Object(player.jni_ref().as_obj()),
                    ],
                )
                .map(|_| ());
            }
            Err(anyhow::anyhow!("no container click API found"))
        })
    }

    /// PICKUP click with the left button: picks up / places / swaps / merges
    /// the stack of menu slot `slot` with the carried one.
    pub fn container_pickup(&self, container_id: i32, slot: i32, player: &LocalPlayer) -> anyhow::Result<()> {
        self.container_click(container_id, slot, 0, "PICKUP", player)
    }

    /// SWAP click: swaps menu slot `slot` of the container `container_id` with
    /// an inventory hotbar slot (`hotbar_button` 0-8) or the off-hand (40).
    pub fn container_swap(
        &self,
        container_id: i32,
        slot: i32,
        hotbar_button: i32,
        player: &LocalPlayer,
    ) -> anyhow::Result<()> {
        self.container_click(container_id, slot, hotbar_button, "SWAP", player)
    }

    /// QUICK_MOVE click (shift-click): moves menu slot `slot`'s stack to its
    /// default destination — into the inventory, or onto an equipment slot.
    pub fn container_quick_move(
        &self,
        container_id: i32,
        slot: i32,
        player: &LocalPlayer,
    ) -> anyhow::Result<()> {
        self.container_click(container_id, slot, 0, "QUICK_MOVE", player)
    }

    /// Calls a `(BlockPos, Direction)` block-breaking method on the game mode.
    fn destroy_block(&self, method: &str, pos: &BlockPos, face: Direction) -> anyhow::Result<()> {
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let pos_obj = pos.to_java(&mut env)?;
            let face_obj = face.to_java()?;
            self.call_method(
                method,
                &[JValue::Object(&pos_obj), JValue::Object(&face_obj)],
            )?;
            Ok(())
        })
    }

    /// Begins breaking the block at `pos`. Instant in creative; in survival it
    /// must be followed by [`continue_destroy_block`](Self::continue_destroy_block)
    /// on later ticks until the block breaks.
    pub fn start_destroy_block(&self, pos: &BlockPos, face: Direction) -> anyhow::Result<()> {
        self.destroy_block("startDestroyBlock", pos, face)
    }

    /// Advances breaking the block at `pos` — survival mining progress.
    pub fn continue_destroy_block(&self, pos: &BlockPos, face: Direction) -> anyhow::Result<()> {
        self.destroy_block("continueDestroyBlock", pos, face)
    }

    /// Aborts any block currently being broken — the server is told the player
    /// stopped mining.
    pub fn stop_destroy_block(&self) -> anyhow::Result<()> {
        self.call_method("stopDestroyBlock", &[])?;
        Ok(())
    }

    /// Uses the item in `hand` (`"MAIN_HAND"` / `"OFF_HAND"`) against the
    /// `face` face of the block at `anchor`, hitting it at world point `hit` —
    /// placing a block at `anchor + face` when that item is a block.
    pub fn place_block_on(
        &self,
        player: &LocalPlayer,
        hand: &str,
        anchor: &BlockPos,
        face: Direction,
        hit: Vec3,
    ) -> anyhow::Result<()> {
        self.in_frame(|| {
            let mut env = mapping().get_env()?;
            let hit_obj = hit.to_java(&mut env)?;
            let anchor_obj = anchor.to_java(&mut env)?;
            let face_obj = face.to_java()?;
            let block_hit_class =
                mapping().resolve_class(&mut env, MinecraftClassType::BlockHitResult.get_name())?;
            let block_hit = env.new_object(
                &block_hit_class,
                "(Lnet/minecraft/world/phys/Vec3;Lnet/minecraft/core/Direction;\
                 Lnet/minecraft/core/BlockPos;Z)V",
                &[
                    JValue::Object(&hit_obj),
                    JValue::Object(&face_obj),
                    JValue::Object(&anchor_obj),
                    JValue::Bool(0),
                ],
            )?;
            let hand = mapping()
                .get_static_field(
                    MinecraftClassType::InteractionHand,
                    hand,
                    FieldType::Object(MinecraftClassType::InteractionHand),
                )?
                .l()?;
            self.call_method(
                "useItemOn",
                &[
                    JValue::Object(player.jni_ref().as_obj()),
                    JValue::Object(&hand),
                    JValue::Object(&block_hit),
                ],
            )?;
            Ok(())
        })
    }
}
