use crate::{api, dom, model as m, session, systems};
use serde_json::{Value as V, json};

pub fn initialize() {}

pub fn registered(projects: V) {
    let old = dom::value("project");
    m::write(|s| s.boot["projects"] = projects);
    session::populate();
    if !m::project(&old).is_null() {
        session::sync_project(&old);
    }
}

pub async fn folder_open(purpose: &str) {
    folder_open_at(purpose, "", "Choose a project folder").await;
}

pub async fn folder_open_at(purpose: &str, start: &str, title: &str) {
    m::set("folder-purpose", json!(purpose));
    dom::text("folder-picker-title", title);
    dom::error("folder-picker-error", "");
    dom::dialog("folder-picker", true);
    if !folder(start, false).await && !start.is_empty() {
        dom::error("folder-picker-error", "");
        folder("", false).await;
    }
}

async fn folder(path: &str, query: bool) -> bool {
    let mut body = json!({
        "query": if query {
            dom::value("folder-picker-query").trim().to_owned()
        } else {
            String::new()
        },
        "hidden": dom::checked("folder-picker-hidden")
    });
    if !path.is_empty() {
        body["path"] = json!(path);
    }
    let epoch = m::next("folder");
    let typed = dom::value("folder-picker-location");
    dom::text("folder-picker-status", "Reading folders…");
    match api::post("/api/projects/browse", body).await {
        Ok(v) if m::current("folder", epoch) => {
            m::set("folder", v.clone());
            if dom::value("folder-picker-location") == typed {
                dom::set_value("folder-picker-location", &m::text(&v, "path"));
            }
            dom::text("folder-picker-current", &m::text(&v, "path"));
            dom::clear("folder-picker-list");
            for entry in m::array(&v["entries"]) {
                dom::append(
                    "folder-picker-list",
                    &dom::button(&m::summary(&entry), "folder-enter", entry["path"].clone()),
                );
            }
            dom::text(
                "folder-picker-status",
                &format!(
                    "{} folders{}",
                    m::array(&v["entries"]).len(),
                    if m::yes(&v["truncated"]) {
                        " · listing limited"
                    } else {
                        ""
                    }
                ),
            );
            dom::disabled("folder-picker-use", v["valid"] == false);
            dom::disabled("folder-picker-parent", v["parent"].is_null());
            true
        }
        Ok(_) => false,
        Err(e) => {
            dom::error("folder-picker-error", &e.message);
            false
        }
    }
}

async fn folder_use() {
    let data = m::data("folder");
    let path = m::text(&data, "path");
    if path.is_empty() || data["valid"] == false {
        return;
    }
    match api::post("/api/projects", json!({"path": path})).await {
        Ok(v) => {
            registered(v["projects"].clone());
            let project = m::text(&v["project"], "id");
            let purpose = m::text(
                &m::data("folder-purpose")
                    .as_str()
                    .map(|s| json!({"v": s}))
                    .unwrap_or(V::Null),
                "v",
            );
            dom::dialog("folder-picker", false);
            if purpose.starts_with("service:") {
                systems::add_service(
                    &project,
                    purpose
                        .strip_prefix("service:")
                        .and_then(|s| s.parse().ok()),
                )
                .await;
            } else {
                session::new_session(Some(&project));
                if purpose == "system-project" {
                    systems::editor_open().await;
                } else {
                    session::load_git().await;
                }
            }
        }
        Err(e) => dom::error("folder-picker-error", &e.message),
    }
}

pub async fn event(kind: &str, id: &str, action: &str, payload: &V) -> bool {
    if kind == "submit" {
        match id {
            "folder-picker-location-form" => {
                folder(&dom::value("folder-picker-location"), false).await;
                return true;
            }
            "folder-picker-search-form" => {
                folder(&m::text(&m::data("folder"), "path"), true).await;
                return true;
            }
            _ => return false,
        }
    }
    if kind != "click" {
        return false;
    }
    match action {
        "project-choose-folder" | "welcome-choose-folder" => folder_open("session").await,
        "folder-enter" => {
            folder(payload.as_str().unwrap_or(""), false).await;
        }
        "folder-picker-home" => {
            folder("", false).await;
        }
        "folder-picker-parent" => {
            folder(&m::text(&m::data("folder"), "parent"), false).await;
        }
        "folder-picker-clear" => {
            dom::set_value("folder-picker-query", "");
            folder(&m::text(&m::data("folder"), "path"), false).await;
        }
        "folder-picker-close" => {
            m::next("folder");
            dom::dialog("folder-picker", false);
        }
        "folder-picker-use" => folder_use().await,
        _ => return false,
    }
    true
}
