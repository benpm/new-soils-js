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
    // Scripting: SOILS_SCRIPTS_DIR wins; else SOILS_SCRIPTS=1 loads ./scripts.
    if let Ok(dir) = std::env::var("SOILS_SCRIPTS_DIR") {
        config.scripts_dir = Some(dir.into());
    } else if std::env::var("SOILS_SCRIPTS").is_ok_and(|v| v != "0") {
        config.scripts_dir = Some("scripts".into());
    }
    soils_server::run(config).await.expect("server failed");
}
