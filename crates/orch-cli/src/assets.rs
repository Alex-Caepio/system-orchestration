use orch_harness::http::Asset;
use std::collections::BTreeMap;

pub fn embedded_assets() -> BTreeMap<String, Asset> {
    let mut map = BTreeMap::new();
    map.insert(
        "/index.html".into(),
        Asset::from_bytes(
            include_bytes!(concat!(env!("ORCH_WEB_DIR"), "/index.html")).to_vec(),
            "text/html; charset=utf-8",
        ),
    );
    map.insert(
        "/app.css".into(),
        Asset::from_bytes(
            include_bytes!(concat!(env!("ORCH_WEB_DIR"), "/app.css")).to_vec(),
            "text/css; charset=utf-8",
        ),
    );
    map.insert(
        "/boot.js".into(),
        Asset::from_bytes(
            include_bytes!(concat!(env!("ORCH_WEB_DIR"), "/boot.js")).to_vec(),
            "text/javascript; charset=utf-8",
        ),
    );
    map.insert(
        "/icons/logo.svg".into(),
        Asset::from_bytes(
            include_bytes!(concat!(env!("ORCH_WEB_DIR"), "/icons/logo.svg")).to_vec(),
            "image/svg+xml",
        ),
    );
    map.insert(
        "/assets/orch_ui.js".into(),
        Asset::from_bytes(
            include_bytes!(concat!(env!("ORCH_WEB_DIR"), "/assets/orch_ui.js")).to_vec(),
            "text/javascript; charset=utf-8",
        ),
    );
    map.insert(
        "/assets/orch_ui_bg.wasm".into(),
        Asset::from_bytes(
            include_bytes!(concat!(env!("ORCH_WEB_DIR"), "/assets/orch_ui_bg.wasm")).to_vec(),
            "application/wasm",
        ),
    );
    map.insert(
        "/assets/abi-manifest.json".into(),
        Asset::from_bytes(
            include_bytes!(concat!(env!("ORCH_WEB_DIR"), "/assets/abi-manifest.json")).to_vec(),
            "application/json",
        ),
    );
    map
}
