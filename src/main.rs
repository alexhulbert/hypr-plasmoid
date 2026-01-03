use hyprland::{
    data::{Clients, CursorPosition, Monitors},
    dispatch::*,
    event_listener::EventListener,
    keyword::Keyword,
    shared::*,
};
use serde::Deserialize;
use std::{
    collections::HashMap,
    env, fs,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use zbus::{Connection, proxy};

const PADDING: i64 = 20;

#[derive(Deserialize, Clone)]
struct Plasmoid {
    title: String,
    plasmoid: String,
    width: u32,
    height: u32,
}

type Config = HashMap<String, Plasmoid>;

#[proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait Watcher {
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;
}

#[proxy(interface = "org.kde.StatusNotifierItem")]
trait Sni {
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;
}

fn load_config() -> Config {
    let path = format!("{}/.config/hypr/plasmoids.json", env::var("HOME").unwrap());
    serde_json::from_str(&fs::read_to_string(&path).expect("config not found"))
        .expect("invalid json")
}

fn title_rule(title: &str) -> String {
    format!("title:^({title})$")
}

fn is_visible(title: &str) -> bool {
    Clients::get()
        .ok()
        .is_some_and(|c| c.iter().any(|w| w.title == title))
}

fn set_focus_mode(show: bool) {
    Keyword::set("input:follow_mouse", if show { "2" } else { "1" }).ok();
    Keyword::set(
        "input:float_switch_override_focus",
        if show { "0" } else { "1" },
    )
    .ok();
    if show {
        Keyword::set("cursor:no_warps", "1").ok();
    }
}

fn set_window_rules(p: &Plasmoid) {
    let Ok(cursor) = CursorPosition::get() else {
        return;
    };
    let Ok(monitors) = Monitors::get() else {
        return;
    };
    let Some(mon) = monitors.iter().find(|m| m.focused) else {
        return;
    };

    let mon_x = mon.x as i64;
    let mon_width = (mon.width as f64 / mon.scale as f64) as i64;
    let x = (cursor.x - PADDING).clamp(
        mon_x + PADDING,
        mon_x + mon_width - p.width as i64 - PADDING,
    );
    let y = (cursor.y - PADDING).max(mon.y as i64 + mon.reserved.1 as i64 + PADDING);

    let rule = title_rule(&p.title);
    Keyword::set("windowrule", format!("float,{rule}")).ok();
    Keyword::set(
        "windowrule",
        format!("size {} {},{rule}", p.width, p.height),
    )
    .ok();
    Keyword::set("windowrule", format!("move {x} {y},{rule}")).ok();
}

fn spawn_plasmoid(p: &Plasmoid) {
    Command::new("plasmawindowed")
        .args(["--statusnotifier", &p.plasmoid])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}

async fn find_sni(conn: &Connection, plasmoid: &str) -> Option<(String, String)> {
    let watcher = WatcherProxy::new(conn).await.ok()?;
    let suffix = format!("plasmawindowed_{plasmoid}");
    for item in watcher.registered_status_notifier_items().await.ok()? {
        let (dest, path) = item.split_once('/')?;
        let sni = SniProxy::builder(conn)
            .destination(dest)
            .ok()?
            .path(format!("/{path}"))
            .ok()?
            .build()
            .await
            .ok()?;
        if sni.id().await.ok()?.ends_with(&suffix) {
            return Some((dest.into(), format!("/{path}")));
        }
    }
    None
}

async fn wait_for_window(title: &str, timeout_ms: u64) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed().as_millis() < timeout_ms as u128 {
        if is_visible(title) {
            return true;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
    false
}

fn hide(title: &str) {
    if is_visible(title) {
        Dispatch::call(DispatchType::CloseWindow(WindowIdentifier::Title(title))).ok();
    }
}

fn hide_all(cfg: &Config, except: Option<&str>) {
    for (name, p) in cfg {
        if Some(name.as_str()) != except {
            hide(&p.title);
        }
    }
    if except.is_none() {
        set_focus_mode(false);
    }
}

fn nudge_cursor() {
    if let Ok(c) = CursorPosition::get() {
        Dispatch::call(DispatchType::Custom(
            "movecursor",
            &format!("{} {}", c.x + 1, c.y),
        ))
        .ok();
        Dispatch::call(DispatchType::Custom(
            "movecursor",
            &format!("{} {}", c.x, c.y),
        ))
        .ok();
    }
}

async fn show(conn: &Connection, cfg: &Config, name: &str) -> zbus::Result<()> {
    let p = cfg.get(name).expect("unknown plasmoid");

    if is_visible(&p.title) {
        Dispatch::call(DispatchType::FocusWindow(WindowIdentifier::Title(&p.title))).ok();
        hide_all(cfg, Some(name));
        return Ok(());
    }

    set_focus_mode(true);
    hide_all(cfg, Some(name));
    set_window_rules(p);

    if let Some((dest, path)) = find_sni(conn, &p.plasmoid).await {
        SniProxy::builder(conn)
            .destination(dest.as_str())?
            .path(path.as_str())?
            .build()
            .await?
            .activate(0, 0)
            .await?;
    } else {
        spawn_plasmoid(p);
    }

    if wait_for_window(&p.title, 500).await {
        Dispatch::call(DispatchType::FocusWindow(WindowIdentifier::Title(&p.title))).ok();
    }
    Ok(())
}

async fn toggle(conn: &Connection, cfg: &Config, name: &str) -> zbus::Result<()> {
    let p = cfg.get(name).expect("unknown plasmoid");
    if is_visible(&p.title) {
        hide(&p.title);
        set_focus_mode(false);
    } else {
        show(conn, cfg, name).await?;
    }
    nudge_cursor();
    Ok(())
}

fn config_cmd(cfg: &Config, name: &str) {
    let p = cfg.get(name).expect("unknown plasmoid");
    Dispatch::call(DispatchType::Exec(&format!(
        "plasmawindowed --config {}",
        p.plasmoid
    )))
    .ok();
}

async fn warm_up(cfg: &Config) {
    for p in cfg.values() {
        let rule = title_rule(&p.title);
        Keyword::set("windowrule", format!("move -10000 -10000,{rule}")).ok();
        spawn_plasmoid(p);
        if wait_for_window(&p.title, 2000).await {
            Dispatch::call(DispatchType::CloseWindow(WindowIdentifier::Title(&p.title))).ok();
        }
        Keyword::set("windowrule", format!("unset,{rule}")).ok();
    }
}

async fn daemon(cfg: Arc<Config>) {
    warm_up(&cfg).await;

    let titles: Vec<_> = cfg.values().map(|p| p.title.clone()).collect();
    let active = Arc::new(AtomicBool::new(false));
    let mut listener = EventListener::new();

    let cfg2 = cfg.clone();
    listener.add_workspace_changed_handler(move |_| hide_all(&cfg2, None));

    let cfg3 = cfg.clone();
    let active2 = active.clone();
    listener.add_active_window_changed_handler(move |data| {
        if data.as_ref().is_some_and(|d| titles.contains(&d.title)) {
            active2.store(true, Ordering::Relaxed);
        } else if active2.swap(false, Ordering::Relaxed) {
            hide_all(&cfg3, None);
        }
    });

    listener
        .start_listener_async()
        .await
        .expect("Failed to start listener");
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> zbus::Result<()> {
    let cfg = Arc::new(load_config());
    let conn = Connection::session().await?;
    let args: Vec<_> = env::args().skip(1).collect();
    let name = args.get(1).map(|s| s.as_str());

    match args.first().map(|s| s.as_str()) {
        Some("toggle") => toggle(&conn, &cfg, name.expect("missing plasmoid name")).await?,
        Some("config") => config_cmd(&cfg, name.expect("missing plasmoid name")),
        Some("hide-all") => hide_all(&cfg, None),
        Some("daemon") => daemon(cfg).await,
        _ => eprintln!("usage: hypr-plasmoid <toggle|config|hide-all|daemon> [name]"),
    }
    Ok(())
}
