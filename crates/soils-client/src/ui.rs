//! What the UI is currently doing, and who owns the mouse cursor.
//!
//! Before this existed the client inferred its UI state from the cursor: the
//! pause menu was shown whenever the pointer was free, and any click re-grabbed
//! it. That makes a second full-screen UI impossible — opening one would also
//! open the pause menu, and clicking anything in it would lock the pointer and
//! dismiss it. See `docs/plan-ui.md`.
//!
//! So the mode is explicit here, and cursor grab is a *consequence* of it.
//! [`apply_cursor_mode`] is the only writer of `CursorOptions`.

use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

/// What the player is looking at.
#[derive(States, Default, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum UiMode {
    /// Playing: pointer locked, mouse-look and editing live.
    #[default]
    Playing,
    /// Pause / settings menu.
    Menu,
    /// Full-screen inventory.
    Inventory,
}

impl UiMode {
    /// Whether the pointer should be free in this mode.
    pub fn wants_cursor(self) -> bool {
        !matches!(self, UiMode::Playing)
    }
}

/// Held-Alt override: frees the pointer without leaving the current mode, so
/// the ring and HUD are mouse-reachable during play.
///
/// Deliberately not a [`UiMode`] variant. Alt is a modifier over whatever is
/// already happening; making it a mode would mean defining what
/// "Alt + Inventory" is, and every gameplay `run_if` would have to accept two
/// states instead of one.
#[derive(Resource, Default)]
pub struct CursorFreed(pub bool);

/// Run condition: gameplay input (look, edit, movement) is live.
pub fn playing(mode: Res<State<UiMode>>, freed: Res<CursorFreed>) -> bool {
    *mode.get() == UiMode::Playing && !freed.0
}

/// Alt frees the pointer for as long as it is held.
pub fn track_alt(keys: Res<ButtonInput<KeyCode>>, mut freed: ResMut<CursorFreed>) {
    let held = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if freed.0 != held {
        freed.0 = held;
    }
}

/// E / I / Tab open and close the inventory; Escape backs out.
///
/// Escape closes whatever is open and only reaches the pause menu when nothing
/// else is — the design note lists Escape as an inventory key too, but it is
/// the one key that must always mean "get me out of here", and E/I/Tab already
/// give the inventory three bindings.
pub fn ui_hotkeys(
    keys: Res<ButtonInput<KeyCode>>,
    mode: Res<State<UiMode>>,
    mut next: ResMut<NextState<UiMode>>,
) {
    let toggle_inventory =
        keys.any_just_pressed([KeyCode::KeyE, KeyCode::KeyI, KeyCode::Tab]);
    if toggle_inventory {
        next.set(match mode.get() {
            UiMode::Inventory => UiMode::Playing,
            _ => UiMode::Inventory,
        });
        return;
    }
    if keys.just_pressed(KeyCode::Escape) {
        next.set(match mode.get() {
            UiMode::Playing => UiMode::Menu,
            _ => UiMode::Playing,
        });
    }
}

/// The single owner of cursor grab. Runs every frame rather than on state
/// transitions so the Alt override and a mode change cannot disagree.
pub fn apply_cursor_mode(
    mode: Res<State<UiMode>>,
    freed: Res<CursorFreed>,
    mut cursor: Query<&mut CursorOptions, With<PrimaryWindow>>,
) {
    let Ok(mut cursor) = cursor.single_mut() else { return };
    let want_free = mode.get().wants_cursor() || freed.0;
    let (grab, visible) =
        if want_free { (CursorGrabMode::None, true) } else { (CursorGrabMode::Locked, false) };
    if cursor.grab_mode != grab {
        cursor.grab_mode = grab;
    }
    if cursor.visible != visible {
        cursor.visible = visible;
    }
}

/// Clicking the world re-grabs the pointer after an Alt-release — but only
/// while `Playing`. Re-grabbing in a menu would lock the pointer the moment
/// the player clicked a button, which is the bug this module exists to remove.
pub fn click_to_grab(
    buttons: Res<ButtonInput<MouseButton>>,
    mode: Res<State<UiMode>>,
    mut freed: ResMut<CursorFreed>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    if *mode.get() == UiMode::Playing
        && !alt
        && freed.0
        && buttons.any_just_pressed([MouseButton::Left, MouseButton::Right])
    {
        freed.0 = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the hotkey system over a bare app, with no window or renderer.
    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin);
        app.init_state::<UiMode>();
        app.init_resource::<CursorFreed>();
        app.init_resource::<ButtonInput<KeyCode>>();
        app.init_resource::<ButtonInput<MouseButton>>();
        app.add_systems(Update, (ui_hotkeys, track_alt, click_to_grab));
        app
    }

    /// One key tap.
    ///
    /// The second `update` is not padding: `NextState` set during `Update` is
    /// applied by the `StateTransition` schedule on the *following* frame. The
    /// `clear` between them is not padding either — `bevy_input` normally
    /// retires `just_pressed` each frame and is absent here, so without it the
    /// key reads as freshly pressed again and every tap toggles twice.
    fn press(app: &mut App, key: KeyCode) {
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(key);
        app.update();
        {
            let mut input = app.world_mut().resource_mut::<ButtonInput<KeyCode>>();
            input.release(key);
            input.clear();
        }
        app.update();
    }

    fn mode(app: &App) -> UiMode {
        *app.world().resource::<State<UiMode>>().get()
    }

    #[test]
    fn each_inventory_key_opens_and_closes_it() {
        for key in [KeyCode::KeyE, KeyCode::KeyI, KeyCode::Tab] {
            let mut app = app();
            assert_eq!(mode(&app), UiMode::Playing);
            press(&mut app, key);
            assert_eq!(mode(&app), UiMode::Inventory, "{key:?} must open the inventory");
            press(&mut app, key);
            assert_eq!(mode(&app), UiMode::Playing, "{key:?} must close it again");
        }
    }

    #[test]
    fn escape_backs_out_of_the_inventory_rather_than_pausing() {
        let mut app = app();
        press(&mut app, KeyCode::KeyE);
        assert_eq!(mode(&app), UiMode::Inventory);
        press(&mut app, KeyCode::Escape);
        assert_eq!(mode(&app), UiMode::Playing, "escape must close, not pause");
    }

    #[test]
    fn escape_opens_the_menu_only_when_nothing_else_is_open() {
        let mut app = app();
        press(&mut app, KeyCode::Escape);
        assert_eq!(mode(&app), UiMode::Menu);
        press(&mut app, KeyCode::Escape);
        assert_eq!(mode(&app), UiMode::Playing);
    }

    #[test]
    fn alt_frees_the_cursor_without_leaving_play() {
        let mut app = app();
        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().press(KeyCode::AltLeft);
        app.update();
        assert!(app.world().resource::<CursorFreed>().0, "alt must free the pointer");
        assert_eq!(mode(&app), UiMode::Playing, "alt is a modifier, not a mode");

        app.world_mut().resource_mut::<ButtonInput<KeyCode>>().release(KeyCode::AltLeft);
        app.update();
        assert!(!app.world().resource::<CursorFreed>().0, "releasing alt re-grabs");
    }

    /// The regression that makes an inventory screen unusable: a click on a
    /// slot re-locking the pointer and dismissing the screen underneath it.
    /// Invisible to a test that only checks state transitions.
    #[test]
    fn clicking_inside_the_inventory_does_not_re_grab_the_cursor() {
        let mut app = app();
        press(&mut app, KeyCode::KeyE);
        assert_eq!(mode(&app), UiMode::Inventory);

        app.world_mut().resource_mut::<ButtonInput<MouseButton>>().press(MouseButton::Left);
        app.update();

        assert_eq!(mode(&app), UiMode::Inventory, "a click must not close the inventory");
        assert!(
            mode(&app).wants_cursor(),
            "the pointer must stay free while the inventory is open"
        );
    }

    #[test]
    fn a_click_in_play_takes_the_cursor_back_after_alt() {
        let mut app = app();
        app.world_mut().resource_mut::<CursorFreed>().0 = true;
        app.world_mut().resource_mut::<ButtonInput<MouseButton>>().press(MouseButton::Left);
        app.update();
        assert!(!app.world().resource::<CursorFreed>().0, "clicking the world re-grabs");
    }

    #[test]
    fn gameplay_is_live_only_while_playing_with_the_cursor_held() {
        let mut app = app();
        assert!(app.world_mut().run_system_cached(playing).unwrap());
        app.world_mut().resource_mut::<CursorFreed>().0 = true;
        assert!(!app.world_mut().run_system_cached(playing).unwrap(), "alt suspends editing");
    }
}
