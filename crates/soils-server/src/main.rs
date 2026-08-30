//! Dedicated-server binary: a thin wrapper over the `soils-server` library.
//! Override the bind address with `SOILS_BIND` (e.g. `127.0.0.1:9001`) and the
//! discovery name with `SOILS_NAME`.

#[tokio::main]
async fn main() {
    let mut config = soils_server::ServerConfig::default();
    if let Ok(bind) = std::env::var("SOILS_BIND") {
        config.bind = bind;
    }
    if let Ok(name) = std::env::var("SOILS_NAME") {
        config.name = name;
    }
    if let Ok(n) = std::env::var("SOILS_CRITTERS") {
        config.critters = n.parse().unwrap_or(0);
    }
    config.physics = std::env::var("SOILS_PHYSICS").is_ok_and(|v| v != "0");
    // A prop pile implies physics: asking for props with physics off would
    // silently drop nothing.
    if let Some(n) = std::env::var("SOILS_PROPS").ok().and_then(|v| v.parse().ok()) {
        config.props = n;
        if n > 0 {
            config.physics = true;
        }
    }
    // The lighting demo's room, for looking at by hand. Off by default, so
    // ordinary worlds are untouched terrain.
    if std::env::var("SOILS_CHAMBER").is_ok_and(|v| v != "0") {
        config.chamber = Some(soils_server::Chamber::DEMO);
    }

    // Scripting: SOILS_SCRIPTS_DIR wins; else SOILS_SCRIPTS=1 loads ./scripts.
    if let Ok(dir) = std::env::var("SOILS_SCRIPTS_DIR") {
        config.scripts_dir = Some(dir.into());
    } else if std::env::var("SOILS_SCRIPTS").is_ok_and(|v| v != "0") {
        config.scripts_dir = Some("scripts".into());
    }
    soils_server::run(config).await.expect("server failed");
}
