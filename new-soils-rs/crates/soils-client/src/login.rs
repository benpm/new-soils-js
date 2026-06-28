//! Login / signup screen shown before the world loads. Mirrors the JS login
//! menu: pick a username (and optional password), then Log in or Sign up. The
//! game world stays gated behind `logged_in` until the server replies `Init`.

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};
use soils_protocol::ClientMsg;

use crate::net::NetClient;

#[derive(Default, PartialEq, Clone, Copy)]
pub enum Field {
    #[default]
    Name,
    Password,
}

/// Login form state. `done` flips true once the server accepts us (`Init`).
#[derive(Resource, Default)]
pub struct LoginState {
    pub name: String,
    pub password: String,
    pub focus: Field,
    pub status: String,
    pub done: bool,
}

/// Run condition: the game world only runs once logged in.
pub fn logged_in(login: Res<LoginState>) -> bool {
    login.done
}

#[derive(Component)]
pub(crate) struct LoginScreen;
#[derive(Component, Clone, Copy)]
pub(crate) enum LoginButton {
    FocusName,
    FocusPassword,
    Login,
    Signup,
}
#[derive(Component)]
pub(crate) struct NameText;
#[derive(Component)]
pub(crate) struct PasswordText;
#[derive(Component)]
pub(crate) struct StatusText;

const PANEL_BG: Color = Color::srgba(0.05, 0.06, 0.08, 0.92);
const FIELD_BG: Color = Color::srgba(0.15, 0.16, 0.20, 1.0);
const BTN_BG: Color = Color::srgba(0.20, 0.34, 0.46, 1.0);

/// Spawn the login screen (skipped entirely in self-test, which auto-logs in).
pub fn setup_login(mut commands: Commands) {
    // Self-test auto-logs in, so the screen is skipped — unless SOILS_LOGINSHOT
    // forces it up for a screenshot.
    if std::env::var("SOILS_SELFTEST").is_ok() && std::env::var("SOILS_LOGINSHOT").is_err() {
        return;
    }
    commands
        .spawn((
            LoginScreen,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                justify_content: JustifyContent::Center,
                align_items: AlignItems::Center,
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.5)),
        ))
        .with_children(|root| {
            root.spawn((
                Node {
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::Center,
                    row_gap: Val::Px(12.0),
                    padding: UiRect::all(Val::Px(28.0)),
                    ..default()
                },
                BackgroundColor(PANEL_BG),
            ))
            .with_children(|panel| {
                panel.spawn((
                    Text::new("new-soils"),
                    TextFont { font_size: 30.0, ..default() },
                    TextColor(Color::WHITE),
                ));
                field(panel, "Username", LoginButton::FocusName, NameText);
                field(panel, "Password", LoginButton::FocusPassword, PasswordText);
                panel
                    .spawn(Node {
                        flex_direction: FlexDirection::Row,
                        column_gap: Val::Px(12.0),
                        ..default()
                    })
                    .with_children(|row| {
                        action(row, "Log in", LoginButton::Login);
                        action(row, "Sign up", LoginButton::Signup);
                    });
                panel.spawn((
                    Text::new("click a field, then type"),
                    TextFont { font_size: 14.0, ..default() },
                    TextColor(Color::srgb(0.8, 0.7, 0.5)),
                    StatusText,
                ));
            });
        });
}

fn field(parent: &mut ChildSpawnerCommands, label: &str, kind: LoginButton, marker: impl Component) {
    parent
        .spawn((Node { flex_direction: FlexDirection::Row, align_items: AlignItems::Center, column_gap: Val::Px(8.0), ..default() }))
        .with_children(|row| {
            row.spawn((
                Text::new(format!("{label}:")),
                TextFont { font_size: 18.0, ..default() },
                TextColor(Color::WHITE),
            ));
            row.spawn((
                Button,
                kind,
                Node { width: Val::Px(180.0), padding: UiRect::axes(Val::Px(8.0), Val::Px(5.0)), ..default() },
                BackgroundColor(FIELD_BG),
            ))
            .with_children(|f| {
                f.spawn((
                    Text::new(""),
                    TextFont { font_size: 18.0, ..default() },
                    TextColor(Color::WHITE),
                    marker,
                ));
            });
        });
}

fn action(parent: &mut ChildSpawnerCommands, label: &str, kind: LoginButton) {
    parent
        .spawn((
            Button,
            kind,
            Node { padding: UiRect::axes(Val::Px(16.0), Val::Px(9.0)), ..default() },
            BackgroundColor(BTN_BG),
        ))
        .with_children(|b| {
            b.spawn((
                Text::new(label),
                TextFont { font_size: 18.0, ..default() },
                TextColor(Color::WHITE),
            ));
        });
}

/// Type into the focused field; Tab switches fields, Enter logs in.
pub fn login_keyboard(
    mut events: MessageReader<KeyboardInput>,
    mut login: ResMut<LoginState>,
    net: Res<NetClient>,
) {
    if login.done {
        return;
    }
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        match &ev.logical_key {
            Key::Tab => {
                login.focus = match login.focus {
                    Field::Name => Field::Password,
                    Field::Password => Field::Name,
                };
            }
            Key::Enter => submit(&mut login, &net, false),
            Key::Backspace => {
                match login.focus {
                    Field::Name => login.name.pop(),
                    Field::Password => login.password.pop(),
                };
            }
            Key::Character(s) => {
                let s = s.clone();
                match login.focus {
                    Field::Name => login.name.push_str(&s),
                    Field::Password => login.password.push_str(&s),
                }
            }
            _ => {}
        }
    }
}

/// Field focus + Log in / Sign up button clicks.
pub fn login_buttons(
    buttons: Query<(&Interaction, &LoginButton), (Changed<Interaction>, With<Button>)>,
    mut login: ResMut<LoginState>,
    net: Res<NetClient>,
) {
    for (interaction, btn) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match btn {
            LoginButton::FocusName => login.focus = Field::Name,
            LoginButton::FocusPassword => login.focus = Field::Password,
            LoginButton::Login => submit(&mut login, &net, false),
            LoginButton::Signup => submit(&mut login, &net, true),
        }
    }
}

fn submit(login: &mut LoginState, net: &NetClient, signup: bool) {
    if login.name.trim().is_empty() {
        login.status = "enter a username".into();
        return;
    }
    login.status = if signup { "signing up…".into() } else { "logging in…".into() };
    net.send(ClientMsg::Login {
        name: login.name.clone(),
        password: login.password.clone(),
        signup,
    });
}

/// Reflect the form state into the field/status text (password masked).
pub fn update_login_text(
    login: Res<LoginState>,
    mut name: Query<&mut Text, (With<NameText>, Without<PasswordText>, Without<StatusText>)>,
    mut pass: Query<&mut Text, (With<PasswordText>, Without<StatusText>)>,
    mut status: Query<&mut Text, With<StatusText>>,
) {
    let cursor = |focused: bool| if focused { "_" } else { "" };
    if let Ok(mut t) = name.single_mut() {
        t.0 = format!("{}{}", login.name, cursor(login.focus == Field::Name));
    }
    if let Ok(mut t) = pass.single_mut() {
        t.0 = format!("{}{}", "*".repeat(login.password.len()), cursor(login.focus == Field::Password));
    }
    if let Ok(mut t) = status.single_mut() {
        t.0 = login.status.clone();
    }
}

/// When login completes, tear down the screen and grab the cursor for play.
pub fn finish_login(
    login: Res<LoginState>,
    screen: Query<Entity, With<LoginScreen>>,
    mut cursor: Query<&mut CursorOptions, With<PrimaryWindow>>,
    mut commands: Commands,
    mut was_done: Local<bool>,
) {
    if login.done && !*was_done {
        for e in &screen {
            commands.entity(e).despawn();
        }
        if let Ok(mut c) = cursor.single_mut() {
            c.grab_mode = CursorGrabMode::Locked;
            c.visible = false;
        }
    }
    *was_done = login.done;
}
